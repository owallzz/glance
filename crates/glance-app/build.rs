//! Windows: embed the app icon and version info in glance.exe, so Explorer,
//! the taskbar and the window title bar show the glance icon.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    #[cfg(windows)]
    windows::embed_resources();
}

#[cfg(windows)]
mod windows {
    use std::path::PathBuf;

    pub fn embed_resources() {
        if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
            return;
        }
        let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
        let icon = manifest.join("../../packaging/windows/glance.ico");
        println!("cargo:rerun-if-changed={}", icon.display());
        let version = std::env::var("CARGO_PKG_VERSION").unwrap();
        let mut parts = version
            .split(['.', '-', '+'])
            .map(|p| p.parse::<u16>().unwrap_or(0));
        let (major, minor, patch) = (
            parts.next().unwrap_or(0),
            parts.next().unwrap_or(0),
            parts.next().unwrap_or(0),
        );
        // rc.exe takes forward slashes, which spares escaping backslashes.
        let icon = icon.display().to_string().replace('\\', "/");
        let rc = format!(
            r#"#include <winver.h>
1 ICON "{icon}"
VS_VERSION_INFO VERSIONINFO
FILEVERSION {major},{minor},{patch},0
PRODUCTVERSION {major},{minor},{patch},0
FILEFLAGSMASK 0x3fL
FILEFLAGS 0x0L
FILEOS VOS_NT_WINDOWS32
FILETYPE VFT_APP
FILESUBTYPE VFT2_UNKNOWN
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904b0"
    BEGIN
      VALUE "CompanyName", "glance contributors"
      VALUE "FileDescription", "glance"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "glance"
      VALUE "LegalCopyright", "MIT OR Apache-2.0"
      VALUE "OriginalFilename", "glance.exe"
      VALUE "ProductName", "glance"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
        );
        let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("glance.rc");
        std::fs::write(&out, rc).expect("writing glance.rc");
        embed_resource::compile(&out, embed_resource::NONE)
            .manifest_optional()
            .expect("embedding the Windows resources");
    }
}
