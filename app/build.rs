//! Platform bits of the executable itself.
//!
//! - Windows: the icon (Explorer, Start menu and desktop shortcuts, the
//!   uninstall entry: 1.x's PyInstaller `icon=app.ico`) and version
//!   information, as a resource compiled into the .exe.
//! - Linux: the packages ship FFmpeg's shared libraries (LGPL) with the
//!   binary, in /opt/orchard-commander/lib (or next to it in the tarball). An
//!   RPATH with `$ORIGIN` finds them there first, then the system's; a
//!   DT_RPATH rather than DT_RUNPATH so the libraries' dependencies on each
//!   other are found there as well. (macOS: the bundle rewrites install names
//!   to `@executable_path/../Frameworks`.)

use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    match std::env::var("CARGO_CFG_TARGET_OS").as_deref() {
        Ok("windows") => windows_resource(),
        Ok("linux") => println!("cargo:rustc-link-arg-bins=-Wl,--disable-new-dtags,-rpath,$ORIGIN/lib:$ORIGIN"),
        _ => {}
    }
}

fn windows_resource() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let icon = Path::new(&manifest_dir).join("resources").join("icon.ico");
    println!("cargo:rerun-if-changed={}", icon.display());
    let version = std::env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION");
    let mut parts: Vec<u16> = version.split(['.', '-', '+']).map_while(|p| p.parse().ok()).collect();
    parts.resize(4, 0);
    let numeric = parts.iter().map(u16::to_string).collect::<Vec<_>>().join(",");
    let rc = format!(
        r#"#pragma code_page(65001)
1 ICON "{icon}"
1 VERSIONINFO
FILEVERSION {numeric}
PRODUCTVERSION {numeric}
FILEOS 0x40004
FILETYPE 0x1
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904b0"
    BEGIN
      VALUE "CompanyName", "Orchard Commander"
      VALUE "FileDescription", "Orchard Commander"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "orchard-commander"
      VALUE "LegalCopyright", "Stephan Lengl, MIT License"
      VALUE "OriginalFilename", "orchard-commander.exe"
      VALUE "ProductName", "Orchard Commander"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#,
        icon = icon.display().to_string().replace('\\', "\\\\"),
    );
    let out = Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR")).join("orchard-commander.rc");
    std::fs::write(&out, rc).expect("write the resource script");
    embed_resource::compile(&out, embed_resource::NONE).manifest_optional().expect("compile the Windows resources");
}
