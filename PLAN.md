# HepOS — Design Reference & Roadmap

> **Purpose:** Authoritative reference for HepOS. Survives context compaction.
> **Last updated:** 2026-07-02

---

## Overview

HepOS is a custom x86\_64 operating system written in Rust using an **exokernel architecture**. The kernel only does hardware multiplexing; all OS abstractions live in a kernel-space libOS for now. Single user, no permissions, networking partially implemented.

**Language:** Rust (nightly, `no_std` + `alloc`)  
**Target:** x86\_64, bare metal  
**Bootloader:** Limine v9.x (BIOS + UEFI)  
**Dev machine:** Windows 11, QEMU 11.x  
**License:** MIT  
**Repository:** https://github.com/The-Hep-Group/HepOS

---

## Source Files

```
kernel/
  build.rs       Emits linker script path via CARGO_MANIFEST_DIR (cross-platform)
  linker.ld      Custom linker script (Limine protocol sections)
  src/
    main.rs        kmain entry, global state, task_blink, window rendering, HepFS click handler
    framebuffer.rs GOP pixel/rect/text renderer — 8×8 bitmap font, double-buffered (backbuf flush)
    gdt.rs         GDT (null, code64, data64)
    idt.rs         IDT, exception stubs, timer_stub
    pmm.rs         Bitmap PMM (pages above 1MB only, alloc_contiguous)
    vmm.rs         HHDM offset, phys_to_virt
    paging.rs      PML4 walker, map_page, map_mmio (NOCACHE)
    heap.rs        Slab allocator — 10 size classes (8B–4KB), large allocs via PMM, full dealloc
    apic.rs        x2APIC (MSR), 10ms timer, disables 8259 PIC
    acpi.rs        ACPI shutdown (port 0x604) + PS/2 reboot
    rtc.rs         CMOS RTC: now(), fmt_time(), fmt_date()
    scheduler.rs   Round-robin preemptive, context_switch (naked asm)
    pci.rs         Config-space scan (0xCF8/0xCFC), enumerate()
    ps2.rs         PS/2 kbd: scancode set 1 + extended (0xE0) + shift/caps/ctrl/PgUp/PgDn
    mouse.rs       PS/2 AUX mouse (3-byte packets, relative, AUX port)
    xhci.rs        XHCI USB host controller — USB HID tablet, absolute mouse coords
    nvme.rs        NVMe driver, admin+IO queues, global CONTROLLER
    hepfs.rs       HepFS: flat inode, 4KB blocks, 12 direct + 1 indirect block per file
    desktop.rs     Compositor, WM, start menu, taskbar, resize handles, RTC clock
    terminal.rs    Full shell: history, left/right cursor, tab completion, 30+ commands
    editor.rs      Text editor: Ctrl+F find, PgUp/Dn, Ctrl+Home/End, F2=save, F10=close
    net.rs         ARP, ICMP, eth_send, ping (bypasses ARP, uses SLiRP MAC)
    e1000.rs       Intel 82540EM driver (TX works, RX pending)
    rtl8139.rs     RTL8139 driver (flat ring, TX works, RX broken on QEMU Windows)
    virtio_net.rs  virtio-net legacy (incomplete)
    syscall.rs     SYSCALL/SYSRET gate, SWAPGS, MSR setup, dispatcher (write/exit/getpid)
    process.rs     Ring-3 process: user PML4, ELF entry, process table (PID, state, exec, ps)
    elf.rs         ELF64 parser/loader — maps PT_LOAD segments into a user PML4
    serial.rs      COM1 debug: print, print_hex
    panic.rs       Prints file:line:message to serial, then spins

bootloader/
  limine.conf    Boot entry: timeout 0, loads /boot/hepos-kernel

limine/          Limine v9.x binary release (committed to repo)
  limine.exe     Windows installer tool
  limine.c       Installer source (compiled on Linux by build.sh via make)
  limine-bios.sys, limine-bios-cd.bin, limine-uefi-cd.bin, BOOTX64.EFI

build.ps1        Windows: build + ISO + QEMU launch
build.sh         Linux:   build + ISO + QEMU launch
```

---

## QEMU Command

