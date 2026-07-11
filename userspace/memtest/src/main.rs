#![no_std]
#![no_main]
extern crate alloc;
extern crate hepos_rt; // pulls in global allocator + panic handler

// Proves out SYS_MMAP_ANON — the first of Phase 1's new foundational
// syscalls for the (eventual) desktop-to-userspace migration (see PLAN.md).
// Mirrors how `userspace/hwtest` proved SYS_MMAP_MMIO/SYS_PORT_IN/OUT before
// any real driver used them: request memory well beyond the ~256KB static
// bump heap `hepos-rt`'s allocator works from, write a distinctive pattern
// across the *entire* requested range, read it all back, and confirm every
// byte survived — the same rigor used to verify every disk driver's R/W
// path this session.

use hepos_std::println;

const REQUEST_LEN: u64 = 4 * 1024 * 1024; // 4 MB — far past the 256KB heap

#[no_mangle]
pub unsafe extern "C" fn _start() -> ! {
    println!("memtest: requesting {} bytes via SYS_MMAP_ANON", REQUEST_LEN);
    let va = hepos_rt::sys_mmap_anon(REQUEST_LEN);
    if va == 0 {
        println!("memtest: SYS_MMAP_ANON failed — exiting");
        hepos_rt::sys_exit(1);
    }
    println!("memtest: mapped at {:#x}", va);

    let ptr = va as *mut u8;
    let len = REQUEST_LEN as usize;

    // Distinctive pattern: byte at offset i = (i * 2654435761) as u8 —
    // cheap, deterministic, and not just a constant fill (catches
    // off-by-one/aliasing bugs a uniform fill would hide).
    for i in 0..len {
        core::ptr::write_volatile(ptr.add(i), (i as u32).wrapping_mul(2654435761) as u8);
    }

    let mut mismatches: u64 = 0;
    for i in 0..len {
        let expected = (i as u32).wrapping_mul(2654435761) as u8;
        let actual = core::ptr::read_volatile(ptr.add(i));
        if actual != expected { mismatches += 1; }
    }

    println!("memtest: pattern check — mismatches={} (0 = pass)", mismatches);
    if mismatches == 0 {
        println!("memtest: PASS");
        hepos_rt::sys_exit(0);
    } else {
        println!("memtest: FAIL");
        hepos_rt::sys_exit(1);
    }
}
