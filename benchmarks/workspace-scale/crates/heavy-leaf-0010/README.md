# heavy-leaf-0010

Image / graphics subgraph (pure-Rust): `image` (with `png`,
`jpeg-decoder`, `gif`, `tiff`, `bmp`, `ico`, `qoi`, `pnm`), `qoi`,
`exr` (OpenEXR HDR), `ravif` + `rav1e` (AV1 still encoder),
`imageproc`, `resvg` + `tiny-skia` + `usvg` (SVG render to
raster), `fontdue`, `palette`, `color`, `kurbo`, `lyon`. Heavy
mixed compute (proc-macros, generics, large pixel-data routines)
distinct from the prior nine leaves' subgraphs. No `cc-rs` build
scripts (webp / libheif / libwebp avoided to stay pure-Rust).
See parent `../README.md` and soldr#648.
