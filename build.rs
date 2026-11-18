//! Embed the icon and version information in fasttts.exe.

use std::{env, fs, path::PathBuf, process::Command};

fn main() {
    println!("cargo::rerun-if-changed=assets/fasttts.ico");
    println!("cargo::rerun-if-changed=Cargo.toml");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let icon = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("assets/fasttts.ico");
    let version = env::var("CARGO_PKG_VERSION").unwrap();
    let numbers: Vec<&str> = version.split(['.', '-']).take(3).collect();
    let rc = format!(
        r#"1 ICON "{icon}"
1 VERSIONINFO
FILEVERSION {n},0
PRODUCTVERSION {n},0
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "FileDescription", "FastTTS - hold Caps Lock to dictate"
      VALUE "ProductName", "FastTTS"
      VALUE "FileVersion", "{version}"
      VALUE "ProductVersion", "{version}"
      VALUE "LegalCopyright", "MIT License"
      VALUE "OriginalFilename", "fasttts.exe"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#,
        icon = icon.display().to_string().replace('\\', "/"),
        n = numbers.join(","),
    );
    let script = out.join("fasttts.rc");
    let resource = out.join("fasttts.res");
    fs::write(&script, rc).unwrap();
    let status = Command::new("llvm-rc")
        .arg("/nologo")
        .arg("/fo")
        .arg(&resource)
        .arg(&script)
        .status()
        .expect("llvm-rc not found: install LLVM MinGW and put its bin folder on PATH");
    assert!(status.success(), "llvm-rc failed");
    println!("cargo::rustc-link-arg-bins={}", resource.display());
}
