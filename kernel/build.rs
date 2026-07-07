fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:rustc-link-arg=-T{}/linker.ld", dir);
    println!("cargo:rerun-if-changed=linker.ld");

    // Bake the userspace hello ELF into a generated Rust file.
    // If the file doesn't exist yet (first build before running build.ps1),
    // generate an empty slice so the kernel still compiles.
    let root = std::path::Path::new(&dir).parent().unwrap().to_path_buf();
    let hello = root.join("userspace/target/x86_64-unknown-none/release/hello");

    let out_dir = std::env::var("OUT_DIR").unwrap();
    let gen     = std::path::Path::new(&out_dir).join("hello_elf.rs");

    if hello.exists() {
        let bytes = std::fs::read(&hello).expect("read hello ELF");
        let lit: String = bytes.iter().map(|b| format!("{},", b)).collect();
        std::fs::write(&gen, format!("static HELLO_ELF: &[u8] = &[{}];\n", lit)).unwrap();
        println!("cargo:rerun-if-changed={}", hello.display());
    } else {
        std::fs::write(&gen, "static HELLO_ELF: &[u8] = &[];\n").unwrap();
    }

    // Bake the userspace hwtest ELF the same way.
    let hwtest     = root.join("userspace/target/x86_64-unknown-none/release/hwtest");
    let gen_hwtest = std::path::Path::new(&out_dir).join("hwtest_elf.rs");
    if hwtest.exists() {
        let bytes = std::fs::read(&hwtest).expect("read hwtest ELF");
        let lit: String = bytes.iter().map(|b| format!("{},", b)).collect();
        std::fs::write(&gen_hwtest, format!("static HWTEST_ELF: &[u8] = &[{}];\n", lit)).unwrap();
        println!("cargo:rerun-if-changed={}", hwtest.display());
    } else {
        std::fs::write(&gen_hwtest, "static HWTEST_ELF: &[u8] = &[];\n").unwrap();
    }
}
