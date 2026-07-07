# HepOS — Design Reference & Roadmap

> **Purpose:** Authoritative reference for HepOS. Survives context compaction.
> **Last updated:** 2026-07-06

---

## Overview

HepOS is a custom x86\_64 operating system written in Rust using an **exokernel architecture**. The kernel only does hardware multiplexing; all OS abstractions live in a kernel-space libOS for now. Single user, no permissions, networking partially implemented.

**Language:** Rust (nightly, `no_std` + `alloc`)  
**Target:** x86\_64, bare metal  
**Bootloader:** HepBL v0.1 — our own, written from scratch in Rust (UEFI)  
**Dev machine:** Windows 11, QEMU 11.x  
**License:** MIT  
**Repository:** https://github.com/The-Hep-Group/HepOS

---

## Original Design Plan vs. Current Reality

The project started from a design doc with a specific target shape (libOS-in-userspace, wide file-format support, animated desktop, IDE-grade editor, etc). This section is an honest audit of that plan against what's actually implemented today — updated 2026-07-06. ✅ = done, 🟡 = partial/deviates, ❌ = not implemented.

**Kernel: Exokernel (Rust, x86\_64)** — ✅ done.

**LibOS layer**
| Item | Status | Notes |
|---|---|---|
| Memory allocator | ✅ | Slab allocator (`heap.rs`) |
| Scheduler (round-robin) | 🟡 | Structure exists (`scheduler.rs`), but **real preemption is currently broken** — traced to a bug in the interrupt/context-switch mechanism (EOI ordering + missing `sti` on fresh tasks + a resume-time interrupt-frame corruption once a task that was genuinely interrupted gets resumed via `iretq`). Attempted a full fix in one session; found and fixed two of the three bugs, but the third (GPF on resume) wasn't safely resolved, so **all the fixes were reverted** rather than ship a kernel that crashes on its first natural preemption. `task_blink` runs the whole OS today effectively as a single always-scheduled task. |
| Custom filesystem supporting SATA **and** NVMe | 🟡 | HepFS only works over **NVMe**. No AHCI/SATA driver exists at all. |
| Thin `std` shim (e.g. so Symphonia links unmodified) | ❌ | `hepos-std` only re-exports `println!`/`String`/`Vec` for a tiny demo binary — nowhere near enough surface (`std::io`, error traits, etc.) for an unmodified real-world crate to link. |

**Drivers (plan: "all in libOS userspace except where HW forces kernel")**
| Item | Status | Notes |
|---|---|---|
| XHCI → USB HID (keyboard/mouse) | 🟡 | XHCI drives a USB HID **mouse** only (tablet). Keyboard is PS/2, not USB HID. |
| Intel HDA (audio out) | ✅ | Non-blocking playback (`hda::play_pcm()` + `poll()`) |
| NVMe (storage) | ✅ | |
| PCI enumeration | ✅ | |
| ACPI (shutdown/reboot) | 🟡 | QEMU-only (hardcoded port 0x604); no real FADT parsing for physical hardware |
| GOP framebuffer (display) | ✅ | Via HepBL's BootInfo |
| **Drivers live in userspace libOS** | ❌ | **Not true.** Every driver (XHCI, HDA, NVMe, PCI, ACPI, GOP) runs in the kernel (ring 0). `userspace/` only contains `hepos-rt`/`hepos-std`/the `hello` demo — no actual drivers have been moved out. This is the single biggest architectural gap versus the original plan. |

**Custom Filesystem (HepFS)**
| Item | Status | Notes |
|---|---|---|
| Flat inode table | ✅ | |
| No permissions | ✅ | |
| Files + directories | ✅ | |
| Large blocks, designed for NVMe | ✅ | 4KB blocks |
| No journaling | ✅ | Matches the plan's "keep it simple, maybe add later" |
| SATA support | ❌ | NVMe only |

**File Format Support** — the biggest gap versus the plan; almost nothing beyond TXT/WAV exists, and the thin `std` shim that would unlock real codec crates isn't there either.
| Format | Status |
|---|---|
| TXT | ✅ |
| MD (rendered) | ❌ editor is plain text, no markdown rendering |
| PNG, JPG | ❌ image viewer only decodes uncompressed BMP |
| WAV | ✅ 16-bit PCM |
| MP3, FLAC, OGG | ❌ no codecs |
| MP4/H.264 | ❌ no video support at all |
| PDF | ❌ |
| ZIP, TAR | ❌ |

**Desktop Environment**
| Item | Status | Notes |
|---|---|---|
| Software rendered, GOP, 60fps | ✅ | |
| Dirty rect tracking + double buffer | 🟡 | Close but not literal per-widget dirty rects — a two-tier scheme instead (full-scene redraw on any dirty flag vs. a ~20-row partial flush for cursor-only movement). Same goal, different mechanism. |
| Floating WM, opaque windows, flat design | ✅ | |
| Dark color scheme hardcoded | 🟡 | Dark by default, but Settings now lets you switch to a Bliss-style wallpaper (blue sky/green hills) — window chrome stays dark, background is no longer strictly one hardcoded scheme |
| Simple 150–200ms ease-out animations (open/close/minimize) | ❌ | Not implemented — windows snap instantly, no easing |
| Taskbar: open apps, clock, volume control | 🟡 | Apps + clock yes; **no volume control** anywhere (HDA has no adjustable gain/UI) |
| Desktop icons | ✅ | |