```
qemu-system-x86_64
  -M q35
  -cpu qemu64,+x2apic      # x2APIC via MSR
  -m 256M
  -cdrom hepos.iso
  -boot d
  -drive file=hepos_disk.img,if=none,id=nvme0,format=raw
  -device nvme,serial=heposv1,drive=nvme0
  -netdev user,id=net0
  -device rtl8139,netdev=net0
  -device qemu-xhci,id=xhci
  -device usb-tablet,bus=xhci.0     # absolute mouse via USB HID
  -vga std
  -display sdl,window-close=off
  -serial stdio
  -no-reboot
  -no-shutdown
```

---

## Boot Sequence

```
Limine → kmain()
 1. serial, GDT, IDT
 2. VMM (HHDM offset), PMM (pages >1MB)
 3. Heap (bump, 256 PMM pages = 1MB)
 4. Display + splash screen
 5. Desktop + all windows created (ids 0-4, editor+sysmon minimized)
 6. Terminal init + HepFS navigator state
 7. PCI enumerate
 8. NVMe init → HepFS mount/format → write /kernel.txt
 9. Networking init (RTL8139 → e1000 fallback)
10. PS/2 keyboard + mouse init
11. XHCI USB init (finds usb-tablet, sets up HID ring)
12. Scheduler (2 tasks: idle, task_blink) + APIC timer   ← MUST be last
13. sti → first timer tick context-switches kmain → task_blink
14. task_blink loops forever (input poll + render)
```

**Critical:** APIC timer starts last. The first tick switches to task_blink; if APIC starts early, task_blink runs before XHCI/NVMe are ready.

---

## Focus System

- **Default:** Terminal focused (`FOCUSED_WIN = Some(2)`), all keys → terminal
- **Mouse click on window:** brings it to front AND syncs keyboard focus (`FOCUSED_WIN = Some(id)`)
- **Editor close (ESC / F10 / Ctrl+Q):** focus returns to terminal (`Some(2)`)
- **Ctrl+C in terminal:** cancel current input, show `^C`

Key routing in task_blink:
- `FOCUSED_WIN == Some(3)` → editor gets all keys
- anything else → terminal gets all keys

---

## Desktop Windows

| ID | Title | Default | Content |
|----|-------|---------|---------|
| 0 | Welcome to HepOS | open | System info, RAM, NVMe/HepFS status |
| 1 | HepFS | open | File manager: back/forward/path bar, directory navigation |
| 2 | Terminal | open | Full interactive shell |
| 3 | Editor | minimized | Text editor — opened by `edit <file>` or clicking a file in HepFS |
| 4 | Sysmon | minimized | RAM bar, uptime, PCI list, storage/net status |

All windows:
- **Drag** title bar to move
- **Drag** bottom-right corner handle to resize (min 120×60)
- **× button** minimizes to taskbar

---

## Taskbar & Start Menu

- **HepOS button** (left): popup listing ALL programs regardless of state; click to open/focus
- **Window buttons**: only non-minimized windows shown; click focused → minimize, click other → focus
- **Clock** (far right): live RTC time

---

## Terminal

### Key Bindings
| Key | Action |
|-----|--------|
| `←` / `→` | Move cursor within current input |
| `↑` / `↓` | History prev / next |
| `Ctrl+P` / `Ctrl+N` | History prev / next (alternative) |
| `Ctrl+A` / `Ctrl+E` | Jump to start / end of input |
| `Ctrl+C` | Cancel input |
| `Ctrl+L` / `Ctrl+K` | Clear screen |
| `Tab` | Complete command name or filename |
| `Backspace` | Delete char before cursor |

Terminal column count adapts to window width dynamically (up to 120 cols max).

### Commands
| Command | Description |
|---------|-------------|
| `help` | List all commands |
| `pwd` | Print working directory |
| `ls [path]` | List directory |
| `cd <dir>` | Change directory (`..` and `/` supported) |
| `cat <file>` | Print file contents |
| `mkdir <name>` | Create directory |
| `touch <name>` | Create empty file |
| `rm <name>` | Remove file or empty directory |
| `cp <src> <dst>` | Copy file |
| `mv <src> <dst>` | Move / rename file |
| `write <file> <text>` | Write text to file |
| `edit <file>` | Open text editor |
| `history` | Show command history |
| `date` | Current date + time (RTC) |
| `sysinfo` | Full kernel info |
| `uname` / `mem` | System / memory info |
| `lspci` | List all PCI devices |
| `ifconfig` | IP / MAC / gateway |
| `ping <ip>` | ICMP echo |
| `netstart` / `netdiag` / `netpoll` | NIC debug commands |
| `shutdown` / `reboot` | ACPI off / PS/2 reset |
| `echo` / `clear` | Print text / clear screen |
| `exec <file>` | Load and run ELF64 binary from HepFS |
| `ps` | List all processes (PID, state, name) |
| `runtest` | Run embedded ring-3 ELF sanity test |
| `newterm` | Spawn a new floating terminal window |

