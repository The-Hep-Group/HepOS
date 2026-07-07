use core::arch::asm;
use crate::vmm;

fn outw(port: u16, val: u16) {
    unsafe { asm!("out dx, ax", in("dx") port, in("ax") val, options(nomem, nostack)); }
}
fn outb(port: u16, val: u8) {
    unsafe { asm!("out dx, al", in("dx") port, in("al") val, options(nomem, nostack)); }
}

// ── Real ACPI shutdown (FADT PM1a/b_CNT_BLK + a DSDT byte-scan for \_S5) ──────
//
// This is the well-known hobbyist recipe (see OSDev wiki's "Shutdown" page)
// rather than a real AML interpreter — a full AML interpreter is a huge
// undertaking (real OSes use ACPICA, tens of thousands of lines) genuinely
// out of scope here. The byte-scan just looks for the literal `_S5_` name in
// the DSDT and reads the two SLP_TYP values out of the Package that follows
// it, which is reliable in practice because `\_S5` is always encoded as a
// small literal package (PackageOp, then 1-2 small integers) — never
// anything requiring real control-flow evaluation.
//
// Every step here fails soft (returns None) instead of panicking or trusting
// unbounded data: this runs once at boot, and a malformed/absent/nonstandard
// ACPI table set must never risk becoming a hang — same lesson as this
// project's other boot-time driver bugs (see PLAN.md Known Issues). On any
// failure, `shutdown()` falls back to the hardcoded QEMU/Bochs/VirtualBox
// ports that already worked before this existed.

#[derive(Clone, Copy)]
struct S5Shutdown { pm1a_cnt: u16, pm1b_cnt: u16, slp_typa: u16, slp_typb: u16 }

fn checksum_ok(bytes: &[u8]) -> bool {
    bytes.iter().fold(0u8, |acc, &b| acc.wrapping_add(b)) == 0
}

unsafe fn read_bytes(phys: u64, len: usize) -> &'static [u8] {
    core::slice::from_raw_parts(vmm::phys_to_virt(phys), len)
}

/// Walks RSDT/XSDT entries looking for a table with the given 4-byte
/// signature (e.g. `b"FACP"`). Returns its physical address, or `None`.
unsafe fn find_table(sdt_phys: u64, is_xsdt: bool, signature: &[u8; 4]) -> Option<u64> {
    let hdr = read_bytes(sdt_phys, 36);
    let length = u32::from_le_bytes(hdr[4..8].try_into().ok()?) as usize;
    if length < 36 || length > 1 << 20 { return None; } // sanity bound — real tables are small
    let full = read_bytes(sdt_phys, length);
    if !checksum_ok(full) { return None; }

    let entry_size = if is_xsdt { 8 } else { 4 };
    let entries_bytes = &full[36..];
    let n = entries_bytes.len() / entry_size;
    for i in 0..n {
        let entry_phys = if is_xsdt {
            u64::from_le_bytes(entries_bytes[i*8..i*8+8].try_into().ok()?)
        } else {
            u32::from_le_bytes(entries_bytes[i*4..i*4+4].try_into().ok()?) as u64
        };
        if entry_phys == 0 { continue; }
        let sig = read_bytes(entry_phys, 4);
        if sig == signature { return Some(entry_phys); }
    }
    None
}

/// PkgLength encoding (ACPI AML spec §20.2.4): returns (value, bytes_consumed).
fn parse_pkg_length(data: &[u8]) -> Option<(u32, usize)> {
    let lead = *data.first()?;
    let follow_count = (lead >> 6) as usize;
    if follow_count == 0 {
        return Some(((lead & 0x3F) as u32, 1));
    }
    if data.len() < 1 + follow_count { return None; }
    let mut val = (lead & 0x0F) as u32;
    for i in 0..follow_count {
        val |= (data[1 + i] as u32) << (4 + 8 * i);
    }
    Some((val, 1 + follow_count))
}

/// Scans a DSDT/SSDT's AML bytes for the `_S5_` package and extracts
/// SLP_TYPa/SLP_TYPb — the two small integers a real AML interpreter would
/// get by evaluating the `\_S5` object. See the module doc above for why a
/// byte-scan instead of real AML evaluation.
fn find_s5_slp_types(aml: &[u8]) -> Option<(u16, u16)> {
    let needle = b"_S5_";
    let pos = aml.windows(4).position(|w| w == needle)?;
    let mut i = pos + 4;
    if i >= aml.len() || aml[i] != 0x12 { return None; } // PackageOp
    i += 1;
    let (_pkg_len, consumed) = parse_pkg_length(aml.get(i..)?)?;
    i += consumed;
    if i >= aml.len() { return None; }
    i += 1; // NumElements byte

    let read_small_int = |data: &[u8], i: &mut usize| -> Option<u16> {
        let b = *data.get(*i)?;
        if b == 0x0A { // BytePrefix — next byte is the actual value
            *i += 1;
            let v = *data.get(*i)? as u16;
            *i += 1;
            Some(v)
        } else if b == 0x00 { *i += 1; Some(0) } // ZeroOp
        else if b == 0x01 { *i += 1; Some(1) }   // OneOp
        else if b < 0x40 { *i += 1; Some(b as u16) } // raw small literal
        else { None }
    };

    let a = read_small_int(aml, &mut i)?;
    let b = read_small_int(aml, &mut i)?;
    Some((a, b))
}

