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

    // Bake the userspace nvmed ELF (the persistent NVMe I/O-queue process —
    // see nvme.rs) the same way.
    let nvmed     = root.join("userspace/target/x86_64-unknown-none/release/nvmed");
    let gen_nvmed = std::path::Path::new(&out_dir).join("nvmed_elf.rs");
    if nvmed.exists() {
        let bytes = std::fs::read(&nvmed).expect("read nvmed ELF");
        let lit: String = bytes.iter().map(|b| format!("{},", b)).collect();
        std::fs::write(&gen_nvmed, format!("static NVMED_ELF: &[u8] = &[{}];\n", lit)).unwrap();
        println!("cargo:rerun-if-changed={}", nvmed.display());
    } else {
        std::fs::write(&gen_nvmed, "static NVMED_ELF: &[u8] = &[];\n").unwrap();
    }

    // Bake the userspace gopd ELF (the persistent GOP-flush driver process —
    // see framebuffer.rs) the same way.
    let gopd     = root.join("userspace/target/x86_64-unknown-none/release/gopd");
    let gen_gopd = std::path::Path::new(&out_dir).join("gopd_elf.rs");
    if gopd.exists() {
        let bytes = std::fs::read(&gopd).expect("read gopd ELF");
        let lit: String = bytes.iter().map(|b| format!("{},", b)).collect();
        std::fs::write(&gen_gopd, format!("static GOPD_ELF: &[u8] = &[{}];\n", lit)).unwrap();
        println!("cargo:rerun-if-changed={}", gopd.display());
    } else {
        std::fs::write(&gen_gopd, "static GOPD_ELF: &[u8] = &[];\n").unwrap();
    }

    // Bake the userspace memtest ELF (proves out SYS_MMAP_ANON — Phase 1 of
    // the desktop-to-userspace migration, see PLAN.md) the same way.
    let memtest     = root.join("userspace/target/x86_64-unknown-none/release/memtest");
    let gen_memtest = std::path::Path::new(&out_dir).join("memtest_elf.rs");
    if memtest.exists() {
        let bytes = std::fs::read(&memtest).expect("read memtest ELF");
        let lit: String = bytes.iter().map(|b| format!("{},", b)).collect();
        std::fs::write(&gen_memtest, format!("static MEMTEST_ELF: &[u8] = &[{}];\n", lit)).unwrap();
        println!("cargo:rerun-if-changed={}", memtest.display());
    } else {
        std::fs::write(&gen_memtest, "static MEMTEST_ELF: &[u8] = &[];\n").unwrap();
    }

    // Bake the userspace inputtest ELF (proves out SYS_INPUT_STATE — Phase 1
    // of the desktop-to-userspace migration, see PLAN.md) the same way.
    let inputtest     = root.join("userspace/target/x86_64-unknown-none/release/inputtest");
    let gen_inputtest = std::path::Path::new(&out_dir).join("inputtest_elf.rs");
    if inputtest.exists() {
        let bytes = std::fs::read(&inputtest).expect("read inputtest ELF");
        let lit: String = bytes.iter().map(|b| format!("{},", b)).collect();
        std::fs::write(&gen_inputtest, format!("static INPUTTEST_ELF: &[u8] = &[{}];\n", lit)).unwrap();
        println!("cargo:rerun-if-changed={}", inputtest.display());
    } else {
        std::fs::write(&gen_inputtest, "static INPUTTEST_ELF: &[u8] = &[];\n").unwrap();
    }

    // Bake the userspace fstest ELF (proves out SYS_FS_LIST_DIR/READ_FILE/
    // WRITE_FILE/CREATE — Phase 1 of the desktop-to-userspace migration,
    // see PLAN.md) the same way.
    let fstest     = root.join("userspace/target/x86_64-unknown-none/release/fstest");
    let gen_fstest = std::path::Path::new(&out_dir).join("fstest_elf.rs");
    if fstest.exists() {
        let bytes = std::fs::read(&fstest).expect("read fstest ELF");
        let lit: String = bytes.iter().map(|b| format!("{},", b)).collect();
        std::fs::write(&gen_fstest, format!("static FSTEST_ELF: &[u8] = &[{}];\n", lit)).unwrap();
        println!("cargo:rerun-if-changed={}", fstest.display());
    } else {
        std::fs::write(&gen_fstest, "static FSTEST_ELF: &[u8] = &[];\n").unwrap();
    }

    // Bake the userspace svctest ELF (proves out SYS_SERVICE_CTL/POLL and
    // SYS_SPAWN_BYTES — Phase 1 item 4, the last of the desktop-to-userspace
    // migration's foundational syscalls, see PLAN.md) the same way.
    let svctest     = root.join("userspace/target/x86_64-unknown-none/release/svctest");
    let gen_svctest = std::path::Path::new(&out_dir).join("svctest_elf.rs");
    if svctest.exists() {
        let bytes = std::fs::read(&svctest).expect("read svctest ELF");
        let lit: String = bytes.iter().map(|b| format!("{},", b)).collect();
        std::fs::write(&gen_svctest, format!("static SVCTEST_ELF: &[u8] = &[{}];\n", lit)).unwrap();
        println!("cargo:rerun-if-changed={}", svctest.display());
    } else {
        std::fs::write(&gen_svctest, "static SVCTEST_ELF: &[u8] = &[];\n").unwrap();
    }
}
