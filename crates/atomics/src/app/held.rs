//! The window never waits for the runtime. A frame takes the state within [`WAIT`] ([`crate::host::Host::turn`]); when
//! the runtime still has it, the frame is held: it shows the last picture again, and what the user did meanwhile
//! reaches the first frame that has the state, in order. Whatever the runtime does, the window's thread goes on
//! answering its compositor, and a click made while the runtime is busy is late rather than lost.
//!
//! A held frame runs an egui pass that draws no widgets, which egui would take for a window whose widgets are gone:
//! their focus, menus and half-made clicks would go with them. So a held pass registers the last frame's widgets again,
//! and egui's memory from before the hold is put back when it ends.
use std::collections::VecDeque;
use std::time::{Duration, Instant};

use egui::{Event, Id, Pos2, Rect};

/// How long a frame waits for the state: less than a frame at 60 Hz. A pass of the runtime is usually far shorter.
const WAIT: Duration = Duration::from_millis(8);
/// How long it waits once it already holds: the user's input keeps frames coming, and none of them may add up.
const WAIT_AGAIN: Duration = Duration::from_millis(1);
/// When a held frame looks for the state again.
const RETRY: Duration = Duration::from_millis(16);
/// When the window says that it waits; before that the pause is too short to mention.
const SAY_BUSY: Duration = Duration::from_millis(700);
/// How near a click on a window that says it waits has to be to the click before to be the same one, made again.
const SAME_CLICK: f32 = 6.0;

#[derive(Default)]
pub(super) struct Held {
    /// The last picture drawn with the state, and what it asked of the platform.
    painted: egui::layers::GraphicLayers,
    ime: Option<egui::output::IMEOutput>,
    cursor: egui::CursorIcon,
    hold: Option<Hold>,
    queue: Queue,
    dropped: Vec<egui::DroppedFileHandle>,
    /// How many of the queue's events the last frame was given, and whether an egui pass took them: a window that is
    /// not shown runs its logic without one, and its events come back.
    given: usize,
    taken: bool,
    time: Option<f64>,
}

/// What egui remembered when the hold began. A clone of its memory starts with empty caches; they are kept aside.
struct Hold {
    since: Instant,
    memory: egui::Memory,
    caches: egui::cache::CacheStorage,
}

impl Held {
    pub(super) fn wait(&self) -> Duration {
        if self.hold.is_some() { WAIT_AGAIN } else { WAIT }
    }

    /// Before egui sees a frame's input: `free` when the frame has the state.
    pub(super) fn input(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput, free: bool) {
        if !self.taken {
            for event in raw.events.drain(..self.given.min(raw.events.len())).rev() {
                self.queue.events.push_front(event);
            }
        }
        self.given = 0;
        self.taken = false;
        self.time = raw.time;
        if !free {
            let hold = self.hold.get_or_insert_with(|| Hold::begin(ctx));
            let again = hold.since.elapsed() >= SAY_BUSY;
            raw.events.drain(..).for_each(|event| self.queue.keep(event, again));
            self.dropped.append(&mut raw.dropped_files);
            return;
        }
        if let Some(hold) = self.hold.take() {
            hold.end(ctx);
        }
        if !self.dropped.is_empty() {
            raw.dropped_files.splice(0..0, self.dropped.drain(..));
        }
        if !self.queue.events.is_empty() {
            raw.events.drain(..).for_each(|event| self.queue.keep(event, false));
            raw.events = self.queue.next();
            self.given = raw.events.len();
            if !self.queue.events.is_empty() {
                ctx.request_repaint();
            }
        }
    }

    /// Whether the frame that [`Held::input`] prepared is an egui pass, rather than a hidden window's logic alone.
    pub(super) fn begin(&mut self, ctx: &egui::Context) -> bool {
        self.taken = ctx.input(|i| i.raw.time.is_some() && i.raw.time == self.time);
        self.taken
    }

