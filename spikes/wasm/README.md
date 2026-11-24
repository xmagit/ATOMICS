# Phase 0 wasm spike (embedded ATOMICS design §9)

Standalone crate, outside the ATOMICS workspace. Answers: bundle size per renderer, `snow` on wasm32, and the
iOS soft-keyboard question (egui text agent vs hidden `<textarea>`).

**Status:** all four variants build for wasm32 (rustc 1.99.0, eframe 0.37.0, wasm-bindgen 0.2.130, binaryen 133, snow 0.10.0).
The lock now pins wasm-bindgen 0.2.129 (js-sys/web-sys 0.3.106) to match the CLI on the test machine; the sizes
below were measured with 0.2.130.
The `snow` handshake is unverified at runtime. The iPhone test (below) is done: path **b**, the `<textarea>`
overlay, passed in iOS Safari, iOS Chrome and the installed PWA, and is the chosen input path (design §4.4).

## Build (toolchain 1.99.0, `-j ${JOBS:-2}`)

Needs the wasm32 target, `cargo install wasm-bindgen-cli --version 0.2.129 --locked` (same version as the
`wasm-bindgen` in `Cargo.lock`), and optionally `wasm-opt` (binaryen) and `brotli`. `.cargo/config.toml` sets the
`getrandom_backend="wasm_js"` cfg that `snow` needs.

```
./build.sh wgpu          # eframe wgpu: WebGPU, WebGL2 fallback in one bundle
./build.sh glow          # eframe glow: WebGL2 only
./build.sh wgpu snow     # + Noise_XX handshake, to get the snow size delta (also: ./build.sh glow snow)
```

Each run prints `raw/gzip/brotli` bytes of the `.wasm` and copies the output to `www/pkg/`.
The page shows the `snow` handshake result in the egui window when built with `snow`.

## Results

`.wasm` size in bytes, `opt-level = "s"`, LTO, `panic = "abort"`. `wasm-opt -Oz --all-features` is applied after
wasm-bindgen. Brotli was not available in the VM (not measured).

| build | raw | raw + wasm-opt | gzip -9 | gzip -9 + wasm-opt |
|---|---|---|---|---|
| wgpu (WebGPU + WebGL2) | 5,423,029 | 4,975,276 | 2,035,906 | 2,039,606 |
| glow (WebGL2) | 3,340,268 | 3,063,804 | 1,284,384 | 1,281,741 |
| wgpu + snow | 5,481,187 | 5,029,889 | 2,056,573 | 2,062,436 |
| glow + snow | 3,398,461 | 3,118,417 | 1,305,483 | 1,304,465 |

- **snow delta** (Noise_XX, X25519, ChaChaPoly, BLAKE2s, RustCrypto, no `ring`): +58 KB raw (+1.1% on wgpu, +1.7% on glow), about +20 KB gzip.
- **wgpu vs glow:** glow is 38% smaller raw and 37% smaller gzipped (about 750 KB less over the wire).
- **wasm-opt** saves 8-9% raw but nothing once gzipped (gzip +/- 0.2%), so it matters little for transfer.

## iPhone test (design §4.4)

One page, one egui terminal widget, three input paths (links at the top):

- **a: agent** - eframe's hidden text input as shipped (it sets only `autocapitalize="off"`).
- **a+: agent, autocorrect off** - the same input with `autocorrect`, `autocomplete` and `spellcheck` turned off by the
  page after eframe starts.
- **b: textarea** - a transparent native `<textarea>` over the terminal. It forwards committed text, composition
  (as preedit) and Backspace/Return to the app, and keeps itself empty.

The key bar (Esc, Tab, sticky Ctrl, arrows) follows `visualViewport`, so it should sit on the soft keyboard. The
log under the terminal shows every event, newest first: `dom` (key, beforeinput, input, composition, focus,
gesture), `vv` (visualViewport and window resize/scroll), `egui` (what egui received), `ta` (what the textarea
path forwarded), `bar`, and `pty` (the bytes the host would get as `terminal_input`). **send** saves the whole log
on the serving machine in `logs/` (not committed); **copy** puts it on the clipboard; **mark** adds a separator.

Build (`./build.sh glow`; the renderer does not matter here), then serve on the tailnet only, never publicly:

```
./serve.py                                     # 127.0.0.1:8080
tailscale serve --bg --https=<port> http://127.0.0.1:8080
```

and open `https://<host>.<tailnet>.ts.net:<port>/`. Without tailnet HTTPS, `HOST=<tailnet-ip> ./serve.py` and
open `http://<tailnet-ip>:8080/` (copy then needs a secure context; send still works). To install the PWA: Share,
Add to Home Screen, open it from the icon.

### Checklist

Runs: Safari **a**, **a+**, **b**; then the installed PWA **a+** and **b**. In each run, tap **mark** before every
step and **send** at the end. Note the iOS version and, per step, pass or what went wrong.

1. **Focus:** tap the black terminal. The keyboard comes up; the page does not jump or zoom.
2. **Autocorrect and autocapitalise off:** type `hello wrold teh i "x" -- y` and a space. The terminal shows exactly
   that: lowercase `h` and `i`, no corrections, straight quotes, `--` not a dash.
3. **Backspace and Return:** tap Backspace three times (three DELs), hold it two seconds (repeats), then Return.
4. **Swedish and emoji:** on the Swedish keyboard type `åäö ÅÄÖ`; on the English one long-press `a` for `å`; then
   emoji `👍 👍🏽 👨‍👩‍👧`. Each arrives once; `å` is `U+00E5` (not `a` + `U+030A`); each emoji is one unbroken
   sequence. Optional, for real composition: Japanese Romaji `nihon` → 日本 shows yellow preedit, then one commit.
5. **Dictation:** tap the mic and say "list the files in my home directory". The sentence arrives once; words iOS
   revises while you speak are not duplicated.
6. **Key bar:** with the keyboard up the bar sits directly on it (no gap, not hidden). Esc, Tab and the arrows keep
   the keyboard up and show `^[`, `^I`, `^[[D`…; Ctrl then `c` shows `^C`. Rotate with the keyboard up: the bar follows.
7. **Pinch:** pinch on the terminal: the font size in the status line changes and the page does not zoom (`vv`
   lines keep `scale=1.00`). Pinch on the log: the page does not zoom.
