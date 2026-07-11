use core::arch::asm;
use spin::Mutex;

const DATA_PORT:   u16 = 0x60;
const STATUS_PORT: u16 = 0x64;
const CMD_PORT:    u16 = 0x64;

fn inb(port: u16) -> u8 {
    let v: u8;
    unsafe { asm!("in al, dx", out("al") v, in("dx") port, options(nomem, nostack)); }
    v
}
fn outb(port: u16, val: u8) {
    unsafe { asm!("out dx, al", in("dx") port, in("al") val, options(nomem, nostack)); }
}
fn wait_write() { while inb(STATUS_PORT) & 0x02 != 0 {} }
fn wait_read()  { while inb(STATUS_PORT) & 0x01 == 0 {} }

// Circular key event buffer (scancodes decoded to ASCII)
const BUF_SIZE: usize = 64;
struct KeyBuf {
    buf:  [u8; BUF_SIZE],
    head: usize,
    tail: usize,
}
impl KeyBuf {
    const fn new() -> Self { Self { buf: [0; BUF_SIZE], head: 0, tail: 0 } }
    fn push(&mut self, c: u8) {
        let next = (self.tail + 1) % BUF_SIZE;
        if next != self.head { self.buf[self.tail] = c; self.tail = next; }
    }
    pub fn pop(&mut self) -> Option<u8> {
        if self.head == self.tail { return None; }
        let c = self.buf[self.head];
        self.head = (self.head + 1) % BUF_SIZE;
        Some(c)
    }
}

static KEYBUF: Mutex<KeyBuf> = Mutex::new(KeyBuf::new());

// US QWERTY scancode set 1 → ASCII (make codes only, shift ignored for now)
static SCANCODE_MAP: [u8; 58] = [
    0,   0x1B, b'1', b'2', b'3', b'4', b'5', b'6',  // 0x00-0x07 (0x01=ESC→\x1b)
    b'7', b'8', b'9', b'0', b'-', b'=', b'\x08', b'\t', // 0x08-0x0F
    b'q', b'w', b'e', b'r', b't', b'y', b'u', b'i', // 0x10-0x17
    b'o', b'p', b'[', b']', b'\n', 0,   b'a', b's', // 0x18-0x1F
    b'd', b'f', b'g', b'h', b'j', b'k', b'l', b';', // 0x20-0x27
    b'\'', b'`', 0,   b'\\', b'z', b'x', b'c', b'v', // 0x28-0x2F
    b'b', b'n', b'm', b',', b'.', b'/', 0,   b'*',  // 0x30-0x37
    0,   b' ', // 0x38-0x39
];

// Extended scancode state (0xE0 prefix)
static EXTENDED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
static SHIFT: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
static CAPS: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
static CTRL: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

pub fn shift_held() -> bool { SHIFT.load(core::sync::atomic::Ordering::Relaxed) }
pub fn ctrl_held()  -> bool { CTRL .load(core::sync::atomic::Ordering::Relaxed) }

// Special key codes (stored as u8 > 127, read as char via `b as char`)
pub const KEY_UP:    u8 = 0x80;
pub const KEY_DOWN:  u8 = 0x81;
pub const KEY_LEFT:  u8 = 0x82;
pub const KEY_RIGHT: u8 = 0x83;
pub const KEY_DEL:   u8 = 0x84;
pub const KEY_HOME:  u8 = 0x85;
pub const KEY_END:   u8 = 0x86;
pub const KEY_PGUP:  u8 = 0x87;
pub const KEY_PGDN:  u8 = 0x88;
pub const KEY_F1:    u8 = 0x91;
pub const KEY_F2:    u8 = 0x92; // save in editor
pub const KEY_F5:    u8 = 0x95;
pub const KEY_F10:   u8 = 0x9A; // close in editor

// Shifted symbols for US QWERTY
static SHIFT_MAP: [u8; 58] = [
    0,   0x1B, b'!', b'@', b'#', b'$', b'%', b'^',
    b'&', b'*', b'(', b')', b'_', b'+', b'\x08', b'\t',
    b'Q', b'W', b'E', b'R', b'T', b'Y', b'U', b'I',
    b'O', b'P', b'{', b'}', b'\n', 0,   b'A', b'S',
    b'D', b'F', b'G', b'H', b'J', b'K', b'L', b':',
    b'"', b'~', 0,   b'|', b'Z', b'X', b'C', b'V',
    b'B', b'N', b'M', b'<', b'>', b'?', 0,   b'*',
    0,   b' ',
];