    /// The window has been drawn with the state: this is what a held frame shows.
    pub(super) fn drawn(&mut self, ctx: &egui::Context) {
        ctx.graphics(|graphics| self.painted.clone_from(graphics));
        ctx.output(|output| (self.ime, self.cursor) = (output.ime, output.cursor_icon));
    }

    /// A frame without the state. `says` is what it is waiting for, shown at once; a hold says so after [`SAY_BUSY`].
    pub(super) fn paint(&self, ctx: &egui::Context, palette: &atomics_ui::theme::Palette, says: Option<&str>) {
        keep_widgets(ctx);
        ctx.graphics_mut(|graphics| graphics.clone_from(&self.painted));
        ctx.output_mut(|output| (output.ime, output.cursor_icon) = (self.ime, self.cursor));
        let busy = self.hold.as_ref().is_some_and(|hold| hold.since.elapsed() >= SAY_BUSY);
        if let Some(text) = says.or(busy.then_some("ATOMICS is catching up…")) {
            let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Debug, Id::NULL.with("held-note")));
            let galley = painter.layout_no_wrap(text.into(), egui::FontId::proportional(13.0), palette.foreground);
            let at = ctx.content_rect().center_bottom() - egui::vec2(0.0, 34.0);
            let rect = Rect::from_center_size(at, galley.size() + egui::vec2(24.0, 12.0));
            painter.rect_filled(rect, 8.0, palette.panel);
            painter.rect_stroke(rect, 8.0, egui::Stroke::new(1.0, palette.accent), egui::StrokeKind::Inside);
            painter.galley(rect.center() - galley.size() / 2.0, galley, palette.foreground);
        }
        ctx.request_repaint_after(RETRY);
    }
}

impl Hold {
    fn begin(ctx: &egui::Context) -> Self {
        ctx.memory_mut(|memory| Self {
            since: Instant::now(),
            memory: memory.clone(),
            caches: std::mem::take(&mut memory.caches),
        })
    }

    fn end(self, ctx: &egui::Context) {
        ctx.memory_mut(|memory| {
            *memory = self.memory;
            memory.caches = self.caches;
        });
    }
}

/// Registers the last pass's widgets in this one, so that egui still knows where the window's widgets are and which of
/// them has the focus.
fn keep_widgets(ctx: &egui::Context) {
    let widgets = ctx.viewport(|viewport| viewport.prev_pass.widgets.clone());
    let layers: Vec<_> = widgets.layer_ids().map(|layer| (layer, Id::NULL.with(("held-layer", layer)))).collect();
    for &(layer, id) in &layers {
        let mut ui = egui::Ui::new(ctx.clone(), id, egui::UiBuilder::new().layer_id(layer).max_rect(Rect::EVERYTHING));
        for widget in widgets.get_layer(layer).filter(|widget| widget.enabled) {
            if layers.iter().any(|(_, held)| *held == widget.id) {
                continue;
            }
            ui.set_clip_rect(widget.interact_rect);
            ui.interact(widget.rect, widget.id, widget.sense);
        }
    }
}

/// What the user did while no frame had the state.
#[derive(Default)]
struct Queue {
    events: VecDeque<Event>,
    /// The click the queue ends with, if nothing but pointer movement followed it.
    last_click: Option<(Pos2, egui::PointerButton)>,
    /// A button whose press was the last click made again: its release goes too.
    again: Option<egui::PointerButton>,
}

impl Queue {
    /// Adds an event. With `again`, a click made again on the same place counts as the one before it: that is someone
    /// pressing a button that does not answer, and ten of them would switch a toggle ten times.
    fn keep(&mut self, event: Event, again: bool) {
        match &event {
            Event::PointerMoved(_) => {
                if matches!(self.events.back(), Some(Event::PointerMoved(_))) {
                    self.events.pop_back();
                }
            }
            Event::MouseMoved(_) => {}
            Event::PointerButton { pos, button, pressed: true, .. } => {
                let same = |(at, held): (Pos2, egui::PointerButton)| held == *button && at.distance(*pos) <= SAME_CLICK;
                if again && self.last_click.is_some_and(same) {
                    self.again = Some(*button);
                    return;
                }
                self.last_click = None;
            }
            Event::PointerButton { pos, button, pressed: false, .. } => {
                if self.again == Some(*button) {
                    self.again = None;
                    return;
                }
                self.last_click = Some((*pos, *button));
            }
            _ => self.last_click = None,
        }
        self.events.push_back(event);
    }

