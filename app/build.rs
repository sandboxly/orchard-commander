//! Linux: the executable finds the bundled FFmpeg libraries in `lib/` next to
//! itself (/opt/orchard-commander/lib, the tarball, the AppImage), wherever
//! the folder was unpacked. `--disable-new-dtags` makes it a DT_RPATH, which
//! (unlike DT_RUNPATH) also applies to the libraries' own dependencies
//! (libavformat → libavcodec → libswresample). See packaging/stage.sh.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        println!("cargo:rustc-link-arg-bins=-Wl,--disable-new-dtags,-rpath,$ORIGIN/lib:$ORIGIN");
    }
}