**Programs**
| Program | Status | Notes |
|---|---|---|
| Terminal emulator (VT100 subset) | ✅ | 30+ commands |
| File manager, **two-pane** | 🟡 | Single-pane with back/forward nav, not two-pane |
| Text editor | ✅ | |
| IDE (syntax highlighting, Rust/C) | ✅ | Basic keyword/string/comment/number highlighting added — see Text Editor section below |
| Image viewer | 🟡 | Exists, BMP-only |
| Audio/video player | 🟡 | Audio yes (WAV only); **no video player at all** |
| PDF/MD viewer | ❌ | Neither implemented |
| Settings (volume, resolution) | 🟡 | Settings app exists but only has a wallpaper picker — no volume or resolution control |

---

## Source Files

```
kernel/
  build.rs       Emits linker script path via CARGO_MANIFEST_DIR (cross-platform)
  linker.ld      Custom linker script (higher-half at 0xffffffff80000000, ENTRY(kmain))
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
    editor.rs      Text editor: Ctrl+F find, PgUp/Dn, Ctrl+Home/End, F2=save, F10=close, basic Rust/C syntax highlighting
    net.rs         ARP, ICMP, IP, TCP (hand-written stack), HTTP GET client (wget), ping
    e1000.rs       Intel 82540EM driver (TX works, RX pending)
    rtl8139.rs     RTL8139 driver (flat ring, TX works, RX broken on QEMU Windows)
    virtio_net.rs  virtio-net legacy (incomplete)
    syscall.rs     SYSCALL/SYSRET gate, SWAPGS, MSR setup, dispatcher (write/exit/getpid)
    process.rs     Ring-3 process: user PML4, ELF entry, process table (PID, state, exec, ps)
    elf.rs         ELF64 parser/loader — maps PT_LOAD segments into a user PML4
    hda.rs         Intel HDA driver: PCI detect, BAR0 map, immediate-cmd codec config, stream + BDL, beep()
    image.rs       BMP decoder (24/32-bit, BI_RGB) + image viewer window; `view <file>` command
    audio.rs       WAV decoder (16-bit PCM, 48kHz) + playback via hda::play_pcm(); `play <file>` command
    serial.rs      COM1 debug: print, print_hex
    panic.rs       Prints file:line:message to serial, then spins

userspace/             Rust userspace workspace (builds before kernel; output baked into kernel via build.rs)
  hepos-rt/        Ring-3 runtime: bump allocator (#[global_allocator]), panic handler, sys_write/exit/getpid
  hepos-std/       std facade: re-exports alloc types + println!/print! macros backed by sys_write
  hello/           Demo binary: exercises String, Vec, println!, sys_getpid — runs via `runhello`

hepbl/           HepBL — our own UEFI bootloader, written from scratch (replaced Limine)
  src/main.rs    Pure Rust UEFI app: hand-written UEFI FFI (no external crates), GOP mode
                 select, loads \kernel.elf from boot volume, ELF64 loader, builds page
                 tables (identity + HHDM 0..4GiB in 4K pages + kernel high-half),
                 ExitBootServices, asm handoff (CR3/RSP/jmp, RDI = &BootInfo)

kernel/src/bootinfo.rs   HepBL boot protocol structs (BootInfo, MemRegion) — kept in
                         sync with hepbl/src/main.rs

esp/             Boot volume dir — QEMU exposes as FAT disk (VVFAT): EFI/BOOT/BOOTX64.EFI + kernel.elf
build.ps1        Windows: build userspace + kernel + HepBL, assemble ESP, QEMU launch (OVMF)
build.sh         Linux:   same
```

---

## QEMU Command

