# PDFium

PDF previews (crate `oc-pdf`) use PDFium, the PDF engine of Chromium, as a
prebuilt shared library (`libpdfium.dylib` / `pdfium.dll` / `libpdfium.so`)
from <https://github.com/bblanchon/pdfium-binaries>, release `chromium/7881`
(PDFium 151.0.7881.0), fetched by `scripts/fetch-pdfium.sh`.

Ship these texts with every build that bundles the library (about/licenses
page; `/usr/share/doc/orchard-commander/` on Linux):

- `pdfium.txt`: PDFium itself, BSD-3-Clause (plus the Apache-2.0 text that
  covers parts of it)
- `pdfium-binaries.txt`: the build scripts of the binary distribution (MIT)
- all other files: third-party code compiled into the library (FreeType,
  libjpeg-turbo, OpenJPEG, libpng, libtiff, zlib, Little CMS, ICU, Abseil,
  AGG 2.3, fast_float, simdutf, LLVM libc)

They are copied unchanged from the release archive (`LICENSE` and
`licenses/`); `scripts/fetch-pdfium.sh` also puts them next to the library in
`vendor/pdfium/<platform>/`. Update this folder together with the pinned
release.
