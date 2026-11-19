//! Build steps:
//! 1. Fetch the pinned speech model and runtime DLLs into `models/` and
//!    `runtime/` (once; every file is checked against its SHA-256).
//! 2. Embed the icon and version information in fasttts.exe.

use sha2::{Digest, Sha256};
use std::{
    env, fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
};

/// NVIDIA Parakeet TDT 0.6B v2 (CC-BY-4.0), ONNX export by istupakov.
const MODEL_DIR: &str = "models/parakeet-tdt-0.6b-v2";
const MODEL_URL: &str =
    "https://huggingface.co/istupakov/parakeet-tdt-0.6b-v2-onnx/resolve/0bbb45a3365852604aef28b538a8f066f4ccaa85";
const MODEL_FILES: &[(&str, &str)] = &[
    (
        "nemo128.onnx",
        "a9fde1486ebfcc08f328d75ad4610c67835fea58c73ba57e3209a6f6cf019e9f",
    ),
    (
        "encoder-model.int8.onnx",
        "3e0581fda6ab843888b51e56d7ee78b6d5bc3237ec113af1f732d1d5286aa155",
    ),
    (
        "decoder_joint-model.int8.onnx",
        "a449f49acd68979d418651dd2dcb737cc0f1bf0225e009e29ee326354edbf7d3",
    ),
    (
        "vocab.txt",
        "ec182b70dd42113aff6c5372c75cac58c952443eb22322f57bbd7f53977d497d",
    ),
];

/// Microsoft's ONNX Runtime 1.32 (MIT) and the Visual C++ runtime it needs
/// (Microsoft-signed redistributable DLLs), taken from pinned wheels.
const RUNTIME_DIR: &str = "runtime";
const WHEELS: &[(&str, &str, &[&str])] = &[
    (
        "https://files.pythonhosted.org/packages/a9/3e/27039badc39a1b01628b6b40b1ff1bf4a886e6dfb2f311c483606b7c86f7/onnxruntime-1.32.0-cp312-cp312-win_amd64.whl",
        "efe7456952c243a05a53d5b8732953625144642c2f3989094277f6217f0c0c40",
        &["onnxruntime/capi/onnxruntime.dll"],
    ),
    (
        "https://files.pythonhosted.org/packages/98/33/be9ef68901200813fc174a06758b4ae48ca4eb58fa783280dd879d5fd048/msvc_runtime-14.51.36231-cp312-cp312-win_amd64.whl",
        "89f70d1a37bb64164cdb09d02dd82d6b1400e1310be2995b4ed5a9f23efe2222",
        &[
            "msvc_runtime-14.51.36231.data/data/Scripts/msvcp140.dll",
            "msvc_runtime-14.51.36231.data/data/Scripts/msvcp140_1.dll",
            "msvc_runtime-14.51.36231.data/data/Scripts/vcruntime140.dll",
            "msvc_runtime-14.51.36231.data/data/Scripts/vcruntime140_1.dll",
        ],
    ),
];

fn main() {
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    println!("cargo::rerun-if-changed=assets/fasttts.ico");
    println!("cargo::rerun-if-changed=Cargo.toml");
    println!("cargo::rerun-if-changed={MODEL_DIR}");
    println!("cargo::rerun-if-changed={RUNTIME_DIR}");
    fetch_model(&root.join(MODEL_DIR));
    fetch_runtime(&root.join(RUNTIME_DIR));
    embed_resources(&root);
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A file counts as present once it was verified; the `.sha256` marker
/// avoids hashing 650 MB on every build.
fn verified(path: &Path, expected: &str) -> bool {
    let marker = path.with_extension(format!(
        "{}.sha256",
        path.extension().unwrap_or_default().to_string_lossy()
    ));
    path.is_file() && fs::read_to_string(marker).is_ok_and(|hash| hash.trim() == expected)
}

fn mark_verified(path: &Path, hash: &str) {
    let marker = path.with_extension(format!(
        "{}.sha256",
        path.extension().unwrap_or_default().to_string_lossy()
    ));
    fs::write(marker, hash).unwrap();
}

fn download(url: &str, expected: &str) -> Vec<u8> {
    println!(
        "cargo::warning=downloading {}",
        url.rsplit('/').next().unwrap()
    );
    let mut body = Vec::new();
    ureq::get(url)
        .call()
        .unwrap_or_else(|err| panic!("could not download {url}: {err}"))
        .into_body()
        .into_reader()
        .read_to_end(&mut body)
        .unwrap_or_else(|err| panic!("could not download {url}: {err}"));
    let actual = sha256(&body);
    assert_eq!(actual, expected, "{url} does not match its pinned SHA-256");
    body
}

fn fetch_model(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    for (name, hash) in MODEL_FILES {
        let path = dir.join(name);
        if verified(&path, hash) {
            continue;
        }
        let body = download(&format!("{MODEL_URL}/{name}"), hash);
        fs::File::create(&path).unwrap().write_all(&body).unwrap();
        mark_verified(&path, hash);
    }
}

fn fetch_runtime(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    for (url, hash, members) in WHEELS {
        let wanted: Vec<PathBuf> = members
            .iter()
            .map(|member| dir.join(member.rsplit('/').next().unwrap()))
            .collect();
        if wanted.iter().all(|path| verified(path, hash)) {
            continue;
        }
        let wheel = download(url, hash);
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(wheel)).unwrap();
        for (member, path) in members.iter().zip(&wanted) {
            let mut bytes = Vec::new();
            zip.by_name(member)
                .unwrap_or_else(|_| panic!("{member} is missing from {url}"))
                .read_to_end(&mut bytes)
                .unwrap();
            fs::write(path, bytes).unwrap();
            mark_verified(path, hash);
        }
    }
}

fn embed_resources(root: &Path) {
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let icon = root.join("assets/fasttts.ico");
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
