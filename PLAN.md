# HepOS — Design Reference & Roadmap

> **Purpose:** Authoritative reference for HepOS. Survives context compaction.
> **Last updated:** 2026-07-07 (Next Steps cleaned up — completed items removed, see git history for their writeups; new gaps added from the Original Design Plan audit below)

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

The project started from a design doc with a specific target shape (libOS-in-userspace, wide file-format support, animated desktop, IDE-grade editor, etc). This section is an honest audit of that plan against what's actually implemented today — updated 2026-07-07. ✅ = done, 🟡 = partial/deviates, ❌ = not implemented. Next Steps below is derived from the gaps here.

**Kernel: Exokernel (Rust, x86\_64)** — ✅ done.

**LibOS layer**
| Item | Status | Notes |
|---|---|---|
| Memory allocator | ✅ | Slab allocator (`heap.rs`) |
| Scheduler (round-robin) | 🟡 | `scheduler.rs`: real preemption works and is soak-tested (see "Known Issues" — fixed a fresh-task EOI/`sti` bug via `task_trampoline`, and a stale-segment-selector `#GP` on first genuine `iretq` resume via explicit CS/SS/DS/ES/FS/GS reload in `gdt::init()`). `spawn()`/`exit_current()`/`sleep_ms()` now support real dynamic tasks and a real blocking primitive (dead-slot reuse, no unbounded growth) — no longer just the 2 hardcoded boot tasks. Still no priorities, and nothing yet actually spawns a background command-worker task with this (the originally-planned use case) — `process::exec()` is still fully synchronous, unrelated to the scheduler. |
| Custom filesystem supporting SATA **and** NVMe | ✅ | `hepfs::BlockDev` abstracts NVMe vs. AHCI; verified with a real format/write/read round-trip on the AHCI backend. The live boot filesystem still mounts on NVMe by default (switching that is a separate, bigger decision — not done). |
| Thin `std` shim (e.g. so Symphonia links unmodified) | ❌ | `hepos-std` only re-exports `println!`/`String`/`Vec` for a tiny demo binary — nowhere near enough surface (`std::io`, error traits, etc.) for an unmodified real-world crate to link. |

**Drivers (plan: "all in libOS userspace except where HW forces kernel")**
| Item | Status | Notes |
|---|---|---|
| XHCI → USB HID (keyboard/mouse) | 🟡 | XHCI drives a USB HID **mouse** only (tablet). Keyboard is PS/2, not USB HID. |
| Intel HDA (audio out) | ✅ | Non-blocking playback (`hda::play_pcm()` + `poll()`) |
| NVMe (storage) | ✅ | |
| PCI enumeration | ✅ | |
| ACPI (shutdown/reboot) | 🟡 | QEMU-only (hardcoded port 0x604); no real FADT parsing for physical hardware |
| GOP framebuffer (display) | ✅ | Via HepBL's BootInfo; also mirrored live to a virtio-gpu device when present (`virtio_gpu.rs`) as a first step toward real GPU-assisted display |
| **Drivers live in userspace libOS** | ❌ | **Not true.** Every driver (XHCI, HDA, NVMe, PCI, ACPI, GOP) runs in the kernel (ring 0). `userspace/` only contains `hepos-rt`/`hepos-std`/the `hello` demo — no actual drivers have been moved out. This is the single biggest architectural gap versus the original plan. |

**Custom Filesystem (HepFS)**
| Item | Status | Notes |
|---|---|---|
| Flat inode table | ✅ | |
| No permissions | ✅ | |
| Files + directories | ✅ | |
| Large blocks, designed for NVMe | ✅ | 4KB blocks |
| No journaling | ✅ | Matches the plan's "keep it simple, maybe add later" |
| SATA support | ✅ | `ahci.rs` + `hepfs::BlockDev` abstraction; HepFS verified working on both NVMe and AHCI (live boot filesystem still defaults to NVMe) |

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
| Simple 150–200ms ease-out animations (open/close/minimize) | ✅ | 180ms ease-out scale on open (creation + unminimize) and close/minimize |
| Taskbar: open apps, clock, volume control | 🟡 | Apps + clock yes; volume control exists (Settings "Sound" page + `volume` terminal command) but isn't a widget directly on the taskbar itself |
| Desktop icons | ✅ | |

**Programs**
| Program | Status | Notes |
|---|---|---|
| Terminal emulator (VT100 subset) | ✅ | 30+ commands |
| File manager, **two-pane** | 🟡 | Two-pane now, but tree-and-list style (directories-only left pane + full listing right pane, one shared location) rather than Norton-Commander-style independent dual browsers |
| Text editor | ✅ | |
| IDE (syntax highlighting, Rust/C) | ✅ | Basic keyword/string/comment/number highlighting added — see Text Editor section below |
| Image viewer | 🟡 | Exists, BMP-only |
| Audio/video player | 🟡 | Audio yes (WAV only); **no video player at all** |
| PDF/MD viewer | ❌ | Neither implemented |
| Settings (volume, resolution) | 🟡 | Wallpaper picker + volume control ("Sound" page) both exist; resolution control still missing (and likely infeasible without bootloader-level GOP mode-selection UI — HepBL picks the mode once at boot, before the kernel runs) |

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
    xhci.rs        XHCI USB host controller — USB HID tablet (absolute mouse coords) +
                   optional USB HID keyboard (translated to PS/2 scancodes via
                   ps2::handle_scancode()); keyboard not attached by default — see
                   PLAN.md Next Steps
    nvme.rs        NVMe driver, admin+IO queues, global CONTROLLER
    hepfs.rs       HepFS: flat inode, 4KB blocks, 12 direct + 1 indirect block per file
    desktop.rs     Compositor, WM, start menu, taskbar, resize handles, RTC clock
    icons.rs       Pixel-art app/file icon glyphs (fill_rect-based, 16x16 unit grid) —
                   desktop icons, taskbar, Start Menu, file manager rows all draw through here
    terminal.rs    Full shell: history, left/right cursor, tab completion, 30+ commands
    editor.rs      Text editor: Ctrl+F find, PgUp/Dn, Ctrl+Home/End, F2=save, F10=close, basic Rust/C syntax highlighting
    net.rs         ARP, ICMP, IP, TCP (hand-written stack), HTTP GET client (wget), ping
    e1000.rs       Intel 82540EM driver (TX works, RX pending)
    rtl8139.rs     RTL8139 driver (flat ring, TX works, RX broken on QEMU Windows)
    virtio_net.rs  virtio-net legacy (incomplete)
    virtio_gpu.rs  virtio-gpu 2D mode: modern virtio-pci transport, GET_DISPLAY_INFO,
                   resource create/attach/scanout/transfer/flush — not wired into the
                   real display path yet, runs as an independent PCI device
    ahci.rs        AHCI/SATA driver: HBA/port init, IDENTIFY, LBA48 read/write (polled,
                   single command slot); HepFS mounts over it via hepfs::BlockDev
    syscall.rs     SYSCALL/SYSRET gate, SWAPGS, MSR setup, dispatcher (write/exit/getpid)
    process.rs     Ring-3 process: user PML4, ELF entry, process table (PID, state, exec, ps)
    elf.rs         ELF64 parser/loader — maps PT_LOAD segments into a user PML4
    hda.rs         Intel HDA driver: PCI detect, BAR0 map, immediate-cmd codec config, stream + BDL, beep()
    image.rs       BMP decoder (24/32-bit, BI_RGB) + image viewer window; `view <file>` command
    audio.rs       WAV decoder (16-bit PCM, 48kHz) + playback via hda::play_pcm(); `play <file>` command
    serial.rs      COM1 debug: print, print_hex
    panic.rs       Prints file:line:message to serial, then spins