---

## Text Editor

| Key | Action |
|-----|--------|
| Arrow keys | Move cursor |
| `Home` / `End` | Line start / end |
| `Ctrl+Home` / `Ctrl+End` | File start / end |
| `PgUp` / `PgDn` | Scroll one screen |
| `Enter` | Insert newline |
| `Backspace` / `Delete` | Delete character |
| `Tab` | Insert 4 spaces |
| `Ctrl+F` | Enter find mode |
| (in find mode) type | Update search query live |
| (in find mode) `Enter` / `Ctrl+G` | Next match |
| (in find mode) `ESC` | Exit find mode |
| `F2` / `Ctrl+S` | Save |
| `F10` / `Ctrl+Q` | Close (warns if unsaved; second press force-closes) |

Find mode: highlights all matches (blue bg), current match (yellow bg), shows `[N/M]` count in status bar.

---

## HepFS File Manager

- **Nav bar:** `[<] [>] /current/path` — back / forward / path display
- **File list:** `d` (blue) = directory, `f` (white) = file, sizes on right
- `..` entry shown when not at root — click to go up
- Click directory → navigate in (pushes back history)
- Click file → open in editor

---

## HepFS Filesystem

```
Block 0      : Superblock  (magic 0x48657046_53000001)
Block 1      : Inode bitmap (32768 bits)
Blocks 2–5   : Block bitmap (131072 bits)
Blocks 6–37  : Inode table  (1024 inodes × 128 bytes each)
Blocks 38+   : Data blocks  (4KB each)
```

**Inode layout (128 bytes):**
- `flags` (file/dir/free), `size`, `nblocks`, `ctime`, `mtime`
- `direct[12]` — 12 × 4KB = 48KB direct
- `indirect` — points to a block of 1024 × u32 pointers → 1024 × 4KB = 4MB indirect
- **Max file size: ~4.1MB** (48KB + 4MB)

`/kernel.txt` written at every boot as a kernel manifest.

---

## XHCI USB Mouse Driver

**Device:** QEMU `qemu-xhci` (PCI 1B36:000D) + `usb-tablet` (absolute coordinates)

**USB HID report (6 bytes):** `[buttons] [x_lo] [x_hi] [y_lo] [y_hi] [wheel]`  
Range 0–32767 scaled to framebuffer size.

**Key gotchas:**
- Link TRB TC bit must be 1 on EVERY ring wrap (not just odd ones) — else ring desyncs after wrap 2
- Filter `(x=0, y=0, btn=0)` reports — garbage before QEMU window is focused
- Read port speed from PORTSC bits[13:10] after reset — don't hardcode USB2

---

## Networking

**Stack:** `net.rs` — hand-written Ethernet → ARP → IP → ICMP (no smoltcp)  
**Static config:** IP 10.0.2.15, GW 10.0.2.2, mask 255.255.255.0

| Driver | TX | RX | Notes |
|--------|----|----|-------|
| RTL8139 | ✓ | ✗ | SLiRP RX broken on QEMU/Windows |
| e1000 | ✓ | ✗ | Same issue |
| virtio-net | ✗ | ✗ | Not detected |

RX works on Linux/KVM — this is a QEMU Windows SLiRP path issue, not a driver bug.

---

## What's Built

