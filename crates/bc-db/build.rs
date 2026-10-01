//! Embeds `migrations/req_ws*_*.sql` (DDL requested by other workstreams) so they
//! are applied once, by name, after the numbered migrations.
use std::fmt::Write as _;

fn main() {
    println!("cargo:rerun-if-changed=migrations");
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with("req_") && n.ends_with(".sql"))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    let mut out = String::from("pub const REQUESTED: &[(&str, &str)] = &[\n");
    for n in &names {
        let _ = writeln!(
            out,
            "    ({n:?}, include_str!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/migrations/{n}\"))),"
        );
    }
    out.push_str("];\n");
    let dest = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("req_migrations.rs");
    std::fs::write(dest, out).unwrap();
}
