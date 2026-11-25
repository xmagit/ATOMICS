# Fonts

`InterVariable.ttf` is the browser client's face for simple mode's text (`atomics-web`, `BundledFonts`). The desktop
uses the system's fonts.

- Source: the official Inter release v4.1, <https://github.com/rsms/inter/releases/tag/v4.1>,
  `Inter-4.1.zip` (33,707,794 bytes, SHA-256 `9883fdd4a49d4fb66bd8177ba6625ef9a64aa45899767dde3d36aa425756b11e`).
  GitHub publishes no digest for that asset; its size matched the release API's.
- File: `InterVariable.ttf` from that archive, unchanged
  (SHA-256 `4989b125924991b90d05b2d16e0e388c48f7d5bb8b30539bbf9c755278d0ccaf`).
- Licence: the SIL Open Font License 1.1, `Inter-OFL.txt` (the archive's `LICENSE.txt`, unchanged).

`PhosphorIcons.ttf` is the UI's icon font, on every platform (`atomics_ui::icons`): Phosphor's regular weight,
cut down to the glyphs the UI draws, so it adds about 8 KB to the binary and the browser client.

- Source: `@phosphor-icons/web` 2.1.2 from the npm registry,
  <https://registry.npmjs.org/@phosphor-icons/web/-/web-2.1.2.tgz>
  (SHA-256 `40e3096099ca818c047979cece6a3764944d5200c67b64092f7bcd8bcd1d2b08`), its `src/regular/Phosphor.ttf`
  (SHA-256 `06b91e022b7ee899a63efced879392a74f0bacbda54e4467e9f663220d173a10`).
- Cut with fontTools 4.68: `pyftsubset Phosphor.ttf --unicodes=<the code points in crates/atomics-ui/src/icons.rs>
  --layout-features='' --no-hinting --desubroutinize --name-IDs='*' --output-file=PhosphorIcons.ttf`. An icon added
  to `icons.rs` needs its code point added to that list and the font cut again.
- Licence: MIT, `Phosphor-MIT.txt` (the package's `LICENSE`, unchanged).