### Kernel / Low-level
| ✓/○ | Feature |
|-----|---------|
| ✓ | Boot (Limine), Framebuffer, GDT, IDT |
| ✓ | PMM (bitmap, >1MB), HHDM, Paging |
| ✓ | Bump heap (1MB), GlobalAlloc |
| ✓ | x2APIC timer, ACPI shutdown/reboot, CMOS RTC |
| ✓ | Preemptive round-robin scheduler (naked-asm context switch) |
| ✓ | PCI config-space enumeration |
| ✓ | Serial debug (panic prints file:line:message) |
| ✓ | Cross-platform build (build.rs, build.sh, build.ps1) |
| ✓ | Slab allocator — 10 size classes (8B–4KB), large allocs via PMM, full dealloc |
| ✓ | Syscall gate — SYSCALL/SYSRET, SWAPGS, TSS RSP0, dispatcher (write/exit) |
| ✓ | GDT: ring-3 code+data segments, 64-bit TSS descriptor, ltr |
| ✓ | Per-process page tables — user PML4, ring-3 entry via IRETQ, exit longjmp |
| ✓ | ELF loader — ELF64 header/phdr parsing, PT_LOAD mapping, exec from HepFS |
| ✓ | Process table — PID tracking, running/exited state, `ps` command, `SYS_GETPID` |

### Drivers
| ✓/○ | Feature |
|-----|---------|
| ✓ | PS/2 keyboard — full scancode set 1, extended, all modifiers |
| ✓ | PS/2 mouse — relative with non-linear acceleration (1×/2×/3× by speed) |
| ✓ | XHCI USB host controller + USB HID tablet (absolute mouse) |
| ✓ | NVMe — admin + IO queues |
| ✓ | RTL8139 NIC — TX only |
| ✓ | e1000 NIC — TX only |
| ○ | Networking RX (works on Linux/KVM, broken on QEMU/Windows SLiRP) |
| ○ | Intel HDA audio |
| ○ | ACPI FADT parsing (for real hardware shutdown) |

### Storage
| ✓/○ | Feature |
|-----|---------|
| ✓ | HepFS: format, probe, create/read/write/delete files + dirs |
| ✓ | Path resolution (`/a/b/c`), kernel manifest `/kernel.txt` |
| ✓ | Indirect blocks — files up to ~4.1MB |
| ✓ | `cp`, `mv` terminal commands |
| ○ | Double-indirect blocks (files up to ~4GB) |
| ○ | VFS abstraction layer |

### Desktop / WM
| ✓/○ | Feature |
|-----|---------|
| ✓ | Floating compositor — correct z-order, chrome+content per window |
| ✓ | Double-buffered rendering — backbuf flush, no tearing or flicker |
| ✓ | Drag-to-move, drag-to-resize (bottom-right handle, min 120×60) |
| ✓ | Close button minimizes to taskbar |
| ✓ | Start menu (all programs) + taskbar (open windows only) + live clock |
| ✓ | Mouse click syncs visual + keyboard focus |
| ✓ | Context-sensitive cursor — crosshair normally; EW/NS/NESW/NWSE resize icons on all four edges/corners |
| ✓ | Full-edge resize — drag any edge or corner; left edge also moves the window |
| ✓ | Window maximize — double-click title bar or □ button; edge-drag snap to left/right half or full |
| ✓ | "+" button on terminal title bars — click to spawn a new floating terminal window |
| ○ | QEMU cursor on resize — USB tablet coords are logical (0–32767 → fb size) and should be unaffected by SDL window scale, but QEMU/Windows SDL sometimes breaks this when the window is dragged to a different size; workaround: don't resize the QEMU window, or use Ctrl+Alt+F for fullscreen |
| ○ | Desktop icons / wallpaper |
| ○ | Multiple instances of the same app |

### Apps
| ✓/○ | Feature |
|-----|---------|
| ✓ | Terminal — 30+ commands, history, left/right cursor, tab completion, dynamic width |
| ✓ | Text editor — Ctrl+F find, PgUp/Dn, Ctrl+Home/End, F2/F10 |
| ✓ | HepFS file manager — back/forward/path bar, click-to-navigate, click-to-open |
| ✓ | Welcome window — system info |
| ✓ | Sysmon window — RAM bar, uptime, PCI list, storage/net status |
| ✓ | Multiple terminal windows — `newterm` spawns additional floating terminals, each independently focusable |
| ○ | Image viewer |
| ○ | Audio player |
| ○ | Settings panel |

### Networking / Ecosystem
| ✓/○ | Feature |
|-----|---------|
| ✓ | ARP, ICMP, IP checksum, eth_send |
| ○ | Working RX (Linux/KVM only right now) |
| ○ | TCP / UDP stack |
| ○ | DNS, HTTP client |
| ✓ | Userspace — ring 3, SYSCALL/SYSRET, ELF loader, exec from HepFS, process table |
| ○ | `std` shim → unlock Rust crates |