```
qemu-system-x86_64
  -M q35
  -cpu qemu64,+x2apic      # x2APIC via MSR
  -m 256M
  -drive if=pflash,format=raw,readonly=on,file=edk2-x86_64-code.fd   # OVMF UEFI firmware
  -drive if=pflash,format=raw,file=hepbl_vars.fd                     # writable NVRAM
  -drive format=raw,file=fat:rw:esp                                  # boot volume (VVFAT)
  -fw_cfg name=opt/ovmf/X-PciMmio64Mb,string=0   # keep PCI BARs below 4GiB
  -drive file=hepos_disk.img,if=none,id=nvme0,format=raw
  -device nvme,serial=heposv1,drive=nvme0
  -netdev user,id=net0
  -device rtl8139,netdev=net0
  -device intel-hda
  -device hda-duplex
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

**OVMF** = the QEMU-targeted build of **TianoCore/EDK2** (the open-source reference UEFI firmware). It's what runs before HepBL — POSTs the virtual hardware, sets up UEFI boot/runtime services, switches to long mode, then loads `\EFI\BOOT\BOOTX64.EFI` (HepBL) off the ESP. TianoCore's job ends the moment `ExitBootServices` succeeds inside HepBL.

```
OVMF (TianoCore/EDK2 UEFI firmware) → HepBL (\EFI\BOOT\BOOTX64.EFI)
 a. GOP mode select (prefers 1280x800 / 1024x768)
 b. Load + parse \kernel.elf (ELF64, PT_LOAD → allocated pages)
 c. Page tables: identity 0..4GiB (transitional) + HHDM at 0xffff800000000000
    (4K pages so kernel's map_page/map_mmio can walk them) + kernel high-half
 d. GetMemoryMap → ExitBootServices → asm handoff (CR3, RSP, jmp kmain, RDI=&BootInfo)

kmain(BootInfo)
 1. serial, magic check, GDT, IDT
 2. VMM (HHDM offset from BootInfo), PMM (usable regions >1MB), clear identity PML4[0]
 3. Heap (bump, 256 PMM pages = 1MB)
 4. Display + splash screen
 5. Desktop + all windows created (ids 0-4, editor+sysmon minimized)
 6. Terminal init + HepFS navigator state
 7. PCI enumerate
 8. NVMe init → HepFS mount/format → write /kernel.txt
 9. Intel HDA init: PIT-calibrates TSC frequency (used by beep() for timing), then initialises controller
10. Networking init (RTL8139 → e1000 fallback)
11. PS/2 keyboard + mouse init
12. XHCI USB init (finds usb-tablet, sets up HID ring)
13. Scheduler (2 tasks: idle, task_blink) + APIC timer   ← MUST be last
14. sti → first timer tick context-switches kmain → task_blink
15. task_blink loops forever (input poll + render)
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
| 5 | Settings | minimized | Background picker (dark gradient / Bliss) |
| 6 | Image Viewer | minimized | Decoded BMP, opened via `view <file>` or clicking a `.bmp` in HepFS |
| 7 | Audio Player | minimized | Last-played WAV: path, format, duration/error — opened via `play <file>` or clicking a `.wav` in HepFS |

All windows:
- **Drag** title bar to move
- **Drag** bottom-right corner handle to resize (min 120×60)
- **× button** minimizes to taskbar

---

## Taskbar & Start Menu

- **HepOS button** (left): popup listing ALL programs regardless of state; click to open/focus
- **Window buttons**: only non-minimized windows shown, grouped by app kind (see below); click focused → minimize, click other → focus; group with >1 window opens a jump list instead
- **Right-click** a taskbar button or a Start Menu row → "New Window" (spawns another instance of that program; not offered for Files)
- **Clock** (far right): live RTC time

---

## Multi-instance windowing

Every `Window` carries an `app_kind: desktop::AppKind` tag (`Welcome`, `Files`, `Terminal`, `Editor`, `Sysmon`, `Settings`, `ImageViewer`, `AudioPlayer`). This tag drives four things:

1. **Render dispatch** — `main.rs`'s window-render loop matches on `app_kind`, not raw window id. For kinds with per-instance state (`Terminal`, `Editor`, `ImageViewer`, `Files`), the *main* window (fixed id 2/3/6/1) reads its dedicated static or its own entry in a per-window collection; any other window of that kind is looked up by id in an `EXTRA_*`/`*_NAVS` list — all `Mutex<Vec<(usize, T)>>`, same pattern, keyed by window id: `terminal::EXTRA_TERMINALS`, `editor::EXTRA_EDITORS`, `image::EXTRA_VIEWERS`, `main::HEPFS_NAVS` (Files — every Files window, including id=1, has its own entry; there's no separate "main singleton" special case here since HEPFS_NAVS holds all of them uniformly). Kinds with **no** per-instance state (`Welcome`, `Sysmon`, `Settings`, `AudioPlayer`) render identically no matter which window shows them or how many are open — a "new instance" is just another window pointed at the same stateless render function.
2. **Taskbar grouping** — `Desktop::grouped_taskbar_entries()` buckets non-minimized windows by `app_kind`; the taskbar shows one button per bucket, "(N)" suffixed if N>1.
3. **Start Menu grouping** — `Desktop::grouped_all_entries()` does the same but over *all* windows regardless of minimized state (Start Menu lists every program, not just open ones); shows a "--" badge only when no instance of that kind is currently open.
4. Clicking a bucket (taskbar or Start Menu) with exactly one window behaves like before (focus/minimize toggle, or unminimize+focus from Start Menu); clicking one with >1 opens `taskbar_jumplist: Option<(AppKind, popup_x)>`, a popup listing each instance — numbered ("Terminal 1", "Terminal 2", ...) if the window's title is still the generic app label, or the real title if it already differentiates (e.g. "Editor: foo.txt"). The jump list is built from `grouped_all_entries()` (not the taskbar-only grouping) so it still works when every instance happens to be minimized and it's opened from the Start Menu.
5. **"New Window" spawning** — right-click on a taskbar button or Start Menu row sets `Desktop::new_window_requested: Option<AppKind>` (consumed once per frame in `main.rs`, same pattern as `open_settings_requested`), which dispatches to:
   - `Terminal` → `terminal::spawn_terminal()`
   - `Editor` → `editor::spawn_editor_blank()` (blank buffer, path defaults to `/untitled.txt` for Ctrl+S)
   - `ImageViewer` → `image::spawn_viewer_blank()` (empty until a `view` command targets it)
   - `Files` → `main::spawn_files()` (new window + a fresh `HepfsNav` entry starting at root)
   - `Welcome`/`Sysmon`/`Settings`/`AudioPlayer` → `spawn_stateless_window()` in main.rs (just a new window, no per-instance data)

`editor::open_smart(path)` / `image::open_smart(path)` reuse the main window if it's minimized/free, else spawn a new one — this is how opening a second file/image ends up in its own window instead of clobbering the first (used by `edit`/`view` and the HepFS click handlers).

**Files/HepFS multi-instance:** `HepfsNav` (current dir + back/forward history) used to be a single global `Option<HepfsNav>`; it's now `HEPFS_NAVS: Mutex<Vec<(usize, HepfsNav)>>` keyed by window id, so each Files window browses independently. The click handler now hit-tests *any* non-minimized window with `app_kind == Files` (topmost first) instead of hardcoding id=1, and reads/writes that window's own nav entry.

**A bug this refactor surfaced and fixed:** `render_welcome_window`/`render_hepfs_window`/`render_sysmon_window`/`render_settings_window` used to look themselves up by title string via a `window_rect(title)` helper. If two windows of the same kind ever existed with the same title (which the multi-instance work above makes possible), both would've resolved to the *first* match's coordinates — the second window's chrome would draw in the right place but its content in the wrong one. All four now take explicit `(wx, wy, ww, wh)` parameters like every other window renderer already did; `window_rect()` was deleted.

