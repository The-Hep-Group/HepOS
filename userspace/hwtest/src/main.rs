#![no_std]
#![no_main]
extern crate alloc;
extern crate hepos_rt; // pulls in global allocator + panic handler

// Proof-of-concept "userspace driver": touches real hardware entirely
// through SYS_PORT_IN/OUT, SYS_MMAP_MMIO, and SYS_WAIT_IRQ — the
// foundational primitives for PLAN.md's "move drivers to userspace" item —
// without any direct kernel-side driver code involved. Launched via the
// terminal's `runhwtest` command, which runs this as its own background
// scheduler task (`process::exec_async()`) rather than blocking the
// desktop, so `sys_wait_irq` below genuinely blocks *this* process without
// freezing anything else. See PLAN.md for what this does and doesn't prove.

use hepos_std::println;

#[no_mangle]
pub unsafe extern "C" fn _start() -> ! {
    println!("hwtest: userspace hardware-access proof of concept");

    // Port I/O: read the RTC "seconds" register. Ports 0x70/0x71 are a fixed
    // ISA pair present on every x86 board, so this needs no PCI/BAR discovery
    // — just the raw port-IO syscall.
    hepos_rt::sys_port_out(0x70, 1, 0x00);
    let secs_bcd = hepos_rt::sys_port_in(0x71, 1);
    println!("RTC seconds (BCD) via SYS_PORT_IN: {:#04x}", secs_bcd);

    // MMIO: map the Local APIC — a fixed architectural physical address
    // (0xFEE00000 on every x86_64 machine, not board/BAR-dependent — kept
    // this way deliberately so the demo needs no PCI enumeration syscalls
    // yet) — and read its ID register at offset 0x20.
    let va = hepos_rt::sys_mmap_mmio(0xFEE0_0000, 0x1000);
    if va == 0 {
        println!("hwtest: SYS_MMAP_MMIO failed");
    } else {
        let id_reg = core::ptr::read_volatile((va + 0x20) as *const u32);
        println!("Local APIC ID via SYS_MMAP_MMIO: {}", id_reg >> 24);
    }

    // IRQ wait: block until the timer interrupt fires instead of
    // busy-polling for it — no real device IRQ exists to wait on yet (every
    // driver in this kernel still polls), but the timer is a real, already-
    // firing interrupt this can prove the mechanism against. Vector 0x20
    // matches the kernel's `apic::TIMER_VECTOR` (see `sys_wait_irq`'s doc
    // comment in hepos-rt for why it's hardcoded here instead of queried).
    println!("hwtest: waiting for a timer interrupt via SYS_WAIT_IRQ...");
    hepos_rt::sys_wait_irq(0x20);
    println!("hwtest: woke up from SYS_WAIT_IRQ");

    println!("hwtest: done");
    hepos_rt::sys_exit(0);
}
