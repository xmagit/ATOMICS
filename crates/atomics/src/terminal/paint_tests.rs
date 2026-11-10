//! The shared painter draws exactly what the desktop painter drew before it (`legacy_paint`). Each
//! scene is fed to an emulator, painted both ways in fresh egui contexts, and must give the same
//! shapes and the same pixels, at 1x and 2x. `ATOMICS_TERMINAL_SCREENSHOTS=<dir>` writes each
//! scene's pixels as a PNG.

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Scroll;
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::Processor;
use egui::{Pos2, Rect, vec2};

use super::{Metrics, TerminalView, legacy_paint, painter};
use crate::test_raster::Raster;
use crate::theme::{Palette, Theme};

struct Scene {
    name: &'static str,
    cols: usize,
    rows: usize,
    output: String,
    /// Lines to scroll the view back into history.
    scroll: i32,
    /// From and to, in grid coordinates.
    selection: Option<(SelectionType, At, At)>,
}

/// A grid line, column and side.
type At = (i32, usize, Side);

impl Scene {
    fn new(name: &'static str, cols: usize, rows: usize, output: impl Into<String>) -> Self {
        Self { name, cols, rows, output: output.into(), scroll: 0, selection: None }
    }

    fn scrolled(self, scroll: i32) -> Self {
        Self { scroll, ..self }
    }

    fn selecting(self, kind: SelectionType, from: At, to: At) -> Self {
        Self { selection: Some((kind, from, to)), ..self }
    }

    fn term(&self) -> (Term<VoidListener>, Option<Selection>) {
        let config = Config { scrolling_history: 1000, ..Config::default() };
        let mut term = Term::new(config, &TermSize::new(self.cols, self.rows), VoidListener);
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, self.output.as_bytes());
        if self.scroll != 0 {
            term.scroll_display(Scroll::Delta(self.scroll));
        }
        let selection = self.selection.map(|(kind, (line, col, side), (to_line, to_col, to_side))| {
            let mut selection = Selection::new(kind, Point::new(Line(line), Column(col)), side);
            selection.update(Point::new(Line(to_line), Column(to_col)), to_side);
            selection
        });
        (term, selection)
    }
}

fn csi(body: &str) -> String {
    format!("\x1b[{body}")
}

/// Colours and attributes: the 16 colours as text and fills, the 256-colour cube and grays,
/// truecolor, every attribute, and attributes mixed with colours.
fn colours() -> String {
    let mut out = String::new();
    for i in 0..8 {
        out += &format!(
            "{}fg{i}{} {}bg{i}{} {}br{i}{} {}BR{i}{} ",
            csi(&format!("3{i}m")),
            csi("0m"),
            csi(&format!("4{i}m")),
            csi("0m"),
            csi(&format!("9{i}m")),
            csi("0m"),
            csi(&format!("10{i}m")),
            csi("0m")
        );
        if i % 2 == 1 {
            out += "\r\n";
        }
    }
    for n in (16..256).step_by(4) {
        out += &format!("{}{:02x}", csi(&format!("38;5;{n}m")), n % 256);
    }
    out += &csi("0m");
    out += "\r\n";
    for n in (16..256).step_by(3) {
        out += &format!("{} ", csi(&format!("48;5;{n}m")));
    }
    out += &csi("0m");
    out += "\r\n";
    for i in 0..60 {
        let (r, g, b) = (i * 4, 255 - i * 4, (i * 9) % 256);
        out += &format!("{}{}▀", csi(&format!("48;2;{r};{g};{b}m")), csi(&format!("38;2;{b};{r};{g}m")));
    }
    out += &csi("0m");
    out += "\r\n";
    for (code, label) in [
        ("1", "bold"),
        ("2", "dim"),
        ("3", "italic"),
        ("4", "under"),
        ("21", "double"),
        ("4:3", "curl"),
        ("4:4", "dotted"),
        ("4:5", "dashed"),
        ("7", "inverse"),
        ("8", "hidden"),
        ("9", "strike"),
        ("2;31", "dimred"),
        ("7;32", "invgreen"),
        ("7;2;33;44", "invdimyb"),
        ("8;41", "hiddenbg"),
        ("4;58;5;196", "ulcolour"),
        ("1;3;4;35", "bolditul"),
    ] {
        out += &format!("{}{label}{} ", csi(&format!("{code}m")), csi("0m"));
    }
    out += "\r\n";
    // A coloured background run with blanks, then text of several colours on one line.
    out += &format!("{}  padded  {}|", csi("44m"), csi("0m"));
    out += &format!("{}r{}g{}b{} same{} {}x", csi("31m"), csi("32m"), csi("34m"), csi("0m"), csi("0m"), csi("31m"));
    out += &csi("0m");
    out
}

