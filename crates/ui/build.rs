//! Icon + manifest resources (see res.rs, shared with the installer).

include!("res.rs");

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=res.rs");
    embed_resources();
}
