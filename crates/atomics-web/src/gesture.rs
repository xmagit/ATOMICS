//! Pointer gestures over a terminal. The `<textarea>` over the terminal (the decided input path, design
//! §4.4) receives every touch and click there, so the canvas never sees them; the page forwards its
//! pointer events here. Pure: events and a clock in, gestures out.
//!
//! A touch: a tap, a one-finger drag (history and panning), a long press that selects a word and drags
//! the selection out, or two fingers pinching (the font size). The terminal pane drags a selection's
//! handles itself. A mouse: a click, or a drag that selects, as
//! on the desktop.
//!
//! A pointer whose lift never arrives would stay down and make every later touch a pinch. The page
//! cancels any pointer still down when a primary one (a new touch's first finger, or a mouse) goes down,
//! and the terminal pane starts a new recognizer when it drops the overlay's input.

use std::collections::BTreeMap;

use egui::{Pos2, Vec2};

/// How far a press may move and still be a tap or a long press, in points.
pub const SLOP: f32 = 8.0;
/// How long a still touch takes to become a long press.
pub const LONG_PRESS_MS: f64 = 450.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Mouse,
    Touch,
    Pen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Down,
    Move,
    Up,
    Cancel,
}

/// One pointer event, in the canvas's points.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pointer {
    pub id: i32,
    pub phase: Phase,
    pub pos: Pos2,
    pub kind: Kind,
    pub time_ms: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Gesture {
    /// A short press that did not move.
    Tap(Pos2),
    /// One finger moved by this much since the last drag.
    Drag(Vec2),
    /// Two fingers: the font size times `factor`, about `center`.
    Pinch { factor: f32, center: Pos2 },
    /// A selection from `from` to `to`; `done` once the pointer lifted. `word`: a long press started it,
    /// so it takes the word under `from` whole; a mouse's drag selects cell by cell.
    Select { from: Pos2, to: Pos2, done: bool, word: bool },
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    Idle,
    Pressed {
        id: i32,
        start: Pos2,
        since: f64,
        kind: Kind,
    },
    Dragging {
        id: i32,
        last: Pos2,
    },
    Selecting {
        id: i32,
        from: Pos2,
        to: Pos2,
        word: bool,
    },
    Pinching {
        distance: f32,
    },
    /// After a pinch, until every finger is up.
    Done,
}

#[derive(Debug, Clone)]
pub struct Recognizer {
    mode: Mode,
    down: BTreeMap<i32, Pos2>,
}

impl Default for Recognizer {
    fn default() -> Self {
        Self { mode: Mode::Idle, down: BTreeMap::new() }
    }
}

impl Recognizer {
    /// The two fingers of a pinch: their distance and centre.
    fn pair(&self) -> Option<(f32, Pos2)> {
        let mut points = self.down.values();
        let (a, b) = (*points.next()?, *points.next()?);
        Some((a.distance(b), a + (b - a) / 2.0))
    }

