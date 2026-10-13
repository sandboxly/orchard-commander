# Shared settings for the macOS packaging scripts (sourced).
#
# The oldest macOS the app runs on. 12.0 is set by the prebuilt PDFium
# (bblanchon/pdfium-binaries mac-arm64: LC_BUILD_VERSION minos 12.0); Rust's
# aarch64-apple-darwin target alone would allow 11.0, and the app's own code
# (winit, wgpu/Metal, muda, objc2, cpal) uses nothing newer. FFmpeg, the
# vendored C/C++ code (UnRAR, Oniguruma, zstd, liblzma) and the Rust code are
# all built for this target; package.sh checks every Mach-O in the bundle.
export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-12.0}"
