// rust-embed needs the folder to exist at compile time. The UI is built by
// trunk into crates/bc-ui/dist (scripts/build-ui.sh); it may also be a symlink.
fn main() {
    let dist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../bc-ui/dist");
    if !dist.exists() {
        let _ = std::fs::remove_file(&dist); // dangling symlink
        let _ = std::fs::create_dir_all(&dist);
    }
    println!("cargo:rerun-if-changed=../bc-ui/dist");
}