**Verification note:** all of the above compiles clean and passes a full boot regression, but the actual mouse-driven interactions (right-click a taskbar button, open a jump list, click a jump-list row, browse two Files windows independently) are keyboard/mouse-driven and weren't scripted through the serial-log testing used elsewhere in this project — worth a manual check.

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
| `wget <host>[:<port>] [/path]` | HTTP GET with DNS resolution (default port 80); prints up to 4KB |
| `udp <host>:<port> <msg>` | Send a UDP datagram (DNS-resolved), print any reply within 3s |
| `view <file.bmp>` | Open an uncompressed 24/32-bit BMP in the image viewer window |
| `play <file.wav>` | Play a 16-bit PCM WAV (48kHz, mono/stereo) via the HDA DMA stream |
| `netstart` / `netdiag` / `netpoll` | NIC debug commands |
| `shutdown` / `reboot` | ACPI off / PS/2 reset |
| `echo` / `clear` | Print text / clear screen |
| `exec <file>` | Load and run ELF64 binary from HepFS |
| `ps` | List all processes (PID, state, name) |
| `runtest` | Run embedded ring-3 ELF sanity test |
| `runhello` | Run hello ELF built from userspace/hello (demos hepos-std: String, Vec, println!) |
| `newterm` | Spawn a new floating terminal window |
| `beep [hz] [ms]` | Play square-wave tone via Intel HDA (default: 440 Hz, 200 ms) |

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

**Syntax highlighting:** basic, per-line, no real lexer — `editor::highlight_line()` colors keywords (Rust + C/C++ combined list), string/char literals, numeric literals, and `//` line comments. Enabled automatically for `.rs`, `.c`, `.h`, `.cpp`, `.hpp`, `.cc`, `.cxx` files (`supports_highlighting()`); everything else renders plain. Known limitation: comments/strings don't span lines (no `/* */` block-comment tracking) — each line is colored independently. Verified with a temporary boot-time test asserting exact highlighted-character counts for keywords/numbers/comments/strings against known sample lines, then removed.

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
- `dindirect` — points to a block of 1024 × u32 pointers, each to another 1024-pointer indirect block → 1024×1024 × 4KB = 4GB double-indirect
- **Max file size: ~4GB** (48KB + 4MB + 4GB), bounded in practice by disk capacity (512MB image)
- Backward compatible: on-disk inodes written before `dindirect` existed have zero bytes at that offset (it was padding), so they decode as `dindirect=0` — no migration needed

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

**Stack:** `net.rs` — hand-written Ethernet → ARP → IP → ICMP + TCP (no smoltcp)  
**Static config:** IP 10.0.2.15, GW 10.0.2.2, mask 255.255.255.0

| Driver | TX | RX | Notes |
|--------|----|----|-------|
| RTL8139 | ✓ | ✓ | Confirmed working on QEMU/Windows — ping and HTTP wget both work |
| e1000 | ✓ | ✓ | Also confirmed working |
| virtio-net | ✗ | ✗ | Not detected |

**TCP implementation:** 3-way handshake (SYN → SYN-ACK → ACK), data receive, FIN close. TSC-based 3s timeout. Rotating ephemeral source ports (49152+) to avoid SLiRP TIME_WAIT collisions on repeated calls.

---

## What's Built

### Kernel / Low-level
| ✓/○ | Feature |
|-----|---------|
| ✓ | Boot (HepBL — own from-scratch UEFI bootloader), Framebuffer, GDT, IDT |
| ✓ | PMM (bitmap, >1MB), HHDM, Paging |
| ✓ | Bump heap (1MB), GlobalAlloc |
| ✓ | x2APIC timer, ACPI shutdown/reboot, CMOS RTC |
| 🟡 | Round-robin scheduler (naked-asm context switch) — structure exists, but **real preemption is currently broken** (see Known Issues); `task_blink` effectively runs the whole OS as a single always-scheduled task today |
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
| ✓ | Networking RX — e1000 RX confirmed working on QEMU/Windows (ping 10.0.2.2 replies) |
| ✓ | Intel HDA audio — `beep [hz] [ms]` via hda-duplex codec, square-wave PCM, DMA stream |
| ○ | ACPI FADT parsing (for real hardware shutdown) |
| ○ | AHCI/SATA driver — original plan called for HepFS to work over both SATA and NVMe; only NVMe exists today |
| ○ | USB HID keyboard — XHCI only drives the USB HID mouse (tablet) today; keyboard is PS/2 |
| ○ | Drivers moved to userspace libOS — original plan put all drivers (XHCI/HDA/NVMe/PCI/ACPI/GOP) in userspace except where hardware forces kernel; every driver is still in-kernel (ring 0) today, `userspace/` only has the `hepos-rt`/`hepos-std`/`hello` scaffolding. Biggest architectural gap vs. the original design. |