    pub fn event(&mut self, e: &Pointer) -> Vec<Gesture> {
        let mut out = Vec::new();
        match e.phase {
            Phase::Down => {
                self.down.insert(e.id, e.pos);
                if self.down.len() == 2 {
                    let distance = self.pair().map_or(1.0, |(d, _)| d.max(1.0));
                    self.mode = Mode::Pinching { distance };
                } else if self.down.len() == 1 {
                    self.mode = Mode::Pressed { id: e.id, start: e.pos, since: e.time_ms, kind: e.kind };
                }
            }
            Phase::Move => {
                if self.down.contains_key(&e.id) {
                    self.down.insert(e.id, e.pos);
                }
                match self.mode {
                    Mode::Pressed { id, start, kind, .. } if id == e.id && e.pos.distance(start) > SLOP => {
                        if kind == Kind::Mouse {
                            self.mode = Mode::Selecting { id, from: start, to: e.pos, word: false };
                            out.push(Gesture::Select { from: start, to: e.pos, done: false, word: false });
                        } else {
                            self.mode = Mode::Dragging { id, last: e.pos };
                            out.push(Gesture::Drag(e.pos - start));
                        }
                    }
                    Mode::Dragging { id, last } if id == e.id => {
                        self.mode = Mode::Dragging { id, last: e.pos };
                        out.push(Gesture::Drag(e.pos - last));
                    }
                    Mode::Selecting { id, from, word, .. } if id == e.id => {
                        self.mode = Mode::Selecting { id, from, to: e.pos, word };
                        out.push(Gesture::Select { from, to: e.pos, done: false, word });
                    }
                    Mode::Pinching { distance } => {
                        if let Some((now, center)) = self.pair() {
                            let now = now.max(1.0);
                            out.push(Gesture::Pinch { factor: now / distance, center });
                            self.mode = Mode::Pinching { distance: now };
                        }
                    }
                    _ => {}
                }
            }
            Phase::Up | Phase::Cancel => {
                self.down.remove(&e.id);
                let lifted = e.phase == Phase::Up;
                match self.mode {
                    Mode::Pressed { id, start, .. } if id == e.id => {
                        if lifted {
                            out.push(Gesture::Tap(start));
                        }
                        self.mode = Mode::Idle;
                    }
                    Mode::Dragging { id, .. } if id == e.id => self.mode = Mode::Idle,
                    Mode::Selecting { id, from, to, word } if id == e.id => {
                        let to = if lifted { e.pos } else { to };
                        out.push(Gesture::Select { from, to, done: true, word });
                        self.mode = Mode::Idle;
                    }
                    Mode::Pinching { .. } | Mode::Done => {
                        self.mode = if self.down.is_empty() { Mode::Idle } else { Mode::Done };
                    }
                    _ => {}
                }
                if self.down.is_empty() && !matches!(self.mode, Mode::Idle) {
                    self.mode = Mode::Idle;
                }
            }
        }
        out
    }

    /// The clock moved: a touch held still long enough selects the word where it is.
    pub fn tick(&mut self, now_ms: f64) -> Vec<Gesture> {
        match self.mode {
            Mode::Pressed { id, start, since, kind } if kind != Kind::Mouse && now_ms - since >= LONG_PRESS_MS => {
                self.mode = Mode::Selecting { id, from: start, to: start, word: true };
                vec![Gesture::Select { from: start, to: start, done: false, word: true }]
            }
            _ => Vec::new(),
        }
    }

