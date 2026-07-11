#![no_std]
#![no_main]
extern crate alloc;
extern crate hepos_rt; // pulls in global allocator + panic handler

// Proves out SYS_SERVICE_CTL/SYS_SERVICE_POLL/SYS_SPAWN_BYTES — Phase 1 item
// 4 (the last one) of the desktop-to-userspace migration (see PLAN.md).
// service_id 0 = rtl8139d (an arbitrary but harmless pick — nothing else
// this OS does depends on the NIC being up during this test).

use hepos_std::println;

include!(concat!(env!("OUT_DIR"), "/hello_elf.rs"));

const EAGAIN: i64 = -11;
const EBUSY:  i64 = -16;
const MAX_POLLS: u32 = 500;
const RTL8139D: u64 = 0;
const ACTION_STATUS:  u64 = 0;
const ACTION_START:   u64 = 1;
const ACTION_STOP:    u64 = 2;
const ACTION_ENABLE:  u64 = 3;
const ACTION_DISABLE: u64 = 4;

fn poll_until_done() -> i64 {
    for _ in 0..MAX_POLLS {
        let r = hepos_rt::sys_service_poll();
        if r != EAGAIN && r != EBUSY { return r; }
        hepos_rt::sys_wait_irq(0x20);
    }
    EBUSY // gave up
}

fn is_running(status: i64) -> bool { status & 1 != 0 }
fn is_enabled(status: i64) -> bool { status & 2 != 0 }

#[no_mangle]
pub unsafe extern "C" fn _start() -> ! {
    println!("svctest: starting");
    let mut pass = true;

    // ── stop rtl8139d, confirm it actually stops ────────────────────────────
    let before = hepos_rt::sys_service_ctl(RTL8139D, ACTION_STATUS);
    println!("svctest: rtl8139d status before stop: running={} enabled={}", is_running(before), is_enabled(before));

    let submit = hepos_rt::sys_service_ctl(RTL8139D, ACTION_STOP);
    if submit != 0 {
        println!("svctest: stop submit FAILED errno={}", submit);
        pass = false;
    } else {
        let r = poll_until_done();
        if r != 0 {
            println!("svctest: stop FAILED errno={}", r);
            pass = false;
        } else {
            let after = hepos_rt::sys_service_ctl(RTL8139D, ACTION_STATUS);
            println!("svctest: rtl8139d status after stop: running={} enabled={}", is_running(after), is_enabled(after));
            if is_running(after) { println!("svctest: still running after stop!"); pass = false; }
        }
    }

    // ── start it back up, confirm it's running again ───────────────────────
    let submit = hepos_rt::sys_service_ctl(RTL8139D, ACTION_START);
    if submit != 0 {
        println!("svctest: start submit FAILED errno={}", submit);
        pass = false;
    } else {
        let r = poll_until_done();
        if r != 0 {
            println!("svctest: start FAILED errno={}", r);
            pass = false;
        } else {
            let after = hepos_rt::sys_service_ctl(RTL8139D, ACTION_STATUS);
            println!("svctest: rtl8139d status after start: running={} enabled={}", is_running(after), is_enabled(after));
            if !is_running(after) { println!("svctest: not running after start!"); pass = false; }
        }
    }

    // ── enable/disable are immediate — no polling ───────────────────────────
    hepos_rt::sys_service_ctl(RTL8139D, ACTION_DISABLE);
    let disabled = hepos_rt::sys_service_ctl(RTL8139D, ACTION_STATUS);
    hepos_rt::sys_service_ctl(RTL8139D, ACTION_ENABLE);
    let enabled = hepos_rt::sys_service_ctl(RTL8139D, ACTION_STATUS);
    println!("svctest: enabled bit toggled: disabled={} then enabled={}", is_enabled(disabled), is_enabled(enabled));
    if is_enabled(disabled) || !is_enabled(enabled) {
        println!("svctest: enable/disable didn't take effect");
        pass = false;
    }

    // ── spawn_bytes: run the already-compiled `hello` ELF from memory ──────
    if HELLO_ELF.is_empty() {
        println!("svctest: HELLO_ELF not embedded yet (userspace needs a second build pass) — skipping spawn test");
    } else {
        let r = hepos_rt::sys_spawn_bytes(HELLO_ELF, 0);
        println!("svctest: spawn_bytes(hello) -> {}", r);
        if r != 0 { pass = false; }
    }

    println!("svctest: {}", if pass { "PASS" } else { "FAIL" });
    hepos_rt::sys_exit(if pass { 0 } else { 1 });
}
