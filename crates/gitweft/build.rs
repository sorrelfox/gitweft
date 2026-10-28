//! On Windows, gives the executable its icon and the version details Explorer shows. The
//! application manifest (DPI awareness, common controls 6) comes with GPUI, whose build
//! script links its resources into every program that uses it.

use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let icon = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("resources")
        .join("windows")
        .join("gitweft.ico");
    println!("cargo::rerun-if-changed={}", icon.display());

    let version = env::var("CARGO_PKG_VERSION").unwrap();
    let numbers = ["MAJOR", "MINOR", "PATCH"]
        .map(|part| env::var(format!("CARGO_PKG_VERSION_{part}")).unwrap())
        .join(",");
    // GPUI gives the window the icon with ID 1. FILEOS is VOS_NT_WINDOWS32 and FILETYPE
    // VFT_APP; the strings are what Explorer's Details tab and Task Manager show.
    let script = format!(
        r#"1 ICON "{icon}"

1 VERSIONINFO
FILEVERSION {numbers},0
PRODUCTVERSION {numbers},0
FILEOS 0x40004
FILETYPE 0x1
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "FileDescription", "Gitweft"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "gitweft"
      VALUE "OriginalFilename", "gitweft.exe"
      VALUE "ProductName", "Gitweft"
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
    let rc = PathBuf::from(env::var("OUT_DIR").unwrap()).join("gitweft.rc");
    fs::write(&rc, script).unwrap();
    embed_resource::compile(&rc, embed_resource::NONE)
        .manifest_required()
        .unwrap();
}