---

## Known Issues

| Issue | Status |
|-------|--------|
| Network RX broken on QEMU/Windows | SLiRP path issue — TX works fine; RX works on Linux/KVM |
| NVMe size reported as 0 MB | Identify Namespace command hangs; workaround: hardcoded 512B/block |
| ACPI shutdown only on QEMU | Hardcoded port 0x604 — real hardware needs FADT parsing |
| Terminal text doesn't reflow on resize | Existing output stays at old column width; new input uses current width |

---

## Next Steps (Priority Order)

1. **`std` shim** — implement enough of `std` (alloc, io, fs stubs) so external Rust crates can link
2. **Preemptive multitasking** — scheduler integration for true concurrent processes (signal on exec, round-robin, wait/waitpid)
3. **Networking RX on Linux/KVM** — confirm RTL8139/e1000 RX works there; if yes, QEMU/Windows is a known environment issue not a bug
4. **Intel HDA audio** — PCI enumerate, CORB/RIRB setup, play PCM; pair with a beep command
5. **TCP/UDP stack** — build on existing ARP/IP layer; needed for any real networking app
6. ~~**Window maximize / snap**~~ ✓ done
7. ~~**Multiple terminal windows**~~ ✓ done
8. ~~**Full-edge resize + directional cursors**~~ ✓ done
9. ~~**"+" new terminal button**~~ ✓ done
10. **QEMU cursor on window resize** — `zoom-to-fit` not available on this QEMU/Windows build; known SDL limitation; no fix yet
11. **Desktop icons** — clickable icons on the desktop background for each app
9. **RTL8169 / real hardware NIC** — for running on physical machines

---

## Key Global State

```rust
// main.rs
pub static DISPLAY:        Mutex<Option<Display>>       // GOP framebuffer
pub static FOCUSED_WIN:    Mutex<Option<usize>>         // Some(id) = focused window
pub static PCI_DEVS:       Mutex<Vec<PciDevice>>        // populated at boot, used by lspci + sysmon
static     HEPFS_NAV:      Mutex<Option<HepfsNav>>      // HepFS navigator: ino, path, back[], fwd[]
static     UPTIME_FRAMES:  AtomicU64                    // incremented each frame (~60fps)

// Other modules
desktop::DESKTOP           Mutex<Option<Desktop>>       // WM state, windows, z-order, dirty flag
nvme::CONTROLLER           Mutex<Option<NvmeController>>
e1000::NIC / rtl8139::NIC  Mutex<Option<...>>
terminal::TERMINAL         Mutex<Option<Terminal>>
editor::EDITOR             Mutex<Option<Editor>>
scheduler::SCHEDULER       Mutex<Scheduler>
mouse::MOUSE               Mutex<Mouse>                 // x, y, buttons — written by XHCI + PS/2
process::CURRENT_PID       AtomicU32                    // PID of running user process (0 = none)
process::PROCTAB           Mutex<Vec<ProcEntry>>        // process history (max 32, oldest exited dropped)
```

```rust
struct HepfsNav {
    ino:  u32,                        // current directory inode
    path: String,                     // display path e.g. "/home"
    back: Vec<(u32, String)>,         // back navigation stack
    fwd:  Vec<(u32, String)>,         // forward navigation stack
}
```

---

## Render Loop (task_blink)

```
Each iteration (~16ms / 60fps):
  1. ps2::poll() + mouse::poll() + xhci::poll_mouse()  → updates mouse::MOUSE
  2. Keyboard routing:
       FOCUSED_WIN == Some(3)  → editor.on_key(c)
                                  if ed.open becomes false: minimize win 3, focus win 2
       anything else           → terminal.on_key(c)
  3. Clamp mouse coords to framebuffer bounds
  4. desktop::update_mouse(mx, my, btn)
       → drag, resize, taskbar clicks, start menu, close button
  5. On fresh left-click: sync FOCUSED_WIN ← desktop.focused
  6. HepFS click handler: nav bar (back/fwd), file list (enter dir / open file)
  7. If dirty:
       a. desktop.render()           — clear background
       b. for each window (bottom → top in z-order):
            draw_window()            — border, title bar, content bg
            render content           — welcome / hepfs / terminal / editor / sysmon
       c. draw_start_menu()          — popup if open
       d. draw_taskbar()             — always on top
       e. draw cursor                — shape depends on CursorType (Normal/EW/NS/NWSE/NESW)
  8. UPTIME_FRAMES += 1
  9. spin ~16ms
```