### Storage
| ✓/○ | Feature |
|-----|---------|
| ✓ | HepFS: format, probe, create/read/write/delete files + dirs |
| ✓ | Path resolution (`/a/b/c`), kernel manifest `/kernel.txt` |
| ✓ | Indirect blocks — files up to ~4.1MB |
| ✓ | `cp`, `mv` terminal commands |
| ✓ | Double-indirect blocks — files up to ~4GB (bounded in practice by disk capacity); verified with a 5MB round-trip (`dindirect` block allocated, byte-exact read-back) |
| ○ | VFS abstraction layer |
| ○ | SATA support — HepFS only mounts over NVMe today; original plan called for both |

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
| ✓ | Desktop icons — 5 icons (Welcome/Files/Terminal/Editor/Sysmon), click to open/focus |
| ✓ | Desktop wallpaper — vertical gradient (navy → near-black) + deterministic LCG star field |
| ✓ | Settings app — left sidebar ("Background" page), wallpaper thumbnails, click to switch; right-click desktop → context menu → "Change background" opens Settings |
| ✓ | Multiple instances of every app, including Files — see "Multi-instance windowing" below |
| ✓ | Taskbar grouping + jump list — windows of the same app kind collapse into one taskbar button with a "(N)" count; clicking it with N>1 opens a jump-list popup listing each instance ("Terminal 1", "Terminal 2", ... or the real title if distinguishing, e.g. "Editor: foo.txt") to pick which to focus |
| ✓ | Start Menu lists one row per *program*, not per window — grouped the same way as the taskbar ("(N)" suffix, "--" badge only when no instance is open, opens the same jump list if N>1) |
| ✓ | Right-click "New Window" — right-click a taskbar button or a Start Menu entry to spawn another instance of that program |
| ○ | Window animations — original plan called for 150–200ms ease-out on open/close/minimize; windows currently snap instantly |
| ✓ | Volume control — `hda::set_volume()`/`get_volume()` (0-100), applied live; Settings app "Sound" page has a click/drag slider, `volume [0-100]` terminal command also available |
| ○ | Dirty-rect (per-widget) tracking — current double-buffer scheme is a coarser two-tier system (full-scene redraw vs. ~20-row cursor-only partial flush), not literal per-widget dirty rectangles |

### Apps
| ✓/○ | Feature |
|-----|---------|
| ✓ | Terminal — 30+ commands, history, left/right cursor, tab completion, dynamic width |
| ✓ | Text editor — Ctrl+F find, PgUp/Dn, Ctrl+Home/End, F2/F10, basic syntax highlighting (Rust/C: keywords/strings/numbers/comments) |
| ✓ | Terminal live input highlighting — command name / known verbs, quoted strings, and numbers colored as you type, reusing the editor's tokenizer with `COMMAND_NAMES` as the keyword list (`terminal::recolor_input()`) |
| ✓ | Text editor selection — drag-to-select and Shift+Arrow/Home/End/PgUp/PgDn extend a visible highlighted selection; typing, Enter, Backspace, and Delete all replace the selection like a normal editor (`Editor::select_anchor`, `selection_range()`, `delete_selection_if_any()`, `mouse_down()`/`mouse_drag()` wired from `main.rs`) |
| ✓ | Terminal selection — drag-to-select and Shift+Left/Right extend a highlighted selection over the scrollback+input grid (`Terminal::select_anchor`/`select_head`, `hit_test()`/`mouse_down()`/`mouse_drag()`) |
| ✓ | Clipboard — Ctrl+C/Ctrl+V and Ctrl+Shift+C/Ctrl+Shift+V copy/paste the editor's selection, and now also the terminal's (Ctrl+Shift+C/V bound in `terminal::on_key` too — Ctrl+C alone stays bound to "cancel input"); shared `CLIPBOARD` static in `clipboard.rs`; right-click Copy/Paste context menu in both the editor and terminal (`ContextMenuKind::EditText`, `Desktop::clipboard_action_requested`) |
| ✓ | Double-click to select a line — in both the editor and terminal, double-clicking a line selects it whole (`select_line()`, double-click detected via `main::is_double_click()` using TSC for timing — switched from `scheduler::TICK_COUNT` after discovering it freezes; see Known Issues) |
| ✓ | HepFS file manager — back/forward/path bar, click-to-navigate, click-to-open |
| ✓ | Welcome window — system info |
| ✓ | Sysmon window — RAM bar, uptime, PCI list, storage/net status |
| ✓ | Multiple terminal windows — `newterm` spawns additional floating terminals, each independently focusable |
| ✓ | Image viewer — decodes uncompressed 24/32-bit BMP (`view <file.bmp>`, or click a `.bmp` in HepFS); `/demo.bmp` checkerboard generated at boot |
| ✓ | Audio player — decodes 16-bit PCM WAV (48kHz, mono/stereo) via non-blocking `hda::play_pcm()`; `play <file.wav>` or click a `.wav` in HepFS opens the Audio Player window (id=7) showing path/format/live progress bar/error; `/demo.wav` (440Hz tone) generated at boot |
| ○ | Two-pane file manager — current HepFS file manager is single-pane with back/forward nav; original plan called for two-pane |
| ○ | Markdown rendering — editor/viewer only handles plain TXT, no `.md` rendering |
| ○ | PNG/JPG image support — image viewer only decodes uncompressed BMP |
| ○ | MP3/FLAC/OGG audio codecs — audio player only decodes uncompressed WAV; no codec support (this is exactly what the planned "thin `std` shim for Symphonia" was meant to unlock) |
| ○ | Video player (MP4/H.264) — no video playback of any kind |
| ○ | PDF viewer — not implemented |
| ○ | ZIP/TAR archive support — not implemented |
| 🟡 | Settings: volume control, resolution — volume control now exists ("Sound" sidebar page); resolution control still missing |

