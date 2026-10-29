# Themes

Every color and font in IMACS comes from a theme. Four themes are built in, theme files beside the config
can add more, and `[theme]` in the config picks one and can override any part of it. Saving a theme file
or the config applies the change while IMACS runs.

```toml
[theme]
name = "atomisk"          # a built-in theme, or themes/<name>.toml beside the config
accent = "#9fd6ae"        # optional: any token set here wins over the theme
```

`preset` is the old name of `name` and still works.

## Built-in themes

| Name | Look |
| --- | --- |
| `claude` (default) | Near-black with Claude Code's orange and diff colors; monospace UI. |
| `midnight` | The original blue-gray look. |
| `atomisk` | atomisk.ai: void `#08090a`, surface `#101113` panels, paper `#f1f2f3` text, the signal `#c5decf` accent, pale blue `#adc8dc` for info and sand `#e4d1a8` for warnings. Inter and IBM Plex Mono when available. |
| `paper` | Light: warm paper with a deeper orange that reads on white. |

The built-in themes are ordinary theme files that set every token. They are in
[crates/imacs-core/themes](../crates/imacs-core/themes), and make good starting points.

## Theme files

A theme file is `themes/<name>.toml` in the folder that holds `config.toml`, so normally
`~/.config/imacs/themes/<name>.toml` (with `IMACS_CONFIG`, the `themes/` folder beside that file). Names use
letters, digits, `-` and `_`. A file sets any tokens and inherits the rest:

```toml
# ~/.config/imacs/themes/dusk.toml
extends = "midnight"      # a built-in theme or another theme file
accent = "#e0a458"
on_accent = "#16181d"
selection = "#4a3b27"
cursor = "#e0a458"
```

- `extends` names the theme to start from. Without it, a file builds on `claude`, or on `paper` when it sets
  `mode = "light"`.
- A file named like a built-in theme replaces it and may extend it (`themes/claude.toml` with
  `extends = "claude"` adjusts the built-in claude theme).
- Chains can be as long as needed, up to 16 themes; a cycle is reported.
- Tokens set inline in `[theme]` win over every file.

Problems are shown in the top bar as `theme: …`, and IMACS keeps going: an unknown key or a
value that is not a color is left out, so the rest of the file still applies. A file that is not TOML, or a
theme that does not exist, falls back to `claude`.

## Tokens

Colors are `#rrggbb`.

| Token | Colors |
| --- | --- |
| `mode` | `"dark"` or `"light"`: egui's base widgets and the default syntax theme. |
| `background` | The main area behind terminal tiles and the diff. |
| `panel` | Bars, the steer bar and egui panels. |
| `raised` | Headers, cards, buttons and other surfaces above panels. |
| `sidebar` | The sidebar. |
| `topbar` | The top bar. |
| `terminal` | Terminal backgrounds; the answer to OSC 11 queries. |
| `overlay` | Windows, menus and popups. |
| `foreground` | Text; the answer to OSC 10 queries. |
| `muted` | Secondary text. |
| `quiet` | Hints, line numbers and the least important text. |
| `border` | Outlines of buttons, fields, cards and windows. |
| `separator` | Lines between panels and sections. |
| `accent` | The theme's highlight: the active agent, links, focus and primary buttons. |
| `on_accent` | Text and icons on accent fills, and on other strong fills such as the remove button. |
| `selection` | Selected text in terminals and fields. |
| `cursor` | Terminal and text cursors; the answer to OSC 12 queries. |
| `success`, `warning`, `error`, `info` | Status: done, needs you, failed, and reports. |
| `added`, `removed` | Backgrounds of added and removed diff lines. |
| `added_emphasis`, `removed_emphasis` | The changed words inside those lines. |
| `ansi` | Terminal colors 0–15: black, red, green, yellow, blue, magenta, cyan, white, then the bright ones; the answers to OSC 4. A shorter list replaces only the first colors. |
| `syntax` | The syntax theme for the diff and file view: any bat/two-face theme, such as `Monokai Extended`, `OneHalfDark`, `OneHalfLight`, `GitHub`, `Nord`, `Dracula` or `Solarized (light)`. |
| `mono_ui` | `true` sets all UI text in the monospace font. |
| `ui_font` | The UI font. |
| `mono_font` | The font of terminals, the diff and code. |
| `ui_size` | Base UI text size in points (13 by default); the whole UI scales with it. |
| `mono_size` | Terminal text size; overrides `[font] size`. |
| `diff_size` | Diff and editor text size; overrides `[font] diff_size`. |

The other 240 terminal colors are the standard xterm cube and gray ramp.

## Fonts

`ui_font` and `mono_font` take an installed family name, such as `"Inter"` or `"IBM Plex Mono"`, or a font
file (`.ttf`, `.otf` or `.ttc`): an absolute path, a `~/` path, or a path relative to the `themes/` folder.
Families are found through the system's font list on a background thread, and the regular face is used.
A family that is not installed, or a file that cannot be read, keeps IMACS's built-in fonts, which also stay
behind a theme font for any character it lacks.

The atomisk theme asks for Inter and IBM Plex Mono. Both are under the SIL Open Font License; IMACS does not
ship them. Install them, or download them and point the theme at the files:

```toml
[theme]
name = "atomisk"
ui_font = "~/fonts/Inter-Regular.ttf"
mono_font = "~/fonts/IBMPlexMono-Regular.ttf"
```

`ui_size` scales the UI through egui's zoom while terminals and the diff keep `mono_size` and `diff_size`.
Changing `ui_size` resets the keyboard zoom; other theme changes keep it. Sizes left out of the
theme come from `[font]`, and sizes outside 6–48 are reported and left out.

## Light themes

`mode = "light"` starts egui from its light widgets, and every color still comes from the theme. The theme's
mode wins over the system's appearance. A theme that switches mode without naming a `syntax` theme gets that
mode's default (`OneHalfLight` for light, `OneHalfDark` for dark), so a light theme never inherits dark code
colors.

Terminals answer color queries with the theme's colors: OSC 10 (foreground), 11 (background, the `terminal`
token), 12 (cursor) and 4 (the palette). Claude Code and Codex ask for these to pick a light or dark look of
their own, and after a theme change, later queries get the new colors. Programs that read `COLORFGBG`
instead get `15;0` in dark themes and `0;15` in light ones, set when the terminal starts.

## For developers

UI code takes every color from `Palette` (crates/imacs/src/theme.rs). A test fails on `Color32` constants,
gray and alpha helpers, or constructors with literal channels anywhere else in the imacs crate; derive a
shade from tokens with `lerp` or `gamma_multiply` instead, or add a token. A new token needs a field in
`ThemeTokens` (crates/imacs-core/src/theme.rs) and its `color_tokens!` list, a value in each built-in theme,
a `Palette` field, and a row in the table above; the tests check that the built-in themes set every token.