userspace/             Rust userspace workspace (builds before kernel; output baked into kernel via build.rs)
  hepos-rt/        Ring-3 runtime: bump allocator (#[global_allocator]), panic handler,
                   sys_write/exit/getpid/mmap_mmio/port_in/port_out
  hepos-std/       std facade: re-exports alloc types + println!/print! macros backed by sys_write
  hello/           Demo binary: exercises String, Vec, println!, sys_getpid — runs via `runhello`
  hwtest/          Userspace-driver proof of concept: reads real hardware (RTC via port I/O,
                   Local APIC ID via MMIO) entirely through syscalls — runs via `runhwtest`

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

## Desktop Icons (real, file-backed, draggable)

Desktop icons (`desktop.rs`'s `Desktop::icons: Vec<DesktopIcon>`) replaced the old fixed compile-time array. Each icon is either `IconKind::Program(AppKind, win_id)` (the 6 built-in apps, unchanged win_ids, seeded once in `Desktop::new()`) or `IconKind::FsEntry { ino, is_dir }`, mirroring a real file/directory under `/home/desktop` (created at boot alongside `/home`, `/etc`).

- **Drag to reposition**: same held/drag-off pattern window dragging already used (`icon_dragging`/`icon_drag_off`/`icon_drag_moved`). Click semantics (select/open/rename) are resolved on *release*, not mousedown, so a real drag never also fires an open or rename just because it started on an already-selected icon.
- **Single click selects; click again to open or rename, timed by `TICK_COUNT`** (not real-time TSC — reuses the already-verified scheduler tick primitive): a second click within `ICON_DBLCLICK_TICKS` (~400ms) on the *same already-selected* icon opens it; a second click slower than that but within `ICON_RENAME_TICKS` (~3s) starts a rename; slower still is treated as an unrelated fresh click. Program icons show a transient "Programs can't be renamed" toast instead of opening a rename prompt.
- **New File / New Folder**: added to the desktop's existing right-click "Background" context menu. Both open the same on-screen `TextPrompt` widget (the first of its kind in this codebase — Enter=confirm/Esc=cancel/Backspace, routed ahead of the editor/terminal keyboard dispatch in `main.rs` whenever `Desktop::text_prompt` is `Some`) that renaming an icon also uses (prefilled with the current name, drawn inline under the icon instead of as a centered modal).
- **HepFS additions**: `hepfs::rename()` (in-place `DirEntry.name` rewrite — doesn't touch the inode or, for a directory, its contents) and the `/home/desktop` directory itself. Confirming the prompt sets `Desktop::prompt_result`, which `main.rs` polls once per frame and turns into the actual `create_file`/`create_dir`/`rename` call (desktop.rs has no access to the block device), then calls `refresh_desktop_icons()` to re-sync from disk.
- **Grid snap**: icons follow the cursor smoothly while dragging, then snap to the nearest cell of a real 2D grid on release (`icon_snap()` — integer round-to-nearest, since `no_std` has no `f32::round` without a math crate: `(delta + cell/2) / cell`). No collision avoidance (two icons can still land on the same cell) — a deliberate simplification, not a bug.
- **Right-click an icon** shows "Open" (+ pin toggles for a program) instead of the generic background menu — `ContextMenuKind::Icon(usize)`, checked via `Desktop::icon_at()` before falling back to `Background`. Right-clicking empty desktop space still shows the original "Change background / New File / New Folder" menu.
- **Pin/unpin to taskbar and desktop**: `desktop::PINNED_TASKBAR: Mutex<Vec<AppKind>>` (module-level, in-memory only — resets on reboot like everything else here) holds apps pinned as taskbar launcher buttons even with no window open; `Desktop::toggle_pinned_desktop()` adds/removes a `Program` icon (reusing the same fixed win_id mapping the original 6 built-ins always had — `program_win_id()`). Both toggle from three places that all funnel into the same `ContextMenuKind::App(AppKind)`/`Icon(usize)` right-click menus: the taskbar, the Start Menu, and desktop icons themselves — labels flip between "Pin"/"Unpin" based on current state (`is_pinned_taskbar()`/`Desktop::is_pinned_desktop()`).
- **Real icon glyphs, not flat color squares** (`kernel/src/icons.rs`, new module): the framebuffer API is `fill_rect`/`draw_text`/`put_pixel` only — no bitmap loading, no line/circle primitives, and no PNG/asset pipeline exists (only a hand-rolled BMP decoder for user *content*, not UI chrome) — so these are blocky Win95-era pixel icons authored on a 16×16 unit grid and scaled to whatever size the caller needs (`icons::u()`), not true bitmaps. `draw_app_icon()` covers all 8 `AppKind`s (house for Welcome, folder for Files, terminal screen + `>_` prompt for Terminal, page + text lines for Editor, bar chart for Sysmon, gear for Settings, photo frame + mountain for ImageViewer, speaker + sound waves for AudioPlayer); `draw_file_icon()` covers desktop `FsEntry` icons and file-manager rows (folder shape for directories, the matching app glyph for `.bmp`/`.wav` files, a generic folded-corner page otherwise). Used at 48px (desktop icons, replacing the old flat-color-plus-title-strip face), 10-12px (taskbar buttons, Start Menu rows, file-manager list rows).
- **Taskbar button drag-to-reorder**: mousedown on a button arms `taskbar_dragging`/`taskbar_drag_start_x` (the button's *click* action — open/focus/minimize/jump-list — still fires immediately at mousedown as it always did, a deliberate simplification over deferring it to release like desktop icons do); moving past a 12px threshold sets `taskbar_drag_moved`; on release, if it moved, the button's `AppKind` is removed from `PINNED_TASKBAR` and reinserted at the dropped-on slot (`(mx.saturating_sub(START_W)) / TASK_BTN_W`) — dragging an unpinned-but-open button also pins it as a side effect of giving it a stable position, matching how real desktop taskbars behave. **Known quirk**: because the click action fires at mousedown rather than being deferred, a drag that starts on a focused window's button will also minimize it once, in addition to reordering — acceptable given the deferred-click alternative would have meant restructuring the existing, already-verified click-action logic under time pressure.
- **Pinned / minimized / running now look different**, not just "dimmed vs. not": pinned-but-never-opened renders as an outline-only "ghost" button (no fill); minimized (has windows, none visible) gets a dim fill plus a dim underline; running-but-unfocused gets the normal button fill with no underline; focused keeps the existing accent fill + bright underline.
- Verified with a temporary boot-time test: icon-grid-snap math against hand-computed expected cells; pin/unpin-to-taskbar round trip; pin/unpin-to-desktop round trip (unpin Settings → icon gone, re-pin → icon back); the file-manager rename prompt end-to-end producing the correct `PromptOutcome`; a full drag-reorder simulated through real `update_mouse()` calls (mousedown on the 3rd pinned button → drag to x=0 → release) confirming `PINNED_TASKBAR`'s order actually changed as expected. Full boot regression clean.
- **What's NOT done yet** (this was one slice of a much larger, staged request — see Next Steps): double-clicking an `FsEntry` icon doesn't yet actually open anything (only `Program` icons do — opening a file by its type or a directory as a Files window needs main.rs's editor/image/audio dispatch, not just desktop.rs); no drag-and-drop between multiple Files windows; no left-edge pinned dock (a *separate* UI element from taskbar pinning, which is done); the file manager is still a two-pane tree+list (now with real per-type icons on each row), not a large-icon grid with mouse-wheel scrolling.

---

## Taskbar & Start Menu

- **HepOS button** (left): popup listing ALL programs regardless of state; click to open/focus
- **Window buttons**: only non-minimized windows shown, grouped by app kind (see below); click focused → minimize, click other → focus; group with >1 window opens a jump list instead
- **Right-click** a taskbar button or a Start Menu row → "New Window" (spawns another instance of that program; not offered for Files), plus "Pin to Taskbar"/"Unpin from Taskbar" and "Pin to Desktop"/"Unpin from Desktop" — see "Desktop Icons" above for how pinning works
- **Pinned apps** show as taskbar buttons even with no window open (`Desktop::taskbar_entries()` merges `PINNED_TASKBAR` with the usual grouped-by-open-window buttons; clicking a pinned-but-not-running one launches a new instance via the existing `new_window_requested` path)
- **Clock** (far right): live RTC time
- **Shutdown ("P") / Restart ("R") buttons**, top-right of the Start Menu's header — same "colored square + single letter" style as a window's close/minimize/maximize buttons. Call `acpi::shutdown()`/`acpi::reboot()` directly (the same functions the terminal's `shutdown`/`reboot` commands use). Hit-testing (`Desktop::menu_btn_hit()`) is checked before the program-row click logic, since row 0's click band overlaps the header's y-range and would otherwise swallow these clicks. Verified with a temporary boot-time test of the hit-test geometry only (button centers hit, buttons don't overlap each other, clicking the "Programs" label misses both) — deliberately not exercised through the real click path, since that would trigger the actual (diverging) shutdown/reboot calls.
- **Volume widget**: a small 3-bar speaker icon just left of the clock (taskbar's reserved right-side zone widened from 80px to `VOL_RESERVED_W = 110` to fit both). Click toggles a slider popup above the taskbar (`Desktop::draw_volume_popup()`), same visual style as the taskbar jump list; click/drag on the slider calls `hda::set_volume()`/`get_volume()` directly (separate consts from `main.rs`'s `VOL_SLIDER_*`, which size the Settings page's own slider in a window's content area). Bars dim when volume is 0. Click-outside closes the popup, except a click back on the icon itself (which falls through to the toggle instead of close-then-reopen). Drag state (`Desktop::volume_drag`) is tracked the same way window-resize/drag is, so moving the mouse off the slider's y-range mid-drag still keeps scrubbing.

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
| `runhwtest` | Run hwtest ELF — userspace-driver proof of concept (RTC read via SYS_PORT_IN, Local APIC ID via SYS_MMAP_MMIO) |
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
- **Select / open / rename now mirrors the desktop's own icons, same thresholds**: `HepfsNav` gained `selected: Option<(pane, row)>`/`selected_at: u64` (still a two-pane tree+list view, not yet the large-icon grid — see "Desktop Icons" for what that still needs); a click on an unselected row just selects it (highlighted); a second click on the *same* row within `desktop::ICON_DBLCLICK_TICKS` (~400ms, the exact same constant the desktop icons use, made `pub` for this reuse) navigates into a directory or opens a file exactly as before; a second click slower than that but within `ICON_RENAME_TICKS` (~3s) opens the same on-screen rename `TextPrompt` the desktop uses (`Desktop::begin_rename_fs_pane()` — a new `PromptKind::RenameFsPane` variant/`fs_rename_ctx` field carries which window/directory the rename applies to, since a file-manager rename isn't necessarily inside `/home/desktop`). Confirming calls `hepfs::rename()` on the *actual* parent directory the file manager was browsing, then unconditionally calls `refresh_desktop_icons()` too (cheap, and simpler than tracking whether the renamed directory happened to be `/home/desktop`).
- Click directory → navigate in (pushes back history) — on the fast-click path only, per above
- Click file → open in editor (or the image viewer / audio player for `.bmp`/`.wav`) — same, fast-click only
- Verified with a temporary boot-time test simulating the full rename prompt flow (open prompt pre-filled with the old name → backspace it clear → type a new name → confirm) and checking the resulting `PromptOutcome::RenameFsPane`'s fields. Full boot regression clean.
- **Still a follow-up, not done here**: the large-icon-grid visual reskin (per-file-type icons, mouse-wheel scrolling sized to the window) and drag-and-drop of files within/between Files windows — see Next Steps.

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
| 🟡 | Round-robin scheduler (naked-asm context switch) — real preemption works and is soak-tested (see Known Issues); `spawn()`/`exit_current()`/`sleep_ms()` add real dynamic tasks + a blocking primitive on top of the original 2 hardcoded tasks; still no priorities |
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
| ✓ | Intel HDA audio — `beep [hz] [ms]` via hda-output codec (switched from hda-duplex — see Known Issues), square-wave PCM, DMA stream |
| ✓ | ACPI FADT parsing — `acpi.rs`: real RSDP→RSDT/XSDT→FADT→DSDT parsing, byte-scanning the DSDT for the `\_S5` package (the well-known hobbyist recipe, not a full AML interpreter — see the module doc) to get the real PM1a/b_CNT_BLK ports and SLP_TYP values, instead of hardcoded QEMU-only ports |
| ✓ | AHCI/SATA driver — `ahci.rs`: PCI detect, port init, IDENTIFY, LBA48 read/write via a single polled command slot, all bounded (never `panic!`/hang on timeout). HepFS now mounts over it too via `hepfs::BlockDev` — see Storage table. |
| ✓ | virtio-gpu driver — `virtio_gpu.rs`: modern virtio-pci transport from scratch (PCI cap-list walk, virtqueue, polled), `GET_DISPLAY_INFO` + full 2D resource create/attach/scanout/transfer/flush pipeline. The real desktop now mirrors to it live (zero-copy, same backbuffer physical memory) alongside the GOP boot display. |
| 🟡 | USB HID keyboard — `xhci.rs` driver support done (translates USB boot-protocol keycodes to PS/2 scancodes, reusing `ps2::handle_scancode()`'s existing shift/caps/ctrl state machine); **not exercised by default** — the shared dev QEMU scripts don't attach a `usb-kbd` device, since doing so risks double-registering every keystroke (QEMU delivers host key events to both PS/2 *and* a USB keyboard simultaneously, and PS/2 stays enabled). See Next Steps for the tradeoff. |
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
| ✓ | SATA support — `ahci.rs` driver + `hepfs::BlockDev` abstraction; HepFS verified working on both NVMe and AHCI backends (live boot filesystem still defaults to NVMe) |

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
| ✓ | Window animations — 180ms ease-out scale on open (creation + unminimize) and close/minimize (`Window::show()`/`hide()`, `eased_rect()`, `Desktop::tick_anims()`) |
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
| ✓ | Two-pane file manager — tree-and-list style (not Norton-Commander independent panes, corrected after user feedback): one shared `HepfsNav` per Files window, single nav bar (back/forward/path) across the top, a directories-only "tree" pane on the left (current directory's subfolders, ~35% width) and the full listing (dirs + files, with sizes) on the right — both views of the same current directory; clicking a directory in either pane navigates both. |
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
| ~~`alloc_block()` returned unzeroed blocks — a freshly-allocated indirect/double-indirect pointer table could read back stale data from a previous boot~~ | Fixed. `bitmap_alloc()` only flips a bit in the block bitmap — it never touches the block's actual on-disk *content*. HepFS reformats its metadata every boot (superblock/bitmaps/inode table), but `format()` never zeroes the data region itself, and the underlying disk image persists on the host across QEMU runs. `write_file()`'s indirect-block phase treats a freshly-allocated block as a pointer table and reads it back immediately (`existing != 0` → "reuse this pointer") — for a genuinely fresh block, that read picked up leftover bytes from an unrelated previous session's filesystem, misread as a real block pointer. Usually invisible (most files never leave the 12 direct blocks), until building the desktop-icon feature below added one more early boot-time directory, shifting which block a large file's (`demo.wav`, ~94KB) indirect table landed on — exposing a garbage pointer that made a later write target an LBA outside the disk, panicking with a very unrelated-looking "NVMe write failed" deep inside `nvme.rs`. Fixed at the root: `alloc_block()` now always zeroes the block it returns (`create_dir`'s own redundant explicit zero afterward was removed). Also added `const _: () = assert!(size_of::<Inode>() == 128)` as a compile-time guardrail after discovering (while investigating this) that the assumption was previously unchecked, even though it happened to hold. Verified: booted repeatedly with the exact directory-count change that originally triggered this, `kernel.txt`/`demo.bmp`/`demo.wav` (the file that exercises the indirect-block path) all write and read back correctly every time. |
| ~~NVMe size reported as 0 MB~~ | Fixed: the code only ever called Identify *Controller* (CNS=1); Identify *Namespace* (CNS=0, NSID=1) was never actually issued, so `lba_count` stayed 0 and `lba_size` was hardcoded to 512. Also fixed the `IdNs` struct's field offsets — the LBAF array was placed at byte 108 instead of the correct spec offset of 128. Verified: reports the real 512 MB (0x100000 blocks × 0x200 bytes) with a correct byte-exact match to the disk image, no hang. |
| ACPI shutdown only on QEMU | Hardcoded port 0x604 — real hardware needs FADT parsing |
| ~~QEMU sometimes refuses to start at all: `SDL_OpenAudioDevice for recording failed: WASAPI can't find requested audio endpoint`~~ | Fixed. `-device hda-duplex` requests a full-duplex (playback **and** recording) host audio backend; when the host's default recording device isn't available (observed repeatedly on this dev machine — possibly after other processes, including stray QEMU instances from interrupted test runs, held onto the audio subsystem), QEMU fails to launch entirely with this error, before HepBL or the kernel ever run. Nothing in HepOS uses microphone input — `beep`/`play` are output-only — so switched both `build.sh`/`build.ps1` to `-device hda-output` (playback-only; same underlying codec model/node topology as `hda-duplex`, just without the record stream, so `hda.rs`'s hardcoded node IDs — AFG=1, DAC=2, output pin=4 — are unaffected). Verified: HDA still initializes and reports codec-detected/init-OK identically to before. This was very likely a real contributor to the "sometimes takes 2-3 tries to boot" reports — a QEMU launch failure looks indistinguishable from a hang if you're not watching the terminal output, and unlike the NVMe-timeout fix (which explains a hang *after* the kernel starts), this explains a hang *before* it ever gets the chance to. |
| `build.sh` failed with `qemu-img: command not found` the first time it needed to create a *new* disk image | Fixed. `qemu-img` was invoked bare, relying on it being on `PATH`; unlike `qemu-system-x86_64` (which has an explicit `command -v` + Program Files fallback), `qemu-img` had neither — this had simply never been hit before because `hepos_disk.img` (NVMe) already existed from earlier sessions, so the creation branch never ran until the new SATA disk image needed to be created for AHCI testing. Now resolved via the same `command -v` + `C:\Program Files\qemu\qemu-img.exe` fallback pattern as the other QEMU binaries. |
| ~~Clicking ✕ on a window stacked above the Files window sometimes acted on the Files window instead of closing the top one~~ | Fixed. Several per-app click-routing blocks in `main.rs` (HepFS, Settings, editor/terminal text-drag-select) each did their *own* independent hit-test scoped only to their own window kind — e.g. HepFS's click handler just checked "is (mx,my) within *some* non-minimized Files window's bounds," with no awareness that a different window might actually be the topmost thing rendered at that exact point. If another window's close button visually overlapped a Files window's large content area, both fired: `Desktop::update_mouse()` correctly closed the top window, but the Files-window handler *also* saw the same click and acted on it (e.g. navigating a directory), which looked like "the window below did something instead of the top one closing." Added `topmost_window_id_at()`, which mirrors `Desktop::update_mouse()`'s own chrome-or-content hit-test but checked topmost-first across *all* windows regardless of kind; every affected block now requires its candidate window to match this before acting. Verified with a boot-time test: a point inside two overlapping windows resolves to the topmost one, a point only inside the bottom window still resolves correctly. |
| Terminal text doesn't reflow on resize | Existing output stays at old column width; new input uses current width |
| ~~`beep` audio doesn't stop~~ | Fixed: after tone duration, zero DMA buffer in-place while stream still runs → QEMU next-period read returns silence → 200 ms SDL drain wait → stop with stream_id preserved in bits[23:20] so QEMU matches the running stream. |
| ~~`beep` command freezes the whole desktop while the tone plays~~ | Fixed: `hda::beep()` used to spin-wait for the full tone duration plus the 200ms drain (blocking the entire main loop the whole time — same class of bug as the terminal network commands). Rewritten to generate the square wave into a buffer and hand it to the already-non-blocking `play_pcm()` (which starts the DMA stream and returns immediately; `hda::poll()` advances the zero-buffer→drain→stop sequence over subsequent frames). Verified via a boot-time test: `beep(440, 300)` returned in ~15ms instead of ~500ms, with `is_playing()` true immediately after. |
| ~~Audio Player window shows no live "playing" indicator~~ | Fixed: `hda::play_pcm()` is now non-blocking — it starts the DMA stream and returns immediately; a small state machine (`hda::poll()`, called once per frame from the main render loop) advances zero-buffer → drain → stop over time instead of spin-waiting inside one call. `hda::progress_ms()`/`is_playing()` let the Audio Player window show a live "Playing... Xs / Ys" indicator + progress bar. `beep()` and `play_pcm()` both call a new `stop_now()` before starting, since the controller has only one output stream to share. Verified live: booted with a temporary instrumented test — `play()` returned immediately (target 500ms), the poll loop observed the "playing" state, then cleanly finished with no hang. |
| ~~Terminal commands freeze the whole desktop while running (network ops, etc.)~~ | Fixed — avoided the scheduler entirely (see below) and instead converted `ping`/`wget`/`udp` into a `net::NetJob` state machine (`Ping`/`Resolve`/`Tcp`/`Udp` variants) polled once per frame from `task_blink`'s loop (`net::poll()`, called alongside `hda::poll()`), the same pattern already proven for audio playback. Commands return immediately after sending the first packet; the eventual result (success, error, or timeout) is delivered async via `Terminal::print_async()`, which reprints the prompt and re-inserts whatever the user had typed in the meantime. Verified live against the real SLiRP gateway/DNS: a successful ping, a timed-out ping to an unreachable on-subnet address, and a full DNS-resolve→TCP handoff (`wget example.com`) all completed without blocking, each confirmed via boot-time instrumented tests. Along the way, fixed a real data-corruption bug this surfaced: TCP payload extraction wasn't trimming Ethernet frame padding to the IP header's own Total Length field, so small response segments got phantom zero-byte padding appended to `wget` output. |
| ~~`scheduler::TICK_COUNT` freezes after the very first tick~~ | Fixed — two stacked bugs, found via a boot-time serial test that TSC-busy-waited past several timer periods and asserted `TICK_COUNT` kept advancing (the same freeze was independently spotted while building the `net::NetJob` fix above, and worked around there via TSC timing instead of being root-caused; this is the follow-up that actually fixes it). (1) A freshly-created task (`Task::new`'s bare-`ret` bootstrap, never through `iretq`) permanently orphaned the EOI for the interrupt that switched it in — `context_switch`'s `ret` jumps straight into the new task, so `timer_stub`'s own `call eoi` (after `call tick` returns) never executes for that entry, leaving the LAPIC's in-service bit stuck and blocking all future timer interrupts; also left `RFLAGS.IF` at whatever it was inside the ISR (0), so the task ran with interrupts permanently off. Fixed with `scheduler::task_trampoline` — a landing pad (`rbx` = entry fn, set as the fake stack's return address) that does `call apic::eoi; sti; jmp rbx` before ever reaching task code. (2) Once that was fixed, the *first genuine `iretq`* (resuming a task actually preempted mid-execution) took a `#GP` with error code 0x38 at the `iretq` in `timer_stub` — this is almost certainly **the same "resuming a genuinely-interrupted task corrupts a segment selector" bug the previous preemption attempt hit and reverted over**, now root-caused: `gdt::init()`'s `lgdt` only repoints GDTR, it never reloads CS/SS/DS/ES/FS/GS's hidden descriptor caches, so the kernel had been running the whole time on HepBL/UEFI's stale segment selectors (invisible during normal execution, since the CPU only re-validates a segment selector when the register is explicitly reloaded — which an `iretq` does). The very first real task resume forced CS to reload from that stale value, landing out of range of our (much smaller) GDT → `#GP`. Fixed by reloading every segment register right after `lgdt` (CS via a `push sel; lea; push; retfq` far-return trick, since CS can't take a direct `mov`). Verified with a temporary boot-time serial test: `TICK_COUNT` advanced continuously (~100/s) across an 8-second window with `task_blink` genuinely preempted and resumed hundreds of times, no GPF, no hang; test code removed after confirming. The `net::NetJob`/double-click TSC-based timing above still works fine and doesn't need to change back, but `TICK_COUNT` is now also safe to depend on directly. This unblocks (but does not by itself re-implement) background command-worker tasks as an alternative to the polling-state-machine approach above. |
| ~~Boot intermittently hangs on the splash screen, needing 2-3 retries~~ | Fixed (likely — see note). Root cause: nothing is drawn to the framebuffer between the initial splash screen (drawn once, right after `Display::new()`, at the very top of `kmain`) and `task_blink`'s first `render()` call at the very end of boot — so *any* panic anywhere in between (PCI/NVMe/HDA/network/PS2/XHCI init) freezes the display exactly on the splash, forever, since `panic()` only prints to serial and spins (`panic.rs`) — no crash screen, no reboot, nothing visibly different from a plain hang. Found a real bug matching this exactly in `nvme.rs`'s controller-disable wait: it panics past a spin-count budget of `to_ms * 1_000`, while the very next loop (controller-enable, waiting on the same class of hardware condition) uses `to_ms * 10_000_000` — 10,000x more generous for no reason. Since `spin_loop()` iterations don't correspond to a fixed amount of wall-clock time, that undersized budget could — and, per repeated user reports, apparently did — panic before the controller's real spec'd timeout (`CAP.TO`, up to ~127s) had actually elapsed, especially on a loaded/slower host machine. Unified both loops to the same generous budget, and floored `to_ms` at 500 (guards a controller reporting `CAP.TO == 0`, which would otherwise make the panic threshold 0 — an instant panic on the very first spin where RDY hasn't already flipped). This may not be the *only* boot-time panic risk (any future `panic!()`/`.unwrap()`/`.expect()` added to code that runs before `task_blink` starts would reproduce the exact same "silently frozen splash" symptom), but it's the only one found in the current driver-init path, and 3 repeated clean boots confirmed no regression. |
| ~~`elf.rs` silently corrupted a userspace binary's `.rodata`/`.got` when two `PT_LOAD` segments shared a page~~ | Fixed. Found while bringing up the `hwtest` userspace program (see "Userspace drivers" below): its GOT/RELRO segment (8 bytes) happened to land in the same 4KB page as the tail of its code/rodata segment — legal per the ELF spec (segments only need page *alignment*, not page *exclusivity*) and something `hello`'s particular layout happened to avoid by luck, which is why this had never been hit before. The loader processed each `PT_LOAD` independently: for a page already mapped by an earlier segment, it allocated a **second**, freshly-zeroed physical page, wrote only the new segment's few bytes into it, and remapped the same virtual address to point at it — silently discarding everything the earlier segment had placed there (in this case, format strings and vtable-ish `.got` entries). Ring-3 execution didn't fault where the data went missing; it faulted much later when a stale pointer into the now-blank page was dereferenced or a corrupted indirect call target was taken, showing up as an unrelated-looking page fault deep in `core::fmt` machinery with RIP in dead zero-padding — a genuinely confusing symptom to trace back to "wrong ELF segment handling." Fixed by tracking already-mapped virtual pages (`page_map: Vec<(virt, phys)>`) and reusing the existing physical page (writing the new segment's bytes into it in place) instead of re-allocating. Verified: `hwtest` runs end-to-end (RTC port read + Local APIC MMIO read, see below); `hello` and `runtest` still pass. |
| Ring-3 syscall wrappers must declare every register the kernel's SYSCALL stub touches as clobbered, not just the ones a given call happens to use | The stub (`kernel/src/syscall.rs`) always shuffles `rdi/rsi/rdx/r10/r8/r9` into the dispatcher's SysV argument registers and always overwrites `rax` with a return value — regardless of how many arguments *this specific* syscall semantically needs. `userspace/hepos-rt`'s wrappers originally only declared `rcx`/`r11` as clobbered (matching what SYSCALL/SYSRET themselves use), which happened not to bite `sys_write`/`sys_exit`/`sys_getpid` in practice, but caused a real, reproducible ring-3 crash (a corrupted control-flow target, landing execution in dead code past the end of the binary) once a fourth wrapper (`sys_port_out`, 3 args) was added and the compiler chose to keep something else live in one of those registers across the call. Fixed by marking every one of `rdi/rsi/rdx/r10/r8/r9/rcx/r11/rax` as `inout(reg) x => _`/`out(reg) _` in all wrappers, whether or not that particular call uses them. Any *future* syscall wrapper added to `hepos-rt` must follow the same rule — this is now called out in a comment directly above the wrappers. |

---

## Next Steps (Priority Order)

Completed items are removed once done — see the "Original Design Plan vs. Current Reality" audit above for current status, "What's Built" below for what exists, and `git log` for how each one was actually implemented (every fix this project has made has a detailed commit/PLAN.md history if it's ever needed again).

1. 🟡 **Move drivers to userspace libOS** — original plan's biggest deviation; XHCI/HDA/NVMe/PCI/ACPI/GOP all still run in-kernel. The foundational IPC/MMIO-passthrough syscall layer this needs first now exists and is proven working; no existing kernel driver has been migrated yet (see below for exactly what remains).
    - Three new HepOS-specific syscalls (`kernel/src/syscall.rs`, numbered 500+ so they can never collide with a real Linux-ABI syscall this project might add later): `SYS_MMAP_MMIO(phys_addr, len) -> user VA` maps a physical MMIO region directly into the calling process's own page tables (via a new `paging::map_page_current_user()`, which — unlike `map_page_into` — operates on the *currently loaded* CR3 with the USER bit set at every page-table level, since a syscall runs with the calling process's PML4 already live); `SYS_PORT_IN(port, width)`/`SYS_PORT_OUT(port, width, val)` do privileged port I/O *inside* the syscall (ring 3 has no IOPL/I/O-bitmap set up here, so this boundary **is** the permission check for now — deliberately not real permission scoping, see below).
    - Once mapped, the process reads/writes the MMIO region directly with no further syscalls — the actual point of "passthrough": a real driver needs fast polled access, not a syscall per register touch.
    - Proven end-to-end with a new userspace program, `userspace/hwtest` (built alongside `hello`, baked into the kernel the same way, run via the terminal's `runhwtest` command): reads the RTC "seconds" register through `SYS_PORT_IN`/`SYS_PORT_OUT` (ports 0x70/0x71 — fixed ISA ports, no PCI/BAR discovery needed) and the Local APIC ID register through `SYS_MMAP_MMIO` (physical address 0xFEE00000 — a fixed x86_64 architectural constant, likewise no BAR discovery needed) — entirely from ring 3, with zero kernel-side driver code involved for either read. Verified via a temporary boot-time test: both reads returned real hardware values (`RTC seconds (BCD) via SYS_PORT_IN: 0x27`, `Local APIC ID via SYS_MMAP_MMIO: 0`) and the process exited cleanly.
    - Building this surfaced two real, previously-unnoticed bugs — one in the ELF loader (`elf.rs` silently corrupted `.rodata`/`.got` when two `PT_LOAD` segments shared a page) and one in the syscall-wrapper convention (`hepos-rt`'s wrappers didn't declare all kernel-clobbered registers) — both fixed; see Known Issues above for the full writeups, since they're subtle enough to be worth understanding if a similar crash resurfaces.
    - **What this does NOT prove, and what's still needed before any real driver migrates:** `run_elf`/`exec()` is still fully synchronous — a userspace program blocks the entire kernel until it exits (see `process.rs`). A driver needs to run *continuously*, handling interrupts and polling hardware while the rest of the OS keeps going. The scheduler now supports real dynamic tasks (see "Dynamic task spawn/exit + blocking primitives" in the completed work below) — that piece is no longer the blocker — but `process::exec()` itself hasn't been changed to run *as* one of those tasks instead of blocking `kmain`/`task_blink`, and there's still no permission/capability scoping (any process reaching these syscalls can touch any physical address or I/O port — fine for one proof-of-concept program, not once more than one process exists) and no IRQ-delivery-to-userspace syscall (a real driver needs to block waiting for its device's interrupt, not busy-poll). None of that is done here — this item lays the hardware-access foundation only.
2. **Full `std` shim** — enough surface (`std::io`, error traits, etc.) for an unmodified real-world crate (e.g. Symphonia) to link; current shim only covers the `hello` demo's needs. Would unlock real audio/image codec support instead of hand-rolled BMP/WAV-only decoders.
3. **Real file format support** — PNG/JPG (image viewer), MP3/FLAC/OGG (audio player), MP4/H.264 (no video player exists at all), PDF viewer, Markdown rendering, ZIP/TAR archive support. All ❌ today; blocked in practice on #2 for the codec-heavy ones.
4. 🟡 **USB HID keyboard** — driver support is done in `xhci.rs`; not attached in the default dev QEMU scripts.
    - `xhci.rs` generalized from single-device (mouse-only) to support a second HID endpoint: `bring_up_hid_device()` factors out the per-device Enable Slot → Address Device → SET_CONFIGURATION → Configure Endpoint sequence (previously inlined once for the mouse) so it can run twice; `poll()` now reads the *slot ID* out of each Transfer Event TRB to route it to the right device instead of assuming every event is the mouse.
    - Keyboard input is translated from USB HID boot-protocol reports (modifier byte + up to 6 simultaneous keycodes) into PS/2 Set-1 scancodes and fed through the existing `ps2::handle_scancode()` — reusing 100% of its shift/caps-lock/ctrl state machine and special-key mapping (arrows, F-keys, Home/End/PgUp/PgDn/Delete) instead of duplicating any of it, so the rest of the OS needed zero changes to accept USB keyboard input. Modifier keys are edge-tracked on both press *and* release (PS/2's SHIFT/CTRL state needs both edges); regular keys only fire on the press edge (boot-protocol reports list every currently-held key on every report, so "newly appeared in this report" is the press signal — releases don't need an event since `ps2.rs` already ignores non-modifier key releases).
    - **Scoped simplification, not hidden:** this driver doesn't parse HID report/interface descriptors to classify device type (deferred to avoid also implementing control-IN data-stage transfers, an even bigger addition on top of an already-sizable one). Instead it assumes the first connected XHCI port is the mouse and the second is the keyboard — true as long as `usb-tablet` is listed before `usb-kbd` on the QEMU command line, since QEMU attaches devices to ports in command-line order. Fine for this dev environment; would need real descriptor parsing to be robust on arbitrary hardware/port orderings.
    - **Why it's not attached by default:** adding `-device usb-kbd,bus=xhci.0` to `build.sh`/`build.ps1` was deliberately *not* done, because QEMU delivers host keyboard events to both the PS/2 controller and any attached USB HID keyboard simultaneously by default — and PS/2 stays enabled (q35's LPC controller always includes it). Since both paths now feed the same `ps2::handle_scancode()` pipeline, attaching a USB keyboard alongside PS/2 would very likely double up every keystroke. Making that safe means either disabling PS/2 in the QEMU config (bigger, riskier change to the shared dev setup) or de-duplicating at the input layer (more code, more state) — neither of which felt right to do un-asked as a side effect of a driver-support task.
    - Verified with a temporary boot-time test of the pure translation functions (keycode table, modifier table, press/hold edge-detection all correct) that needs no real USB keyboard hardware — then a full boot with the existing single-mouse QEMU setup confirming zero regression (mouse still works identically, no second device attempted since only one port is connected). **Not verified:** the actual XHCI multi-device bring-up (second Enable Slot/Address/Configure sequence) against a real attached `usb-kbd`, since that requires the QEMU wiring above. To try it: add `-device usb-kbd,bus=xhci.0` after the `usb-tablet` line in `build.sh`/`build.ps1` for a one-off test, expect "xhci: keyboard ready!" in the serial log, and watch for doubled characters when typing (per the note above).
5. **RTL8169 / real hardware NIC** — for running on physical machines (untestable in this QEMU-only dev environment).
6. **QEMU cursor on window resize** — `zoom-to-fit` not available on this QEMU/Windows build; known SDL limitation, not a HepOS bug, no fix available from our side.
7. **Settings: resolution control** — Settings app has wallpaper + volume pages, no resolution control. Likely infeasible as a *live* control without bootloader-level GOP mode-selection UI, since HepBL picks the display mode once at boot, before the kernel (or Settings) ever runs — would need HepBL itself to offer a mode picker pre-ExitBootServices.
8. **Per-widget dirty-rect tracking** — current two-tier scheme (full-scene redraw vs. ~20-row cursor-only partial flush) achieves the same practical goal as literal per-widget dirty rects but isn't the same mechanism. Minor polish, not blocking anything.
9. **Real desktop shell, remaining pieces** — desktop icons are real/draggable(grid-snapped)/renamable/file-backed; taskbar+Start-Menu pin/unpin, icon glyphs, right-click-icon "Open", and file-manager select/rename-matching-desktop are all done (see "Desktop Icons"/"Taskbar & Start Menu"/"HepFS File Manager" above). What's left of that same original request:
    - Opening a file-backed desktop icon by double-click (currently only `Program` icons open — needs main.rs's editor/image/audio/Files dispatch, which desktop.rs doesn't have access to).
    - Drag-and-drop of files within/between open Files windows.
    - A left-edge pinned dock, separate from the bottom taskbar — note this is now a *distinct* remaining item, since taskbar pinning itself (pin/unpin a program to the taskbar) is done; this is specifically a *new UI element* (nothing like it exists today) for dragging files/directories onto to pin them, positioned so it doesn't collide with the desktop icon column at `ICON_X=16`.
    - The file manager's large-icon-grid visual reskin: per-file-type icons (currently just a `d`/`f` text prefix) and mouse-wheel scrolling sized to the window — the two-pane tree+list layout itself is unchanged; only its *click semantics* (select/open/rename) were brought in line with the desktop this round.

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

---

## Future Vision — Long-Term / Exploratory (not scheduled)

Ideas for after the current roadmap (Next Steps above) is done. These are intentionally vague — captured so they aren't lost, not committed to or scoped in detail yet.

### ".hal" — a per-program hardware abstraction layer

**The idea (user's framing):** a separate, purpose-built layer — possibly its own small language/toolchain — that every userspace program targets instead of talking to the kernel directly. Each program would run isolated behind this layer, which exposes hardware/OS capabilities (files, network, display, audio, input) through a stable interface. The goal: make it realistic to port existing open-source programs onto HepOS by targeting `.hal` instead of rewriting them against HepOS's raw internals.

**How this relates to what's already tracked:** this is a superset of two items already in Next Steps —
- **#27 Move drivers to userspace libOS** (drivers behind an IPC/MMIO-passthrough boundary)
- **#29 Full `std` shim** (enough of `std` that unmodified crates link)

`.hal` is really "what do #27 and #29 look like once they're generalized into a first-class, stable, documented interface" rather than a wholly separate effort — the isolation boundary (userspace drivers) and the ABI surface (`std` shim) are the same underlying problems.

**Two paths, worth deciding between when this becomes active work:**
1. **ABI-first (lower risk):** define `.hal` as a syscall/library ABI in Rust (or C-compatible) — a real libc-equivalent — that existing programs compile against with minimal source changes. This is a large but well-understood undertaking (same category as writing a libc).
2. **Language-first (higher risk, higher ceiling):** design `.hal` as its own small language with a compiler, so programs are *written* in it, not just linked against it. Bigger bet — new toolchain, no existing ecosystem, much longer runway before anything ports successfully.

**Recommendation when this is picked up:** start with (1). It reuses #27/#29's groundwork, and a stable ABI can be validated by porting one real small open-source program end-to-end before investing in anything language-shaped. If the ABI turns out to be fundamentally limiting for the kinds of programs being ported, that's the point to revisit (2) — not before.

**Open questions to resolve later:** what isolation mechanism backs "each program is isolated" (separate address space via existing ring-3/PML4 support already in `process.rs`? something stronger?); what the minimum viable capability set is (files/net/display/audio/input, per the drivers already built); whether porting targets C programs, Rust programs, or both.