### Networking / Ecosystem
| ✓/○ | Feature |
|-----|---------|
| ✓ | ARP, ICMP, IP checksum, eth_send |
| ✓ | RTL8139 + e1000 RX — confirmed on QEMU/Windows |
| ✓ | TCP stack — 3-way handshake, data receive, FIN close, rotating ephemeral ports |
| ✓ | HTTP GET client — `wget <host>[:<port>] [/path]`, prints up to 4KB |
| ✓ | DNS resolver — UDP query to SLiRP's 10.0.2.3:53; `wget example.com` works |
| ✓ | UDP stack (general-purpose) — `net::udp_send_recv()`, DNS-resolved dest, TSC timeout; `udp <host>:<port> <msg>` terminal command |
| ✓ | Userspace — ring 3, SYSCALL/SYSRET, ELF loader, exec from HepFS, process table |
| ✓ | `hepos-std` shim — `hepos-rt` (bump allocator, panic, sys_write/exit/getpid) + `hepos-std` (println!, String, Vec); `runhello` demo command |
| ○ | Full `std` shim (original plan: thin enough that crates like Symphonia link unmodified) — current shim only covers the tiny demo binary's needs (println!/String/Vec); no `std::io`, no error traits, nowhere near enough surface for a real-world crate |

---

## Known Issues

| Issue | Status |
|-------|--------|
| ~~NVMe size reported as 0 MB~~ | Fixed: the code only ever called Identify *Controller* (CNS=1); Identify *Namespace* (CNS=0, NSID=1) was never actually issued, so `lba_count` stayed 0 and `lba_size` was hardcoded to 512. Also fixed the `IdNs` struct's field offsets — the LBAF array was placed at byte 108 instead of the correct spec offset of 128. Verified: reports the real 512 MB (0x100000 blocks × 0x200 bytes) with a correct byte-exact match to the disk image, no hang. |
| ACPI shutdown only on QEMU | Hardcoded port 0x604 — real hardware needs FADT parsing |
| Terminal text doesn't reflow on resize | Existing output stays at old column width; new input uses current width |
| ~~`beep` audio doesn't stop~~ | Fixed: after tone duration, zero DMA buffer in-place while stream still runs → QEMU next-period read returns silence → 200 ms SDL drain wait → stop with stream_id preserved in bits[23:20] so QEMU matches the running stream. |
| ~~`beep` command freezes the whole desktop while the tone plays~~ | Fixed: `hda::beep()` used to spin-wait for the full tone duration plus the 200ms drain (blocking the entire main loop the whole time — same class of bug as the terminal network commands). Rewritten to generate the square wave into a buffer and hand it to the already-non-blocking `play_pcm()` (which starts the DMA stream and returns immediately; `hda::poll()` advances the zero-buffer→drain→stop sequence over subsequent frames). Verified via a boot-time test: `beep(440, 300)` returned in ~15ms instead of ~500ms, with `is_playing()` true immediately after. |
| ~~Audio Player window shows no live "playing" indicator~~ | Fixed: `hda::play_pcm()` is now non-blocking — it starts the DMA stream and returns immediately; a small state machine (`hda::poll()`, called once per frame from the main render loop) advances zero-buffer → drain → stop over time instead of spin-waiting inside one call. `hda::progress_ms()`/`is_playing()` let the Audio Player window show a live "Playing... Xs / Ys" indicator + progress bar. `beep()` and `play_pcm()` both call a new `stop_now()` before starting, since the controller has only one output stream to share. Verified live: booted with a temporary instrumented test — `play()` returned immediately (target 500ms), the poll loop observed the "playing" state, then cleanly finished with no hang. |
| ~~Terminal commands freeze the whole desktop while running (network ops, etc.)~~ | Fixed — avoided the scheduler entirely (see below) and instead converted `ping`/`wget`/`udp` into a `net::NetJob` state machine (`Ping`/`Resolve`/`Tcp`/`Udp` variants) polled once per frame from `task_blink`'s loop (`net::poll()`, called alongside `hda::poll()`), the same pattern already proven for audio playback. Commands return immediately after sending the first packet; the eventual result (success, error, or timeout) is delivered async via `Terminal::print_async()`, which reprints the prompt and re-inserts whatever the user had typed in the meantime. Verified live against the real SLiRP gateway/DNS: a successful ping, a timed-out ping to an unreachable on-subnet address, and a full DNS-resolve→TCP handoff (`wget example.com`) all completed without blocking, each confirmed via boot-time instrumented tests. Along the way, fixed a real data-corruption bug this surfaced: TCP payload extraction wasn't trimming Ethernet frame padding to the IP header's own Total Length field, so small response segments got phantom zero-byte padding appended to `wget` output. |
| **`scheduler::TICK_COUNT` freezes after the very first tick** | Discovered while building the fix above. Confirmed via a TSC-calibrated 1.5-second busy-wait that `TICK_COUNT` never advances past its initial value — the APIC timer interrupt that bootstraps `kmain → task_blink` never meaningfully re-fires afterward (consistent with the fresh-task-entered-via-bare-`ret`-never-restores-RFLAGS.IF bug root-caused and reverted in an earlier session — that revert only undid the *experimental* multi-task changes, not this pre-existing 2-task idle/blink scheduler, which still has the bug). Practical effect: anything timed off `TICK_COUNT` never expires. The new `net::NetJob` deadlines and the double-click detector (`main::is_double_click()`) were both changed to use TSC (`hda::rdtsc()` + `hda::TSC_PER_MS`) instead, which has no dependency on the timer interrupt. Root fix (making the timer actually keep firing) is still open — see Next Steps. |

---

## Next Steps (Priority Order)