---

## Terminal Internals

```rust
const SCALE:           usize = 2;      // 2× font — each char is 19×18 px
const MAX_COLS:        usize = 120;    // cell array width (cells always allocated)
const DEFAULT_COLS:    usize = 30;     // initial cols before first render
const SCROLLBACK:      usize = 200;    // line history
// self.cols updated each frame from window width: (ww - 8) / CHAR_W
```

Lines stored as `[Cell; MAX_COLS]` — no per-line allocation. `self.cols` is updated every `render()` call from the actual window pixel width, so the terminal automatically uses more columns when the window is resized wider.

---

## Architecture Notes

- **PMM above 1MB only** — avoids VGA/BIOS hole 0xA0000–0xFFFFF
- **Slab allocator** — 10 size classes (8B–4KB), large allocs via PMM `alloc_contiguous`, full `dealloc` (push to free list or return page to PMM)
- **Scheduler starts last** — APIC timer fires → context switch kmain → task_blink. If started early, task_blink runs before NVMe/XHCI init
- **x2APIC via MSR** — xAPIC MMIO at 0xFEE00000 is outside Limine's HHDM; MSR mode avoids needing to map it
- **PS/2 poll order** — `ps2::poll()` before `mouse::poll()`; both read port 0x60; mouse bytes get eaten if order is wrong
- **XHCI ring wrap** — Link TRB TC must be 1 on every wrap. If only set on odd wraps, XHC stops toggling PCS and transfers freeze after wrap 2
- **Double-buffered rendering** — all drawing targets a PMM-backed backbuffer (`width×height u32`, ~3.5 MB at 1280×720); `flush()` copies each row to the physical framebuffer in one shot at the end of the frame, eliminating tearing and flicker
- **Z-order rendering** — chrome + content drawn together per window in z-order so a lower window's content can't overdraw a higher window's title bar
- **build.rs** — emits `-T<path>/linker.ld` via `cargo:rustc-link-arg` using `CARGO_MANIFEST_DIR`. Replaces the old hardcoded Windows absolute path in config.toml

---

## Dev Tips

- **Build + run:** `.\build.ps1` (Windows) or `./build.sh` (Linux)
- **Serial output** → the terminal that launched build.ps1/build.sh (panic messages appear here)
- **Mouse** → click any window to focus it for keyboard input
- **Tab** in terminal → complete command or filename
- **←/→** in terminal → move cursor within current input line
- **Resize window** → drag any edge or corner (cursor changes to show resize direction); left edge also moves the window
- **HepFS** → click `<`/`>` to navigate history; click a dir to enter; click `..` to go up
- **Ctrl+F** in editor → find mode; type query, `Enter`/`Ctrl+G` = next match, `ESC` = exit
- **Ctrl+L** in terminal → clear screen
- **F2** = save in editor; **F10** = close (warns unsaved, second press forces)
- **`lspci`** → full PCI device list with vendor:device IDs
- **`sysinfo`** → kernel details from inside the OS
- **Sysmon window** → open from start menu; shows live RAM bar + uptime + PCI list

---

## QEMU Hardware Reference

| Item | Value |
|------|-------|
| RAM | 256 MB |
| NVMe disk | 512 MB raw (`hepos_disk.img`) |
| NVMe BAR | 0xFEBD4000 |
| e1000 BAR | 0xFEBC0000 |
| e1000 MAC | 52:54:00:12:34:56 |
| SLiRP gateway | 10.0.2.2, MAC 52:55:0a:00:02:02 |
| Static IP | 10.0.2.15 / 255.255.255.0 |
| HHDM offset | 0xFFFF800000000000 (Limine default) |
| XHCI | PCI 1B36:000D, usb-tablet on xhci.0 |

---

## Crate Dependencies

```toml
limine = "0.6"   # Boot protocol structs — MIT
spin   = "0.9"   # Mutex without std — MIT
# core, alloc, compiler_builtins from rust-src (MIT/Apache-2)
```

All drivers, filesystem, networking, desktop, and apps written from scratch.