    /// When a long press would start, while a touch is held still.
    pub fn due_ms(&self) -> Option<f64> {
        match self.mode {
            Mode::Pressed { since, kind, .. } if kind != Kind::Mouse => Some(since + LONG_PRESS_MS),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{pos2, vec2};

    fn at(id: i32, phase: Phase, x: f32, y: f32, kind: Kind, time_ms: f64) -> Pointer {
        Pointer { id, phase, pos: pos2(x, y), kind, time_ms }
    }

    fn run(r: &mut Recognizer, events: &[Pointer]) -> Vec<Gesture> {
        events.iter().flat_map(|e| r.event(e)).collect()
    }

    #[test]
    fn a_touch_taps_drags_or_long_presses() {
        let (t, mut r) = (Kind::Touch, Recognizer::default());
        let tap = run(
            &mut r,
            &[
                at(1, Phase::Down, 10.0, 10.0, t, 0.0),
                at(1, Phase::Move, 13.0, 12.0, t, 50.0),
                at(1, Phase::Up, 13.0, 12.0, t, 90.0),
            ],
        );
        assert_eq!(tap, [Gesture::Tap(pos2(10.0, 10.0))], "a little movement is still a tap");

        let drag = run(
            &mut r,
            &[
                at(2, Phase::Down, 10.0, 100.0, t, 0.0),
                at(2, Phase::Move, 10.0, 130.0, t, 30.0),
                at(2, Phase::Move, 4.0, 150.0, t, 60.0),
                at(2, Phase::Up, 4.0, 150.0, t, 90.0),
            ],
        );
        assert_eq!(drag, [Gesture::Drag(vec2(0.0, 30.0)), Gesture::Drag(vec2(-6.0, 20.0))]);

        assert!(run(&mut r, &[at(3, Phase::Down, 50.0, 50.0, t, 1000.0)]).is_empty());
        assert_eq!(r.due_ms(), Some(1000.0 + LONG_PRESS_MS));
        assert!(r.tick(1200.0).is_empty(), "not yet");
        let word = |to: Pos2, done| Gesture::Select { from: pos2(50.0, 50.0), to, done, word: true };
        assert_eq!(r.tick(1460.0), [word(pos2(50.0, 50.0), false)]);
        let selected =
            run(&mut r, &[at(3, Phase::Move, 90.0, 66.0, t, 1500.0), at(3, Phase::Up, 92.0, 66.0, t, 1600.0)]);
        assert_eq!(
            selected,
            [word(pos2(90.0, 66.0), false), word(pos2(92.0, 66.0), true)],
            "a long press selects a word and drags the selection out, then lifts"
        );
        assert_eq!(r.due_ms(), None);
    }

    #[test]
    fn two_fingers_pinch_and_nothing_else() {
        let (t, mut r) = (Kind::Touch, Recognizer::default());
        let pinch = run(
            &mut r,
            &[
                at(1, Phase::Down, 100.0, 100.0, t, 0.0),
                at(2, Phase::Down, 200.0, 100.0, t, 10.0),
                at(2, Phase::Move, 300.0, 100.0, t, 40.0),
                at(1, Phase::Up, 100.0, 100.0, t, 60.0),
                at(2, Phase::Move, 320.0, 100.0, t, 70.0),
                at(2, Phase::Up, 320.0, 100.0, t, 80.0),
            ],
        );
        assert_eq!(pinch, [Gesture::Pinch { factor: 2.0, center: pos2(200.0, 100.0) }], "no tap or drag after a pinch");
        assert!(r.tick(10_000.0).is_empty(), "and no long press");
    }

    #[test]
    fn a_pointer_cancelled_for_a_lost_lift_leaves_the_next_touch_a_tap() {
        let (t, mut r) = (Kind::Touch, Recognizer::default());
        // Finger 1's lift never came; the page cancels it as finger 2, a new primary, goes down.
        let events = [
            at(1, Phase::Down, 10.0, 10.0, t, 0.0),
            at(1, Phase::Cancel, 60.0, 60.0, t, 500.0),
            at(2, Phase::Down, 60.0, 60.0, t, 500.0),
            at(2, Phase::Up, 60.0, 60.0, t, 560.0),
        ];
        assert_eq!(run(&mut r, &events), [Gesture::Tap(pos2(60.0, 60.0))]);
        let drag = run(
            &mut r,
            &[
                at(3, Phase::Down, 0.0, 0.0, t, 600.0),
                at(3, Phase::Move, 0.0, 40.0, t, 620.0),
                at(3, Phase::Up, 0.0, 40.0, t, 640.0),
            ],
        );
        assert_eq!(drag, [Gesture::Drag(vec2(0.0, 40.0))], "and the touch after it");
    }

    #[test]
    fn a_mouse_clicks_or_drags_a_selection_and_never_long_presses() {
        let (m, mut r) = (Kind::Mouse, Recognizer::default());
        assert!(run(&mut r, &[at(1, Phase::Down, 10.0, 10.0, m, 0.0)]).is_empty());
        assert!(r.tick(5_000.0).is_empty() && r.due_ms().is_none());
        assert_eq!(run(&mut r, &[at(1, Phase::Up, 10.0, 10.0, m, 5_000.0)]), [Gesture::Tap(pos2(10.0, 10.0))]);
        let drag = run(
            &mut r,
            &[
                at(1, Phase::Down, 10.0, 10.0, m, 0.0),
                at(1, Phase::Move, 60.0, 10.0, m, 10.0),
                at(1, Phase::Cancel, 70.0, 10.0, m, 20.0),
            ],
        );
        assert_eq!(
            drag,
            [
                Gesture::Select { from: pos2(10.0, 10.0), to: pos2(60.0, 10.0), done: false, word: false },
                Gesture::Select { from: pos2(10.0, 10.0), to: pos2(60.0, 10.0), done: true, word: false },
            ],
            "a cancelled drag keeps where it got to"
        );
    }
}
