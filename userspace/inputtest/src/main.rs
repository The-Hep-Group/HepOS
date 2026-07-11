#![no_std]
#![no_main]
extern crate alloc;
extern crate hepos_rt; // pulls in global allocator + panic handler

// Proves out SYS_INPUT_STATE — Phase 1's second foundational syscall for
// the (eventual) desktop-to-userspace migration (see PLAN.md). Polls mouse
// position/buttons and drains pending keyboard chars once per timer tick
// (same SYS_WAIT_IRQ rate-limiting every persistent driver process already
// uses) and prints whenever something actually changes — run this while
// injecting synthetic input (QEMU monitor `mouse_move`/`mouse_button`, or
// real typing) to confirm it reflects real input, same verification
// approach used for the XHCI migration.
//
// Ignores updates where `mouse_valid == 0` (the kernel's mouse-state lock
// was momentarily busy — see `sys_input_state()`'s doc comment in
// kernel/src/syscall.rs for the real deadlock this avoids) rather than
// treating the zeroed fields as a real position change.

use hepos_std::println;
use hepos_rt::InputState;

const MAX_ITERS: u32 = 3000; // ~30s at the ~10ms SYS_WAIT_IRQ rate limit

#[no_mangle]
pub unsafe extern "C" fn _start() -> ! {
    println!("inputtest: starting — polling mouse/keyboard for ~30s");

    let mut last_x = i32::MIN;
    let mut last_y = i32::MIN;
    let mut last_buttons = u32::MAX;
    let mut total_keys: u32 = 0;
    let mut changes: u32 = 0;
    let mut lock_busy_count: u32 = 0;

    for _ in 0..MAX_ITERS {
        let mut state = InputState { mouse_valid: 0, mouse_x: 0, mouse_y: 0, mouse_buttons: 0, key_count: 0, keys: [0; 16] };
        hepos_rt::sys_input_state(&mut state);

        if state.mouse_valid == 0 {
            lock_busy_count += 1;
        } else if state.mouse_x != last_x || state.mouse_y != last_y || state.mouse_buttons != last_buttons {
            println!("inputtest: mouse x={} y={} buttons={:#x}", state.mouse_x, state.mouse_y, state.mouse_buttons);
            last_x = state.mouse_x;
            last_y = state.mouse_y;
            last_buttons = state.mouse_buttons;
            changes += 1;
        }

        if state.key_count > 0 {
            let n = state.key_count as usize;
            let chars = core::str::from_utf8(&state.keys[..n]).unwrap_or("?");
            println!("inputtest: keys={:?}", chars);
            total_keys += state.key_count;
        }

        hepos_rt::sys_wait_irq(0x20);
    }

    println!("inputtest: done — mouse_changes={} total_keys={} lock_busy_count={}", changes, total_keys, lock_busy_count);
    hepos_rt::sys_exit(0);
}
