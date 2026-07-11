#![no_std]
#![no_main]
extern crate alloc;
extern crate hepos_rt; // pulls in global allocator + panic handler

// Proves out SYS_FS_LIST_DIR/READ_FILE/WRITE_FILE/CREATE/POLL — Phase 1
// item 3 of the (eventual) desktop-to-userspace migration (see PLAN.md).
// Lists "/", reads "/kernel.txt", writes a scratch file, reads it back, and
// confirms a byte-exact round-trip — same rigor used to verify the
// AHCI/NVMe migrations' own R/W paths.
//
// These are async submit/poll syscalls, not one-shot blocking calls — see
// kernel/src/syscall.rs's "HepFS syscalls" doc comment for why (the real
// disk I/O runs through nvmed's mailbox, which needs the scheduler to make
// progress, impossible from inside an interrupt-disabled syscall). Every
// submit is followed by a poll loop that retries on EBUSY ("still working")
// or EAGAIN ("a lock was momentarily busy, try again") until it gets a real
// result, yielding via SYS_WAIT_IRQ between attempts so task_blink's own
// fs_service() gets a scheduling turn to actually drive the job forward.

use hepos_std::println;

const EAGAIN: i64 = -11;
const EBUSY:  i64 = -16;
const MAX_POLLS: u32 = 500;

fn poll_until_done(out: &mut [u8]) -> i64 {
    for _ in 0..MAX_POLLS {
        let r = hepos_rt::sys_fs_poll(out);
        if r != EAGAIN && r != EBUSY { return r; }
        hepos_rt::sys_wait_irq(0x20);
    }
    EBUSY // gave up
}

#[no_mangle]
pub unsafe extern "C" fn _start() -> ! {
    println!("fstest: starting");
    let mut pass = true;

    // ── list_dir("/") ──────────────────────────────────────────────────────
    let submit = hepos_rt::sys_fs_list_dir("/");
    if submit != 0 {
        println!("fstest: list_dir(\"/\") submit FAILED errno={}", submit);
        pass = false;
    } else {
        let mut buf = [0u8; 16 * core::mem::size_of::<hepos_rt::DirEntryOut>()];
        let n = poll_until_done(&mut buf);
        if n >= 0 {
            println!("fstest: list_dir(\"/\") -> {} entries", n);
            let entries = core::slice::from_raw_parts(buf.as_ptr() as *const hepos_rt::DirEntryOut, n as usize);
            for e in entries {
                let name = core::str::from_utf8(&e.name[..e.name_len as usize]).unwrap_or("?");
                println!("fstest:   ino={} name={}", e.ino, name);
            }
        } else {
            println!("fstest: list_dir(\"/\") FAILED errno={}", n);
            pass = false;
        }
    }

    // ── read_file("/kernel.txt") ────────────────────────────────────────────
    let submit = hepos_rt::sys_fs_read_file("/kernel.txt");
    if submit != 0 {
        println!("fstest: read_file(\"/kernel.txt\") submit FAILED errno={}", submit);
        pass = false;
    } else {
        let mut buf = [0u8; 512];
        let n = poll_until_done(&mut buf);
        if n >= 0 {
            let s = core::str::from_utf8(&buf[..n as usize]).unwrap_or("?");
            println!("fstest: read_file(\"/kernel.txt\") -> {} bytes: {:?}", n, s);
        } else {
            println!("fstest: read_file(\"/kernel.txt\") FAILED errno={}", n);
            pass = false;
        }
    }

    // ── write_file + read_file round-trip on a scratch file ────────────────
    let pattern: alloc::vec::Vec<u8> = (0..777u32).map(|i| (i.wrapping_mul(2654435761) >> 24) as u8).collect();
    let submit = hepos_rt::sys_fs_write_file("/fstest_scratch.bin", &pattern);
    if submit != 0 {
        println!("fstest: write_file scratch submit FAILED errno={}", submit);
        pass = false;
    } else {
        let mut buf = [0u8; 4];
        let n = poll_until_done(&mut buf);
        if n == pattern.len() as i64 {
            println!("fstest: write_file scratch -> {} bytes written", n);
        } else {
            println!("fstest: write_file scratch FAILED/short: {}", n);
            pass = false;
        }
    }

    let submit = hepos_rt::sys_fs_read_file("/fstest_scratch.bin");
    let matches = if submit != 0 {
        println!("fstest: read_file scratch submit FAILED errno={}", submit);
        false
    } else {
        let mut readback = [0u8; 1024];
        let n = poll_until_done(&mut readback);
        if n == pattern.len() as i64 {
            &readback[..n as usize] == pattern.as_slice()
        } else {
            println!("fstest: read_file scratch FAILED/short: {}", n);
            false
        }
    };
    println!("fstest: scratch round-trip data_matches={}", matches);
    if !matches { pass = false; }

    println!("fstest: {}", if pass { "PASS" } else { "FAIL" });
    hepos_rt::sys_exit(if pass { 0 } else { 1 });
}