/// Called from keyboard interrupt handler (or polled).
pub fn handle_scancode(sc: u8) {
    use core::sync::atomic::Ordering::Relaxed;

    if sc == 0xE0 {
        EXTENDED.store(true, Relaxed);
        return;
    }
    let ext = EXTENDED.swap(false, Relaxed);

    let release = sc & 0x80 != 0;
    let base_sc = sc & 0x7F;

    // Handle modifier keys (both make and break)
    match base_sc {
        0x2A | 0x36 => { SHIFT.store(!release, Relaxed); return; } // L/R shift
        0x1D        => { CTRL .store(!release, Relaxed); return; } // Ctrl
        0x3A if !release => { // Caps lock toggle on press
            let c = CAPS.load(Relaxed);
            CAPS.store(!c, Relaxed);
            return;
        }
        _ => {}
    }

    if release { return; } // ignore all other key releases

    let ch: u8 = if ext {
        match base_sc {
            0x48 => KEY_UP,
            0x50 => KEY_DOWN,
            0x4B => KEY_LEFT,
            0x4D => KEY_RIGHT,
            0x53 => KEY_DEL,
            0x47 => KEY_HOME,
            0x4F => KEY_END,
            0x49 => KEY_PGUP,
            0x51 => KEY_PGDN,
            _ => 0,
        }
    } else {
        let sc = base_sc as usize;
        let shift = SHIFT.load(Relaxed);
        let caps  = CAPS .load(Relaxed);
        let ctrl  = CTRL .load(Relaxed);

        // F-keys (non-extended, scancodes 0x3B-0x44)
        if (0x3B..=0x44).contains(&base_sc) {
            let fkey = base_sc - 0x3A; // F1=1, F2=2, ..., F10=10
            let ch = 0x90 + fkey;
            if ch != 0 { KEYBUF.lock().push(ch); }
            return;
        }

        let sc = base_sc as usize;
        let raw = if shift && sc < SHIFT_MAP.len() { SHIFT_MAP[sc] }
                  else if sc < SCANCODE_MAP.len()  { SCANCODE_MAP[sc] }
                  else { 0 };

        // Caps lock inverts case for letters
        let raw = if caps && raw.is_ascii_lowercase() { raw - 32 }
                  else if caps && raw.is_ascii_uppercase() { raw + 32 }
                  else { raw };

        // Ctrl modifier: subtract 0x60 from lowercase letters → control codes
        if ctrl && raw.is_ascii_lowercase() { raw - 0x60 } else { raw }
    };

    if ch != 0 { KEYBUF.lock().push(ch); }
}

/// Non-blocking read — returns Some(char) if a key is available.
pub fn read_char() -> Option<char> {
    KEYBUF.lock().pop().map(|b| b as char)
}

/// Same as `read_char()`, but never spins waiting for `KEYBUF`'s own lock
/// either — returns `None` if the lock is currently held by someone else,
/// instead of busy-waiting for it. Needed by `SYS_INPUT_STATE`
/// (`syscall.rs`): a syscall handler runs with interrupts disabled the
/// whole time (SFMASK clears IF on `SYSCALL` entry), so if the *only* other
/// holder of this lock (`poll()`, called from `task_blink`) got preempted
/// mid-hold by the very timer interrupt that would otherwise reschedule it
/// back in, an ordinary `.lock()` here would spin forever with no way to
/// ever make progress — a real, previously-latent deadlock a plain `.lock()`
/// call in a syscall handler can hit that in-kernel code calling `.lock()`
/// from `task_blink`'s own single-threaded context never could (nothing
/// else ever contended these locks from a genuinely different scheduled
/// task before this syscall existed).
pub fn try_read_char() -> Option<char> {
    KEYBUF.try_lock().and_then(|mut g| g.pop()).map(|b| b as char)
}

/// Blocking read — spins until a key is available.
pub fn read_char_blocking() -> char {
    loop {
        if let Some(c) = read_char() { return c; }
        core::hint::spin_loop();
    }
}

pub fn init() {
    // Flush output buffer
    while inb(STATUS_PORT) & 0x01 != 0 { inb(DATA_PORT); }

    // Disable both PS/2 ports
    wait_write(); outb(CMD_PORT, 0xAD);
    wait_write(); outb(CMD_PORT, 0xA7);

    // Read and modify controller config byte
    wait_write(); outb(CMD_PORT, 0x20);
    wait_read();
    let mut config = inb(DATA_PORT);
    config &= !(1 << 0); // enable port 1 IRQ
    config &= !(1 << 6); // disable translation
    wait_write(); outb(CMD_PORT, 0x60);
    wait_write(); outb(DATA_PORT, config);

    // Enable port 1 (keyboard)
    wait_write(); outb(CMD_PORT, 0xAE);

    // Reset keyboard
    wait_write(); outb(DATA_PORT, 0xFF);
    wait_read();  inb(DATA_PORT); // ACK
    wait_read();  inb(DATA_PORT); // self-test result

    // Set scancode set 1
    wait_write(); outb(DATA_PORT, 0xF0);
    wait_read();  inb(DATA_PORT);
    wait_write(); outb(DATA_PORT, 0x01);
    wait_read();  inb(DATA_PORT);

    // Enable scanning
    wait_write(); outb(DATA_PORT, 0xF4);
    wait_read();  inb(DATA_PORT);
}

/// Drain ALL pending keyboard bytes (bit 5 = 0 in status).
/// Stops as soon as a mouse byte appears, leaving it for mouse::poll().
pub fn poll() {
    loop {
        let s = inb(STATUS_PORT);
        if s & 0x01 == 0 { break; } // nothing in buffer
        if s & 0x20 != 0 { break; } // next byte is mouse data — stop, don't consume
        handle_scancode(inb(DATA_PORT));
    }
}
