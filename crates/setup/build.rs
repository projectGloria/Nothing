//! Embeds the freshly built release binaries as the installer payload, plus the shared
//! icon/manifest resources.

include!("../ui/res.rs");

const PAYLOAD: [&str; 2] = ["nothing.exe", "nothing-svc.exe"];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../ui/res.rs");
    println!("cargo:rerun-if-env-changed=NOTHING_PAYLOAD_DIR");
    let manifest_dir = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let payload = std::env::var_os("NOTHING_PAYLOAD_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| manifest_dir.join("..").join("..").join("target").join("release"));
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    for f in PAYLOAD {
        let src = payload.join(f);
        if !src.exists() {
            panic!("{} not found. Build the app first: cargo build --release (or run build-installer.cmd)", src.display());
        }
        println!("cargo:rerun-if-changed={}", src.display());
        std::fs::copy(&src, out.join(f)).unwrap();
    }
    embed_resources();
}