/// A full-screen program on the alternate screen: frame, title, inverse status bar, panes with
/// their own backgrounds, a progress bar and a beam cursor.
fn tui(alt: bool) -> String {
    let (cols, rows) = (72, 20);
    let mut out = String::new();
    if alt {
        out += "shell output under the alternate screen\r\n";
        out += &csi("?1049h");
    }
    out += &csi("H");
    out += &csi("2J");
    out += &format!("{}┌{}┐{}", csi("36m"), "─".repeat(cols - 2), csi("0m"));
    for row in 2..rows - 1 {
        out += &format!("{}{}│{}│{}", csi(&format!("{row};1H")), csi("36m"), csi(&format!("{row};{cols}H")), csi("0m"));
    }
    out += &format!("{}{}└{}┘{}", csi(&format!("{};1H", rows - 1)), csi("36m"), "─".repeat(cols - 2), csi("0m"));
    out += &format!("{}{} htop-ish  {} ⠋ load 1.23 ", csi("1;3H"), csi("1;97;44m"), csi("0;44m"));
    out += &csi("0m");
    for (i, (name, pct)) in [("cpu0", 73), ("cpu1", 12), ("mem", 55), ("swp", 3)].iter().enumerate() {
        let bar = 30 * pct / 100;
        out += &format!(
            "{}{}{name:<5}{}[{}{}{}{}{}] {pct:>3}%",
            csi(&format!("{};3H", 3 + i)),
            csi("1m"),
            csi("0m"),
            csi("32m"),
            "▓".repeat(bar),
            csi("90m"),
            "░".repeat(30 - bar),
            csi("0m")
        );
    }
    for (i, line) in
        ["  PID USER      CPU%  COMMAND", "  101 alice     73.0  cargo test", "  202 root       0.3  launchd"]
            .iter()
            .enumerate()
    {
        let style = if i == 0 {
            "30;42m"
        } else if i == 1 {
            "7m"
        } else {
            "0m"
        };
        out += &format!("{}{}{line:<66}{}", csi(&format!("{};3H", 8 + i)), csi(style), csi("0m"));
    }
    out += &format!("{}{}{:<30}{}", csi("12;3H"), csi("48;5;236m"), " left pane", csi("0m"));
    out += &format!("{}{}{:<30}{}", csi("12;37H"), csi("48;2;40;0;60m"), " right pane 日本", csi("0m"));
    out += &format!("{}  →  ✓ ✗ ⚠ ● ○ ◆ ▶ ◀ λ ≥ ≠ ∞  ", csi("14;3H"));
    out += &format!(
        "{}{} F1 {}Help {} F10 {}Quit ",
        csi(&format!("{};1H", rows)),
        csi("30;46m"),
        csi("0m"),
        csi("30;46m"),
        csi("0m")
    );
    out += &format!("{}{}", csi("6 q"), csi("16;10H"));
    out
}

/// Wide characters, including at the end of a line (they wrap) and split from their spacer by an
/// erase, which the desktop draws in its own way.
fn wide() -> String {
    let mut out = String::new();
    out += "中文字符 日本語 한국어 ＡＢＣ a中b 🙂👍 ✅⌛\r\n";
    out += &format!(
        "{}红色{} {}蓝底{} {}下划线{}\r\n",
        csi("31m"),
        csi("0m"),
        csi("44m"),
        csi("0m"),
        csi("4m"),
        csi("0m")
    );
    // Fills the line so the last wide character wraps to the next one.
    out += &format!("{}中\r\n", "x".repeat(39));
    // Erase from the spacer of 文: 文 keeps its wide flag without a spacer.
    out += &format!("ab中文cd{}{}\r\n", csi("6G"), csi("K"));
    // Erase up to the first column: 中's spacer stays without its character.
    out += &format!("{}中ef{}{}{}\r\n", csi("43m"), csi("1G"), csi("1K"), csi("0m"));
    // Erase characters over a wide one, and insert into the middle of one.
    out += &format!("gh文字ij{}{}", csi("3G"), csi("1X"));
    out += &format!("\r\nkl文字{}{}", csi("4G"), csi("2@"));
    out
}

