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

    // Bake the userspace rtl8139d ELF (the persistent NIC driver process —
    // see rtl8139.rs) the same way.
    let rtl8139d     = root.join("userspace/target/x86_64-unknown-none/release/rtl8139d");
    let gen_rtl8139d = std::path::Path::new(&out_dir).join("rtl8139d_elf.rs");
    if rtl8139d.exists() {
        let bytes = std::fs::read(&rtl8139d).expect("read rtl8139d ELF");
        let lit: String = bytes.iter().map(|b| format!("{},", b)).collect();
        std::fs::write(&gen_rtl8139d, format!("static RTL8139D_ELF: &[u8] = &[{}];\n", lit)).unwrap();
        println!("cargo:rerun-if-changed={}", rtl8139d.display());
    } else {
        std::fs::write(&gen_rtl8139d, "static RTL8139D_ELF: &[u8] = &[];\n").unwrap();
    }

    // Bake the userspace hdad ELF (the persistent HDA audio driver process —
    // see hda.rs) the same way.
    let hdad     = root.join("userspace/target/x86_64-unknown-none/release/hdad");
    let gen_hdad = std::path::Path::new(&out_dir).join("hdad_elf.rs");
    if hdad.exists() {
        let bytes = std::fs::read(&hdad).expect("read hdad ELF");
        let lit: String = bytes.iter().map(|b| format!("{},", b)).collect();
        std::fs::write(&gen_hdad, format!("static HDAD_ELF: &[u8] = &[{}];\n", lit)).unwrap();
        println!("cargo:rerun-if-changed={}", hdad.display());
    } else {
        std::fs::write(&gen_hdad, "static HDAD_ELF: &[u8] = &[];\n").unwrap();
    }

    // Bake the userspace ahcid ELF (the persistent AHCI/SATA driver process —
    // see ahci.rs) the same way.
    let ahcid     = root.join("userspace/target/x86_64-unknown-none/release/ahcid");
    let gen_ahcid = std::path::Path::new(&out_dir).join("ahcid_elf.rs");
    if ahcid.exists() {
        let bytes = std::fs::read(&ahcid).expect("read ahcid ELF");
        let lit: String = bytes.iter().map(|b| format!("{},", b)).collect();
        std::fs::write(&gen_ahcid, format!("static AHCID_ELF: &[u8] = &[{}];\n", lit)).unwrap();
        println!("cargo:rerun-if-changed={}", ahcid.display());
    } else {
        std::fs::write(&gen_ahcid, "static AHCID_ELF: &[u8] = &[];\n").unwrap();
    }

    // Bake the userspace xhcid ELF (the persistent USB HID poller process —
    // see xhci.rs) the same way.
    let xhcid     = root.join("userspace/target/x86_64-unknown-none/release/xhcid");
    let gen_xhcid = std::path::Path::new(&out_dir).join("xhcid_elf.rs");
    if xhcid.exists() {
        let bytes = std::fs::read(&xhcid).expect("read xhcid ELF");
        let lit: String = bytes.iter().map(|b| format!("{},", b)).collect();
        std::fs::write(&gen_xhcid, format!("static XHCID_ELF: &[u8] = &[{}];\n", lit)).unwrap();
        println!("cargo:rerun-if-changed={}", xhcid.display());
    } else {
        std::fs::write(&gen_xhcid, "static XHCID_ELF: &[u8] = &[];\n").unwrap();
    }
}