/// Parses RSDP → RSDT/XSDT → FADT → DSDT → `_S5_` package, entirely from
/// `rsdp_phys` (passed through from HepBL via `BootInfo::acpi_rsdp`).
/// Returns `None` on any validation failure — caller falls back to the
/// hardcoded QEMU-only shutdown ports.
fn find_s5(rsdp_phys: u64) -> Option<S5Shutdown> {
    if rsdp_phys == 0 { return None; }
    unsafe {
        let rsdp = read_bytes(rsdp_phys, 20);
        if &rsdp[0..8] != b"RSD PTR " { return None; }
        let revision = rsdp[15];

        let (sdt_phys, is_xsdt) = if revision >= 2 {
            let rsdp_full = read_bytes(rsdp_phys, 36);
            if !checksum_ok(rsdp_full) { return None; }
            let xsdt = u64::from_le_bytes(rsdp_full[24..32].try_into().ok()?);
            (xsdt, true)
        } else {
            if !checksum_ok(rsdp) { return None; }
            let rsdt = u32::from_le_bytes(rsdp[16..20].try_into().ok()?) as u64;
            (rsdt, false)
        };
        if sdt_phys == 0 { return None; }

        let fadt_phys = find_table(sdt_phys, is_xsdt, b"FACP")?;
        let fadt_hdr = read_bytes(fadt_phys, 36);
        let fadt_len = u32::from_le_bytes(fadt_hdr[4..8].try_into().ok()?) as usize;
        if fadt_len < 76 || fadt_len > 4096 { return None; }
        let fadt = read_bytes(fadt_phys, fadt_len);
        if !checksum_ok(fadt) { return None; }

        let pm1a_cnt = u32::from_le_bytes(fadt[64..68].try_into().ok()?) as u16;
        let pm1b_cnt = u32::from_le_bytes(fadt[68..72].try_into().ok()?) as u16;

        // Prefer X_DSDT (u64 @ offset 140, ACPI 2.0+) if the table is long
        // enough and it's non-zero; otherwise fall back to DSDT (u32 @ 40).
        let dsdt_phys = if fadt_len >= 148 {
            let x = u64::from_le_bytes(fadt[140..148].try_into().ok()?);
            if x != 0 { x } else { u32::from_le_bytes(fadt[40..44].try_into().ok()?) as u64 }
        } else {
            u32::from_le_bytes(fadt[40..44].try_into().ok()?) as u64
        };
        if dsdt_phys == 0 { return None; }

        let dsdt_hdr = read_bytes(dsdt_phys, 36);
        let dsdt_len = u32::from_le_bytes(dsdt_hdr[4..8].try_into().ok()?) as usize;
        if dsdt_len < 36 || dsdt_len > 1 << 20 { return None; }
        let dsdt = read_bytes(dsdt_phys, dsdt_len);
        if !checksum_ok(dsdt) { return None; }

        let (slp_typa, slp_typb) = find_s5_slp_types(&dsdt[36..])?;
        Some(S5Shutdown { pm1a_cnt, pm1b_cnt, slp_typa, slp_typb })
    }
}

const SLP_EN: u16 = 1 << 13;

/// Shut down the machine via ACPI.
/// Tries real ACPI first (RSDP → FADT → DSDT `_S5` byte-scan, see above);
/// falls back to the hardcoded QEMU/Bochs/VirtualBox ports this project used
/// before real parsing existed, so behavior on QEMU is unchanged either way.
pub fn shutdown() -> ! {
    let rsdp = crate::BOOTINFO_ACPI_RSDP.load(core::sync::atomic::Ordering::Relaxed);
    if let Some(s5) = find_s5(rsdp) {
        outw(s5.pm1a_cnt, s5.slp_typa | SLP_EN);
        if s5.pm1b_cnt != 0 {
            outw(s5.pm1b_cnt, s5.slp_typb | SLP_EN);
        }
    }
    // Fallback ports — also acts as a safety net if the real ACPI path above
    // silently didn't actually power the machine off (e.g. a firmware that
    // doesn't honor the write for some reason).
    outw(0x604, 0x2000); // QEMU ACPI shutdown
    outw(0xB004, 0x2000); // older QEMU / Bochs fallback
    outw(0x4004, 0x3400); // VirtualBox fallback
    loop { unsafe { asm!("hlt", options(nomem, nostack)); } }
}

/// Reboot via PS/2 controller reset line.
pub fn reboot() -> ! {
    // Pulse reset line via PS/2 controller
    let mut good = 0x02u8;
    while good & 0x02 != 0 {
        good = unsafe {
            let v: u8;
            asm!("in al, dx", out("al") v, in("dx") 0x64u16, options(nomem, nostack));
            v
        };
    }
    outb(0x64, 0xFE);
    loop { unsafe { asm!("hlt", options(nomem, nostack)); } }
}
