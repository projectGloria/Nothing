//! Embeds the freshly built release binaries as the installer payload, plus the shared
//! icon/manifest resources, and packs the sparse identity package (nothing.msix) that puts
//! "Nothing" in Windows 11's compact Explorer context menu.

include!("../ui/res.rs");

const PAYLOAD: [&str; 3] = ["nothing.exe", "nothing-svc.exe", "nothing_shell.dll"];

/// Publisher of an unsigned package. Windows 11 registers a package without a signature
/// only if its Publisher carries this OID (so it can never collide with a signed identity)
/// and `Add-AppxPackage -AllowUnsigned` runs elevated, which our install step is.
const UNSIGNED_PUBLISHER: &str = "CN=projectGloria, OID.2.25.311729368913984317654407730594956997722=1";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../ui/res.rs");
    println!("cargo:rerun-if-env-changed=NOTHING_PAYLOAD_DIR");
    let manifest_dir = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let root = manifest_dir.join("..").join("..");
    let payload = std::env::var_os("NOTHING_PAYLOAD_DIR").map(std::path::PathBuf::from).unwrap_or_else(|| root.join("target").join("release"));
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    for f in PAYLOAD {
        let src = payload.join(f);
        if !src.exists() {
            panic!("{} not found. Build the app first: cargo build --release (or run build-installer.cmd)", src.display());
        }
        println!("cargo:rerun-if-changed={}", src.display());
        std::fs::copy(&src, out.join(f)).unwrap();
    }
    // Package logo: the manifest references it relative to the install folder.
    let logo = root.join("assets").join("icon-256.png");
    println!("cargo:rerun-if-changed={}", logo.display());
    std::fs::copy(&logo, out.join("nothing.png")).unwrap();
    pack_msix(&root, &out);
    embed_resources();
}

/// Newest Windows SDK tool (makeappx.exe / signtool.exe) for the host architecture.
fn sdk_tool(name: &str) -> std::path::PathBuf {
    let kits = std::path::PathBuf::from(std::env::var("ProgramFiles(x86)").unwrap_or_else(|_| r"C:\Program Files (x86)".into()))
        .join("Windows Kits")
        .join("10")
        .join("bin");
    let mut versions: Vec<_> = std::fs::read_dir(&kits)
        .unwrap_or_else(|e| panic!("Windows SDK not found at {}: {e}", kits.display()))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join("x64").join(name).exists())
        .collect();
    // "10.0.26100.0" sorts correctly as a list of numbers, not as a string.
    versions.sort_by_key(|p| p.file_name().unwrap().to_string_lossy().split('.').map(|n| n.parse::<u32>().unwrap_or(0)).collect::<Vec<_>>());
    versions.last().unwrap_or_else(|| panic!("{name} not found in {}", kits.display())).join("x64").join(name)
}

fn run(tool: &std::path::Path, args: &[&std::ffi::OsStr]) {
    let o = std::process::Command::new(tool).args(args).output().unwrap_or_else(|e| panic!("{}: {e}", tool.display()));
    if !o.status.success() {
        panic!("{} failed:\n{}{}", tool.display(), String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    }
}

/// Fill in package/AppxManifest.xml and pack it. Unsigned by default; set NOTHING_SIGN_PFX
/// (+ NOTHING_SIGN_PASSWORD, and NOTHING_SIGN_PUBLISHER = the cert's exact subject) to sign
/// it with a real code-signing cert instead. The install step is the same either way.
fn pack_msix(root: &std::path::Path, out: &std::path::Path) {
    for v in ["NOTHING_SIGN_PFX", "NOTHING_SIGN_PASSWORD", "NOTHING_SIGN_PUBLISHER"] {
        println!("cargo:rerun-if-env-changed={v}");
    }
    let template = root.join("package").join("AppxManifest.xml");
    println!("cargo:rerun-if-changed={}", template.display());
    let pfx = std::env::var_os("NOTHING_SIGN_PFX");
    let publisher = match &pfx {
        Some(_) => std::env::var("NOTHING_SIGN_PUBLISHER").expect("NOTHING_SIGN_PFX needs NOTHING_SIGN_PUBLISHER (the cert subject, e.g. CN=...)"),
        None => UNSIGNED_PUBLISHER.to_string(),
    };
    // Package versions are four-part; the same version can't be registered twice, which
    // install.rs sidesteps by removing the old registration first.
    let version = format!("{}.0", env!("CARGO_PKG_VERSION"));
    let xml = std::fs::read_to_string(&template).unwrap().replace("{VERSION}", &version).replace("{PUBLISHER}", &publisher);
    let dir = out.join("msix");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("AppxManifest.xml"), xml).unwrap();
    let msix = out.join("nothing.msix");
    // /nv: don't check that files the manifest names (dll, exe, logo) are inside the
    // package; they live in the external location, i.e. the install folder.
    run(&sdk_tool("makeappx.exe"), &["pack".as_ref(), "/o".as_ref(), "/nv".as_ref(), "/d".as_ref(), dir.as_os_str(), "/p".as_ref(), msix.as_os_str()]);
    if let Some(pfx) = pfx {
        let pw = std::env::var_os("NOTHING_SIGN_PASSWORD").unwrap_or_default();
        run(&sdk_tool("signtool.exe"), &["sign".as_ref(), "/fd".as_ref(), "SHA256".as_ref(), "/f".as_ref(), &pfx, "/p".as_ref(), &pw, msix.as_os_str()]);
    }
}