1. ~~**`std` shim**~~ ✓ done — `userspace/` workspace: `hepos-rt` (allocator/panic/syscalls), `hepos-std` (println!, String/Vec re-exports), `hello` demo; kernel bakes hello ELF via build.rs; `runhello` terminal command
2. ~~**Preemptive ring-3**~~ ✓ done (scoped) — timer unmasked during `run_elf` so ring-3 is preemptible by the scheduler; `sys_write` output buffered in `PROC_OUT` and flushed to the terminal window after exec; `swapgs` GS-state bug fixed (was freezing on 2nd run); full multi-process scheduling (fork/waitpid) remains future work
3. ~~**Networking RX**~~ ✓ works — e1000 RX confirmed working on QEMU/Windows (user confirmed ping 10.0.2.2 replies); previously thought to be Windows-only broken but appears to work
4. ~~**Intel HDA audio**~~ ✓ done — HDA DMA stream, `beep [hz] [ms]` via PCM square wave. Stop fix: zero buffer in-place while running, 200 ms SDL drain, stop with stream_id preserved in SD_CTL bits[23:20].
5. ~~**TCP/UDP stack**~~ ✓ done — 3-way handshake, HTTP GET (`wget <host>[:<port>]`), DNS A-record resolver via SLiRP 10.0.2.3:53, TSC timeouts, rotating source ports; general-purpose `udp_send_recv()` + `udp <host>:<port> <msg>` terminal command
6. ~~**Window maximize / snap**~~ ✓ done
7. ~~**Multiple terminal windows**~~ ✓ done
8. ~~**Full-edge resize + directional cursors**~~ ✓ done
9. ~~**"+" new terminal button**~~ ✓ done
10. ~~**Deadlock fix — spawn_terminal outside DESKTOP lock**~~ ✓ done
11. **QEMU cursor on window resize** — `zoom-to-fit` not available on this QEMU/Windows build; known SDL limitation; no fix yet
12. **`std` shim** — implement enough of `std` (alloc, io, fs stubs) so external Rust crates can link
13. ~~**Desktop icons**~~ ✓ done — 5 coloured icons on desktop left edge (Welcome, Files, Terminal, Editor, Sysmon); click opens/focuses the window
14. **RTL8169 / real hardware NIC** — for running on physical machines
18. **virtio-gpu driver** — path to real GPU acceleration. Software rendering (current) writes every pixel via the CPU into a PMM-backed backbuffer, then copies it to the linear GOP framebuffer HepBL hands off — there's no GPU involved at all. Real hardware GPU command-ring formats (Intel/AMD/Nvidia) are enormously complex and largely undocumented for modern hardware, but `virtio-gpu` is a paravirtualized, well-documented, ring-based PCI device in QEMU — the realistic first step toward accelerated blits/scanout and eventually real command submission instead of hand-drawn pixels. Comparable in scope to the NVMe/XHCI drivers, likely bigger. Not started.
15. ~~**HepBL — own bootloader**~~ ✓ done — from-scratch UEFI bootloader in pure Rust (hepbl/), no Limine, no external crates; hand-written UEFI FFI, ELF64 loader, own page tables + HHDM, BootInfo protocol; only asm is the final CR3/RSP/jmp handoff
16. ~~**General UDP stack**~~ ✓ done — `net::udp_send_recv()` + `udp <host>:<port> <msg>` terminal command
17. ~~**Image viewer**~~ ✓ done — `image.rs`: uncompressed 24/32-bit BMP decoder, `view <file>` command, click `.bmp` in HepFS to open; `/demo.bmp` checkerboard generated at boot for testing
19. ~~**Audio player**~~ ✓ done — `audio.rs`: 16-bit PCM WAV decoder (48kHz, mono/stereo), `play <file>` command; `hda::play_pcm()` reuses beep()'s validated DMA stop sequence (zero buffer in-place, drain, stream_id-preserving stop) for arbitrary sample playback, truncated to a 1MB buffer (~5.4s); `/demo.wav` generated at boot for testing
20. ~~**Double-indirect blocks**~~ ✓ done — added `dindirect` field to the on-disk inode (backward-compatible: old padding bytes there were already zero); `write_file`/`read_file`/`remove` all extended with a Phase 3 that walks the 1024×1024 double-indirect tree; verified with a 5MB file (`dindirect` allocated, byte-exact round-trip)
21. ~~**Multiple instances of the same app**~~ ✓ done (Editor) — `editor::open_smart()` reuses the main Editor window (id=3) if minimized/free, else spawns a new window via `editor::spawn_editor()` + `EXTRA_EDITORS` (mirrors `terminal::EXTRA_TERMINALS`); keyboard routing and render dispatch in main.rs extended accordingly. Verified via clean build + full boot regression; interactive multi-window behavior not scripted/tested (keyboard-driven, no automated GUI test harness) — worth a manual check.
22. ~~**Multi-instance for every app + taskbar grouping + right-click "New Window"**~~ ✓ done — generalized #21 to all apps via `Window.app_kind: AppKind` (see "Multi-instance windowing" section above): `image.rs` gained the same `EXTRA_VIEWERS`/`open_smart` treatment as Editor; Welcome/Sysmon/Settings/AudioPlayer got trivial multi-instance since they're stateless; Files converted `HEPFS_NAV` (global) → `HEPFS_NAVS` (per-window) so it's multi-instance too. Taskbar buttons group by app kind with a "(N)" jump-list popup; Start Menu groups the same way (one row per program, not per window); right-click a taskbar button or Start Menu row → "New Window". Fixed a latent title-lookup bug in 4 window renderers along the way (see section above). Verified via clean build + full boot regression; interactive behavior (right-click, jump list, independent Files browsing) not scripted-tested — worth a manual check.
23. ~~**NVMe real disk size**~~ ✓ done — Identify Namespace (CNS=0, NSID=1) was never actually called (only Identify Controller was); added it, and fixed the `IdNs` struct's LBAF array offset (was 108, spec says 128). Verified: reports the true 512 MB instead of the old hardcoded-512-byte/0-blocks placeholder, no hang, R/W still works.
24. ~~**Non-blocking audio playback + live Audio Player progress**~~ ✓ done — `hda::play_pcm()` now starts the DMA stream and returns immediately instead of spin-waiting for the whole clip; a new `hda::poll()` (called once per frame) advances the zero-buffer → drain → stop state machine over time. `hda::progress_ms()`/`is_playing()` feed a live "Playing... Xs / Ys" indicator + progress bar in the Audio Player window. `beep()` and `play_pcm()` each call a new `stop_now()` before starting, since there's only one HDA output stream to share between them. Verified with a temporary instrumented boot test: `play()` returned immediately, the poll loop observed the transient "playing" state, then completed cleanly with no hang.
25. ~~**Syntax highlighting (editor + terminal)**~~ ✓ done — `editor::highlight_line_kw()`: basic per-line tokenizer (keywords/strings/numbers/`//` comments), auto-enabled for `.rs`/`.c`/`.h`/`.cpp`/`.hpp`/`.cc`/`.cxx` files. Made generic over the keyword list so `terminal.rs` reuses the exact same tokenizer for **live input highlighting** — command name/known verbs, quoted strings, and numbers colored as you type (`terminal::recolor_input()`, `COMMAND_NAMES` list shared with tab-completion). Verified with temporary boot-time tests asserting exact highlighted-character counts against known sample lines for both the editor and terminal tokenizers, then removed.
26. ~~**Terminal commands freezing the desktop**~~ ✓ done — see Known Issues for the full writeup; `ping`/`wget`/`udp` are now polled `net::NetJob` state machines instead of blocking calls.
26b. **Real timer-driven preemption** — `scheduler::TICK_COUNT` freezing after the first tick (see Known Issues) means the round-robin scheduler is effectively inert; this is the same underlying territory as #26/the earlier reverted attempt, just now clearly isolated as "the timer interrupt itself doesn't keep firing" rather than a resume/GPF issue. Needed before any real preemptive multitasking (ring-3 processes, background OS tasks) is viable.
27. **Move drivers to userspace libOS** — original plan's biggest deviation; XHCI/HDA/NVMe/PCI/ACPI/GOP all still run in-kernel. Large undertaking — needs a real driver-in-userspace IPC/MMIO-passthrough mechanism first.
28. **AHCI/SATA driver** — HepFS is NVMe-only today; original plan called for both.
29. **Full `std` shim** — enough surface (`std::io`, error traits, etc.) for an unmodified real-world crate (e.g. Symphonia) to link; current shim only covers the `hello` demo's needs. Would unlock real audio/image codec support instead of hand-rolled BMP/WAV-only decoders.
30. **Real file format support** — PNG/JPG (image viewer), MP3/FLAC/OGG (audio player), MP4/H.264 (no video player exists at all), PDF viewer, Markdown rendering, ZIP/TAR archive support. All ❌ today; blocked in practice on #29 for the codec-heavy ones.
31. **Two-pane file manager** — current HepFS file manager is single-pane with back/forward nav; original plan called for two-pane.
32. **Window animations** — 150–200ms ease-out on open/close/minimize, per the original plan; windows currently snap instantly.
33. ~~**Volume control**~~ ✓ done — `hda::set_volume()`/`get_volume()` (0-100, maps to the DAC output amp's 7-bit gain field, applied immediately via a live verb so it affects whatever's currently playing, not just the next clip); `volume [0-100]` terminal command; Settings app gained a second sidebar page ("Sound") with a click-to-set and drag-to-scrub slider. Verified via a boot-time test (default/set/clamp values, and that `beep()` stays non-blocking with volume changes applied).
34. **Settings: resolution control** — Settings app currently only has the wallpaper picker.

