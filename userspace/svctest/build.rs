fn main() {
    // Embed the already-built `hello` ELF so this test can prove out
    // SYS_SPAWN_BYTES without needing HepFS at all (spawn_bytes takes raw
    // bytes already in the caller's own memory). Same empty-slice-if-absent
    // fallback as every other embedded-ELF build.rs in this project — on a
    // clean workspace build `hello` may not exist yet on the first pass
    // (cargo doesn't order sibling-crate builds for a raw file read like
    // this), so a second `cargo build --release` in `userspace/` picks it
    // up once `hello`'s binary exists.
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let root = std::path::Path::new(&dir).parent().unwrap().to_path_buf(); // userspace/
    let hello = root.join("target/x86_64-unknown-none/release/hello");

    let out_dir = std::env::var("OUT_DIR").unwrap();
    let gen = std::path::Path::new(&out_dir).join("hello_elf.rs");

    if hello.exists() {
        let bytes = std::fs::read(&hello).expect("read hello ELF");
        let lit: String = bytes.iter().map(|b| format!("{},", b)).collect();
        std::fs::write(&gen, format!("static HELLO_ELF: &[u8] = &[{}];\n", lit)).unwrap();
        println!("cargo:rerun-if-changed={}", hello.display());
    } else {
        std::fs::write(&gen, "static HELLO_ELF: &[u8] = &[];\n").unwrap();
    }
}