/// Combining characters over ASCII, non-ASCII and wide bases, and grapheme clusters.
fn combining() -> String {
    let mut out = String::new();
    out += "e\u{301} a\u{308} n\u{303} o\u{302}\u{323} | é ñ (precomposed)\r\n";
    out += "а\u{301} ж\u{306} ю\u{301} | क्षि | שָׁלוֹם\r\n";
    out += "中\u{301} 🙂\u{fe0f} 👨\u{200d}👩\u{200d}👧 🇸🇪 👍\u{1f3fd}\r\n";
    out += &format!("{}e\u{301}x{} {}ü\u{304}{} \u{301}space\r\n", csi("32m"), csi("0m"), csi("7m"), csi("0m"));
    out
}

fn numbered(lines: usize) -> String {
    (0..lines)
        .map(|i| format!("{}{i:04} line of history 中文{}\r\n", csi(&format!("3{}m", i % 7 + 1)), csi("0m")))
        .collect()
}

/// The program's own colours (OSC 4, 10, 11, 12): cells on the default background are filled once
/// the background no longer matches the theme's.
fn program_colours() -> String {
    let mut out = String::new();
    out += "\x1b]4;1;rgb:12/34/56\x07\x1b]4;200;rgb:ff/80/00\x07";
    out += &format!("{}red is now blue{} {}orange{}\r\n", csi("31m"), csi("0m"), csi("38;5;200m"), csi("0m"));
    out += "\x1b]10;rgb:aa/bb/cc\x07\x1b]11;rgb:20/10/30\x07\x1b]12;rgb:00/ff/00\x07";
    out += &format!("default colours {}inverse{} after\r\n", csi("7m"), csi("0m"));
    out
}

fn cursor(shape: u8) -> String {
    format!("$ ls{}{}  中x{}", csi(&format!("{shape} q")), "\r\n", csi("2;3H"))
}

fn scenes() -> Vec<Scene> {
    use SelectionType::{Block, Semantic, Simple};
    let mut scenes = vec![
        Scene::new("blank", 40, 6, ""),
        Scene::new("tiny", 2, 1, "ab"),
        Scene::new("colours", 80, 18, colours()),
        Scene::new("tui", 72, 20, tui(false)),
        Scene::new("alternate-screen", 72, 20, tui(true)),
        Scene::new("wide", 40, 10, wide()),
        Scene::new("combining", 50, 6, combining()),
        Scene::new("program-colours", 50, 4, program_colours()),
        Scene::new("wrapped", 20, 6, "a long line that wraps over several rows of a narrow terminal"),
        Scene::new("hidden-cursor", 30, 3, format!("$ quiet{}", csi("?25l"))),
        Scene::new("selection", 40, 10, wide()).selecting(Simple, (0, 3, Side::Right), (3, 5, Side::Left)),
        Scene::new("selection-reversed", 40, 10, combining()).selecting(
            Simple,
            (2, 9, Side::Left),
            (0, 2, Side::Right),
        ),
        Scene::new("selection-block", 80, 18, colours()).selecting(Block, (1, 4, Side::Left), (6, 20, Side::Right)),
        Scene::new("selection-word", 72, 20, tui(true)).selecting(Semantic, (8, 9, Side::Left), (9, 12, Side::Left)),
        Scene::new("scrollback", 40, 12, numbered(200)).scrolled(37),
        Scene::new("scrollback-top", 40, 12, numbered(200)).scrolled(1000),
        Scene::new("scrollback-selection", 40, 12, numbered(200)).scrolled(5).selecting(
            Simple,
            (-9, 4, Side::Left),
            (3, 10, Side::Right),
        ),
    ];
    for (name, shape) in [
        ("cursor-block", 2),
        ("cursor-blinking", 1),
        ("cursor-underline", 4),
        ("cursor-beam", 6),
        ("cursor-default", 0),
    ] {
        scenes.push(Scene::new(name, 20, 3, cursor(shape)));
    }
    scenes.push(Scene::new("cursor-on-wide", 20, 3, format!("中文{}", csi("1;2H"))));
    scenes
}

