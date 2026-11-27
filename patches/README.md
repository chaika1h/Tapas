# Patched dependencies

`Cargo.toml` uses these in place of the crates.io versions (`[patch.crates-io]`).
Each is the published crate, unchanged but for the lines marked `Tapas patch`.

## i-slint-renderer-femtovg 1.19.0

Slint's OpenGL renderer, from [Slint](https://slint.dev) 1.19.0, used under the
Slint Royalty-free License 2.0 (`LICENSES/`), which allows modified copies in a
desktop application.

**The fix** (`itemrenderer.rs`, `draw_glyph_run`): text is drawn on whole
device pixels. femtovg rounds each glyph's position in the item's own
coordinates and the GPU then adds the item's offset; at a fractional scale
factor (125 %, 150 %…) that offset often has a fraction, so glyphs landed
between pixels and looked smeared or uneven. The glyphs are now drawn under the
whole-pixel part of the offset, with the fraction added to their positions, and
each starts on a whole pixel across, as Windows places UI text.

**Updating Slint:** `slint` is pinned (`=1.19.0`). When raising it, copy the new
`i-slint-renderer-femtovg` from the registry here and apply the same change, or
drop the patch if Slint has fixed this.