    /// What one frame is given: up to a click, or what was typed before one. A frame sees one click of a kind, and the
    /// next may be on what this one opens; what was typed goes to where the keyboard was then.
    fn next(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        let mut other = false;
        while let Some(event) = self.events.front() {
            let (pressed, released) = match event {
                Event::PointerButton { pressed, .. } => (*pressed, !*pressed),
                _ => (false, false),
            };
            if pressed && other {
                break;
            }
            other |= !matches!(
                event,
                Event::PointerButton { .. } | Event::PointerMoved(_) | Event::MouseMoved(_) | Event::PointerGone
            );
            events.extend(self.events.pop_front());
            if released {
                break;
            }
        }
        if self.events.is_empty() {
            self.last_click = None;
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Modifiers, PointerButton, pos2, vec2};

    fn button(x: f32, pressed: bool) -> Event {
        Event::PointerButton { pos: pos2(x, 10.0), button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE }
    }

    fn click(x: f32) -> [Event; 3] {
        [Event::PointerMoved(pos2(x, 10.0)), button(x, true), button(x, false)]
    }

    fn queue(events: impl IntoIterator<Item = Event>, again: bool) -> Queue {
        let mut queue = Queue::default();
        events.into_iter().for_each(|event| queue.keep(event, again));
        queue
    }

    #[test]
    fn a_frame_is_given_one_click_and_what_was_typed_before_it() {
        let typed = [Event::Text("a".into()), Event::Text("b".into())];
        let mut queue = queue(click(10.0).into_iter().chain(typed.clone()).chain(click(90.0)), false);
        assert_eq!(queue.next(), click(10.0));
        // The move to the second click may come with the text: only its press may not.
        assert_eq!(queue.next(), [&typed[..], &click(90.0)[..1]].concat());
        assert_eq!(queue.next(), click(90.0)[1..]);
        assert!(queue.next().is_empty());
    }

    #[test]
    fn a_click_made_again_on_a_window_that_says_it_waits_counts_once() {
        let ten = || (0..10).flat_map(|_| click(10.0));
        assert_eq!(queue(ten(), true).events.iter().filter(|e| matches!(e, Event::PointerButton { .. })).count(), 2);
        assert_eq!(queue(ten(), false).events.iter().filter(|e| matches!(e, Event::PointerButton { .. })).count(), 20);
        // Another place, or something done in between, is another click.
        let others = click(10.0).into_iter().chain(click(90.0)).chain([Event::Text("a".into())]).chain(click(90.0));
        assert_eq!(queue(others, true).events.iter().filter(|e| matches!(e, Event::PointerButton { .. })).count(), 6);
        // A press that is held, and a release that is not a repeated press's, both stay.
        let drag = [button(10.0, true), Event::PointerMoved(pos2(60.0, 10.0)), button(60.0, false)];
        assert_eq!(queue(drag.clone(), true).events, drag);
    }

    /// One frame of a window with a button that counts and a field, or a held frame.
    struct Window {
        ctx: egui::Context,
        held: Held,
        clock: f64,
        clicks: usize,
        text: String,
        rects: [Rect; 2],
    }

    impl Window {
        fn new() -> Self {
            let mut window = Self {
                ctx: egui::Context::default(),
                held: Held::default(),
                clock: 0.0,
                clicks: 0,
                text: String::new(),
                rects: [Rect::NOTHING; 2],
            };
            // egui sizes its widgets in the first frames.
            (0..3).for_each(|_| drop(window.frame(true, [])));
            window
        }

        fn frame(&mut self, free: bool, events: impl IntoIterator<Item = Event>) -> Vec<egui::epaint::ClippedShape> {
            self.clock += 0.016;
            let mut raw = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(400.0, 300.0))),
                time: Some(self.clock),
                events: events.into_iter().collect(),
                ..Default::default()
            };
            let ctx = self.ctx.clone();
            self.held.input(&ctx, &mut raw, free);
            let mut output = ctx.run_ui(raw, |ui| {
                assert!(self.held.begin(ui.ctx()));
                if !free {
                    self.held.paint(
                        ui.ctx(),
                        &atomics_ui::theme::Palette::new(&Default::default(), Default::default()),
                        None,
                    );
                    return;
                }
                let button = ui.button("Dock");
                self.clicks += usize::from(button.clicked());
                let field = ui.text_edit_singleline(&mut self.text);
                self.rects = [button.rect, field.rect];
                self.held.drawn(ui.ctx());
            });
            output.textures_delta.clear();
            output.shapes
        }

        fn click(&self, widget: usize) -> [Event; 3] {
            let pos = self.rects[widget].center();
            let button = |pressed| Event::PointerButton {
                pos,
                button: PointerButton::Primary,
                pressed,
                modifiers: Modifiers::NONE,
            };
            [Event::PointerMoved(pos), button(true), button(false)]
        }
    }

    #[test]
    fn a_held_frame_shows_the_last_picture_and_keeps_focus_and_clicks() {
        let mut window = Window::new();
        let [moved, pressed, released] = window.click(1);
        window.frame(true, [moved, pressed, released]);
        let focused = window.ctx.memory(|m| m.focused());
        assert!(focused.is_some(), "the field has the keyboard");
        window.frame(true, []);
        let picture = window.held.painted.clone();
        let shapes = window.frame(true, []).len();

        // A press before the hold, its release during it, and then text and a click on the button.
        let [moved, pressed, released] = window.click(1);
        window.frame(true, [moved, pressed]);
        for events in [vec![released], vec![Event::Text("held".into())], window.click(0).to_vec(), vec![]] {
            let held = window.frame(false, events);
            assert_eq!(held.len(), shapes, "the last picture, not an empty window");
        }
        assert_eq!((window.clicks, window.text.as_str()), (0, ""), "nothing happens without the state");
        drop(picture);

        for _ in 0..6 {
            window.frame(true, []);
        }
        assert_eq!((window.clicks, window.text.as_str()), (1, "held"), "all of it, once, where it was meant");
    }

    #[test]
    fn a_long_hold_changes_nothing_egui_remembers() {
        let mut window = Window::new();
        window.frame(true, window.click(1));
        let focused = window.ctx.memory(|m| m.focused());
        for _ in 0..40 {
            window.frame(false, []);
        }
        window.frame(true, [Event::Text("a".into())]);
        assert_eq!(window.ctx.memory(|m| m.focused()), focused, "the field still has the keyboard");
        assert_eq!(window.text, "a");
        window.frame(true, window.click(0));
        window.frame(true, []);
        assert_eq!(window.clicks, 1);
    }

    #[test]
    fn events_a_hidden_windows_logic_was_given_come_back_in_order() {
        let mut window = Window::new();
        let [first, second] = [window.click(1), window.click(0)];
        window.frame(false, first.into_iter().chain([Event::Text("a".into())]).chain(second));
        // The state is free, but the window is hidden: its logic runs without a pass, and eframe keeps the input.
        let ctx = window.ctx.clone();
        let mut raw = egui::RawInput { time: Some(window.clock + 1.0), ..Default::default() };
        window.held.input(&ctx, &mut raw, true);
        assert!(!raw.events.is_empty());
        assert!(!window.held.begin(&ctx));
        let given = raw.events;
        window.frame(true, given);
        for _ in 0..6 {
            window.frame(true, []);
        }
        assert_eq!((window.clicks, window.text.as_str()), (1, "a"));
    }
}