/// What one painter drew: its shapes and their pixels.
struct Frame {
    shapes: Vec<egui::epaint::ClippedShape>,
    image: egui::ColorImage,
}

/// Paints the scene into a fresh context at `ppp`, through the shared painter or the old one.
fn frame(scene: &Scene, palette: &Palette, ppp: f32, focused: bool, legacy: bool) -> Frame {
    let (term, selection) = scene.term();
    let ctx = egui::Context::default();
    ctx.set_pixels_per_point(ppp);
    let size = vec2(scene.cols as f32 * 8.0 + 30.0, scene.rows as f32 * 18.0 + 20.0);
    let screen = Rect::from_min_size(Pos2::ZERO, size);
    let input = egui::RawInput { screen_rect: Some(screen), ..Default::default() };
    let mut output = ctx.run_ui(input, |ui| {
        let font = palette.mono(13.0);
        let cell = ui.fonts_mut(|f| vec2(f.glyph_width(&font, 'M'), f.row_height(&font)));
        let metrics = Metrics { font, cell: vec2(cell.x, cell.y.ceil()) };
        let outer = Rect::from_min_size(Pos2::new(3.5, 2.0), size - vec2(4.0, 3.0));
        let inner = outer.shrink2(vec2(6.0, 4.0));
        let mut view =
            TerminalView { grid: (scene.cols, scene.rows), selection: selection.clone(), ..Default::default() };
        if legacy {
            let selection = view.selection.as_ref();
            legacy_paint::paint(&term, selection, scene.rows, ui, (outer, inner), &metrics, palette, focused);
        } else {
            let selection = view.copy_view(&term, palette, None);
            let theme = palette.terminal_theme();
            painter::paint(ui.painter(), (outer, inner), &view.screen, selection.as_ref(), &metrics, &theme, focused);
        }
    });
    let mut raster = Raster::default();
    raster.feed(&output);
    output.textures_delta.clear();
    let shapes = output.shapes.clone();
    let pixels = size * ppp;
    let image = raster.paint(&ctx, output, [pixels.x as usize, pixels.y as usize], palette.background);
    Frame { shapes, image }
}

#[test]
fn the_shared_painter_draws_what_the_desktop_drew() {
    let dir = std::env::var_os("ATOMICS_TERMINAL_SCREENSHOTS").map(std::path::PathBuf::from);
    let themes = ["claude", "paper"].map(|name| {
        let config = atomics_core::config::ThemeConfig { name: name.into(), ..Default::default() };
        (name, Theme::from_config(&config).palette())
    });
    let mut checked = 0;
    for scene in scenes() {
        for (theme, palette) in &themes {
            for ppp in [1.0, 2.0] {
                for focused in [true, false] {
                    let label = format!("{} {theme} {ppp}x focused={focused}", scene.name);
                    let new = frame(&scene, palette, ppp, focused, false);
                    let old = frame(&scene, palette, ppp, focused, true);
                    assert!(new.shapes.len() > 1, "{label}: nothing painted");
                    assert_eq!(new.shapes.len(), old.shapes.len(), "{label}: shape counts differ");
                    for (i, (a, b)) in new.shapes.iter().zip(&old.shapes).enumerate() {
                        assert_eq!(a, b, "{label}: shape {i} differs");
                    }
                    let (new, old) = (new.image, old.image);
                    let differ = new.pixels.iter().zip(&old.pixels).filter(|(a, b)| a != b).count();
                    assert_eq!((new.size, differ), (old.size, 0), "{label}: pixels differ");
                    if let Some(dir) = &dir
                        && *theme == "claude"
                        && ppp == 2.0
                        && focused
                    {
                        std::fs::create_dir_all(dir).unwrap();
                        crate::test_raster::save_png(&dir.join(format!("{}.png", scene.name)), &new).unwrap();
                    }
                    checked += 1;
                }
            }
        }
    }
    assert_eq!(checked, scenes().len() * 8);
}