---

## Key Global State

```rust
// main.rs
pub static DISPLAY:        Mutex<Option<Display>>       // GOP framebuffer
pub static FOCUSED_WIN:    Mutex<Option<usize>>         // Some(id) = focused window
pub static PCI_DEVS:       Mutex<Vec<PciDevice>>        // populated at boot, used by lspci + sysmon
static     HEPFS_NAVS:     Mutex<Vec<(usize, HepfsNav)>> // per-Files-window navigator: ino, path, back[], fwd[]
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
- **x2APIC via MSR** — avoids mapping xAPIC MMIO at 0xFEE00000 (works under both Limine and HepBL)
- **HepBL page tables live on** — the kernel keeps running on the bootloader's PML4; `map_page`/`map_mmio` walk it (all 4K pages, no huge pages), user PML4s copy entries 256–511 from it; the transitional identity map in PML4[0] is cleared during kmain init
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
| NVMe BAR | Assigned dynamically by OVMF's PCI allocator (observed 0xC000004000 — well above 4GiB, despite `X-PciMmio64Mb=0`); `map_mmio` handles it fine since `map_page`'s PML4 walker creates whatever intermediate tables the address needs, not just the ones from the initial HHDM build |
| e1000 BAR | Also OVMF-assigned, varies by boot |
| e1000 MAC | 52:54:00:12:34:56 |
| SLiRP gateway | 10.0.2.2, MAC 52:55:0a:00:02:02 |
| Static IP | 10.0.2.15 / 255.255.255.0 |
| HHDM offset | 0xFFFF800000000000 (HepBL, covers phys 0..4GiB in 4K pages) |
| XHCI | PCI 1B36:000D, usb-tablet on xhci.0 |

---

## Crate Dependencies

```toml
spin   = "0.9"   # Mutex without std — MIT
# core, alloc, compiler_builtins from rust-src (MIT/Apache-2)
```

All drivers, filesystem, networking, desktop, apps — and now the bootloader (HepBL) — written from scratch.
