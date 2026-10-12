# Bundled fonts

- `Montserrat-Regular.ttf`, `Montserrat-SemiBold.ttf` —
  [Montserrat](https://fonts.google.com/specimen/Montserrat) by Julieta Ulanovsky,
  licensed under the SIL Open Font License 1.1.

Both are static instances (weight 400 and 600) cut from the upstream variable
font with `fontTools.varLib.instancer`, then subset with `pyftsubset`
(`--unicodes="U+0000-D7FF,U+F900-10FFFF"`). The subsetting also drops the
`U+E000–U+F8FF` Private Use Area, which would otherwise shadow the Phosphor icon
font — Montserrat is the primary font for both proportional *and* monospace text
in reel, so it must not claim those codepoints.

Icons come from the `egui-phosphor` crate (Phosphor Icons, MIT); the font itself
is compiled in by that crate and is not vendored here.