#[test]
fn the_view_copy_follows_the_display_offset_and_hides_a_scrolled_out_cursor() {
    let scene = Scene::new("scrollback", 40, 4, numbered(30)).scrolled(10);
    let (term, _) = scene.term();
    let palette = Theme::from_config(&atomics_core::config::ThemeConfig::default()).palette();
    let mut view = TerminalView { grid: (40, 4), ..Default::default() };
    view.copy_view(&term, &palette, None);
    assert!(view.screen.lines[0].text().starts_with("0017"), "{:?}", view.screen.lines[0].text());
    assert_eq!(view.screen.cursor.shape, atomics_core::screen::CursorShape::Hidden);
    assert_eq!(view.screen.palette[257], {
        let c = palette.terminal;
        atomics_core::screen::Rgb([c.r(), c.g(), c.b()])
    });
    // A view with fewer rows than the emulator does not show a cursor below its last row.
    let scene = Scene::new("short view", 12, 4, "a\r\nb\r\nc\r\nd");
    let (term, _) = scene.term();
    let mut view = TerminalView { grid: (12, 2), ..Default::default() };
    view.copy_view(&term, &palette, None);
    assert_eq!(view.screen.cursor.shape, atomics_core::screen::CursorShape::Hidden);
}

#[test]
fn a_view_copies_again_after_output_scrolling_or_a_resize() {
    let palette = Theme::from_config(&atomics_core::config::ThemeConfig::default()).palette();
    let scene = Scene::new("copies", 40, 3, numbered(10));
    let (mut term, _) = scene.term();
    let mut view = TerminalView { grid: (40, 3), ..Default::default() };
    let top = |view: &TerminalView| view.screen.lines[0].text();
    view.copy_view(&term, &palette, Some(1));
    let first = top(&view);
    let mut parser: Processor = Processor::new();
    parser.advance(&mut term, b"\x1b[Hchanged");
    view.copy_view(&term, &palette, Some(1));
    assert_eq!(top(&view), first, "the same revision reuses the copy");
    view.copy_view(&term, &palette, Some(2));
    assert!(top(&view).starts_with("changed"), "{}", top(&view));
    term.scroll_display(Scroll::Delta(2));
    view.copy_view(&term, &palette, Some(2));
    assert!(top(&view).starts_with("0006"), "scrolling the view copies again: {}", top(&view));
    view.grid = (40, 2);
    view.copy_view(&term, &palette, Some(2));
    assert_eq!(view.screen.cursor.shape, atomics_core::screen::CursorShape::Hidden, "the view's rows changed");
    // The palette follows the theme on every frame, copied or not.
    let paper =
        Theme::from_config(&atomics_core::config::ThemeConfig { name: "paper".into(), ..Default::default() }).palette();
    view.copy_view(&term, &paper, Some(2));
    let c = paper.terminal;
    assert_eq!(view.screen.palette[257], atomics_core::screen::Rgb([c.r(), c.g(), c.b()]));
}

/// Through a real PTY: output moves the revision, so the next frame shows it.
#[test]
fn frames_show_new_pty_output() {
    let dir = crate::test_support::tempdir().unwrap();
    let theme = Theme::from_config(&atomics_core::config::ThemeConfig::default());
    let palette = theme.palette();
    let terminal = super::Terminal::spawn(
        crate::host::Wake::default(),
        theme.terminal_colors(),
        crate::test_support::program("cat"),
        vec![],
        dir.path(),
        std::collections::HashMap::new(),
        100,
    )
    .unwrap();
    let ctx = egui::Context::default();
    let mut view = TerminalView::default();
    let metrics = Metrics { font: egui::FontId::monospace(12.0), cell: vec2(8.0, 16.0) };
    let rect = Rect::from_min_size(Pos2::ZERO, vec2(400.0, 120.0));
    let frame = |view: &mut TerminalView| {
        let mut output = ctx.run_ui(egui::RawInput { screen_rect: Some(rect), ..Default::default() }, |ui| {
            let id = ui.scope_id().with("terminal");
            view.show(&terminal, ui, rect, id, &metrics, &palette);
        });
        output.textures_delta.clear();
        view.screen.text()
    };
    frame(&mut view);
    for word in ["first", "second", "third"] {
        terminal.write(format!("{word}\r").into_bytes());
        let deadline =
            std::time::Instant::now() + crate::test_support::scaled_timeout(std::time::Duration::from_secs(3));
        while !terminal.screen_text(24).contains(word) {
            assert!(std::time::Instant::now() < deadline, "cat did not echo {word}");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(frame(&mut view).contains(word), "the frame after {word} shows it");
    }
    terminal.shutdown();
}
