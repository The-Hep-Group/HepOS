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
- **Single click selects; click again to open or rename, timed by `TICK_COUNT`** (not real-time TSC — reuses the already-verified scheduler tick primitive): a second click within `ICON_DBLCLICK_TICKS` (~600ms, widened from an initial ~400ms — see Known Issues) on the *same already-selected* icon opens it; a second click slower than that but within `ICON_RENAME_TICKS` (~3s) starts a rename; slower still is treated as an unrelated fresh click. Program icons show a transient "Programs can't be renamed" toast instead of opening a rename prompt. This timing check is measured against `icon_last_clicked`, kept separate from the actual selection set (`icon_selected: Vec<usize>`, see multi-select below) so a Shift+click doesn't reset it and multi-selecting several icons doesn't make an unrelated later click on one of them look like a double-click.
- **Multi-select — Shift+click and rubber-band marquee, drag moves the whole group**: `icon_selected` is a `Vec<usize>`, not a single `Option`. Shift+click toggles one icon's membership (no open/rename/drag semantics for that click — it only ever adjusts the set); a plain click on empty desktop space clears the selection and arms a marquee (`marquee_start`), which — while held — recomputes `icon_selected` every frame to whatever icons the rectangle from `marquee_start` to the current cursor position intersects, and renders as a hollow accent-colored rectangle. Dragging any icon that's part of a >1-item selection moves every selected icon by the same delta (and later snaps all of them to the grid together on release); dragging an icon that *isn't* part of the current selection first collapses the selection to just that one, like any normal desktop.
- **New File / New Folder**: added to the desktop's existing right-click "Background" context menu. Both open the same on-screen `TextPrompt` widget (the first of its kind in this codebase — Enter=confirm/Esc=cancel/Backspace, routed ahead of the editor/terminal keyboard dispatch in `main.rs` whenever `Desktop::text_prompt` is `Some`) that renaming an icon also uses (prefilled with the current name, drawn inline under the icon instead of as a centered modal).
- **HepFS additions**: `hepfs::rename()` (in-place `DirEntry.name` rewrite — doesn't touch the inode or, for a directory, its contents) and the `/home/desktop` directory itself. Confirming the prompt sets `Desktop::prompt_result`, which `main.rs` polls once per frame and turns into the actual `create_file`/`create_dir`/`rename` call (desktop.rs has no access to the block device), then calls `refresh_desktop_icons()` to re-sync from disk.
- **Grid snap**: icons follow the cursor smoothly while dragging, then snap to the nearest cell of a real 2D grid on release (`icon_snap()` — integer round-to-nearest, since `no_std` has no `f32::round` without a math crate: `(delta + cell/2) / cell`). No collision avoidance (two icons can still land on the same cell) — a deliberate simplification, not a bug.
- **Right-click an icon** shows "Open" + "Pin to Taskbar"/"Unpin"+"Unpin from Desktop" for a program, or "Open"+"Pin"/"Unpin" (to the left dock — see "Left Pin Dock")+"Copy" for a file/directory — instead of the generic background menu. `ContextMenuKind::Icon(usize)`, checked via `Desktop::icon_at()` before falling back to `Background`. Right-clicking empty desktop space still shows the original "Change background / New File / New Folder" menu.
- **Pin/unpin to taskbar and desktop**: `desktop::PINNED_TASKBAR: Mutex<Vec<AppKind>>` (module-level, in-memory only — resets on reboot like everything else here) holds apps pinned as taskbar launcher buttons even with no window open; `Desktop::toggle_pinned_desktop()` adds/removes a `Program` icon (reusing the same fixed win_id mapping the original 6 built-ins always had — `program_win_id()`). Both toggle from three places that all funnel into the same `ContextMenuKind::App(AppKind)`/`Icon(usize)` right-click menus: the taskbar, the Start Menu, and desktop icons themselves — labels flip between "Pin"/"Unpin" based on current state (`is_pinned_taskbar()`/`Desktop::is_pinned_desktop()`).
- **Real icon glyphs, not flat color squares** (`kernel/src/icons.rs`, new module): the framebuffer API is `fill_rect`/`draw_text`/`put_pixel` only — no bitmap loading, no line/circle primitives, and no PNG/asset pipeline exists (only a hand-rolled BMP decoder for user *content*, not UI chrome) — so these are blocky Win95-era pixel icons authored on a 16×16 unit grid and scaled to whatever size the caller needs (`icons::u()`), not true bitmaps. `draw_app_icon()` covers all 8 `AppKind`s (house for Welcome, folder for Files, terminal screen + `>_` prompt for Terminal, page + text lines for Editor, bar chart for Sysmon, gear for Settings, photo frame + mountain for ImageViewer, speaker + sound waves for AudioPlayer); `draw_file_icon()` covers desktop `FsEntry` icons and file-manager rows (folder shape for directories, the matching app glyph for `.bmp`/`.wav` files, a generic folded-corner page otherwise). Used at 48px (desktop icons, replacing the old flat-color-plus-title-strip face), 10-12px (taskbar buttons, Start Menu rows, file-manager list rows).
- **Taskbar button drag-to-reorder**: mousedown on a button arms `taskbar_dragging`/`taskbar_drag_start_x` (the button's *click* action — open/focus/minimize/jump-list — still fires immediately at mousedown as it always did, a deliberate simplification over deferring it to release like desktop icons do); moving past a 12px threshold sets `taskbar_drag_moved`; on release, if it moved, the button's `AppKind` is removed from `PINNED_TASKBAR` and reinserted at the dropped-on slot (`(mx.saturating_sub(START_W)) / TASK_BTN_W`) — dragging an unpinned-but-open button also pins it as a side effect of giving it a stable position, matching how real desktop taskbars behave. **Known quirk**: because the click action fires at mousedown rather than being deferred, a drag that starts on a focused window's button will also minimize it once, in addition to reordering — acceptable given the deferred-click alternative would have meant restructuring the existing, already-verified click-action logic under time pressure.
- **Fixed: dragging a taskbar button didn't visibly follow the cursor.** The reorder logic above worked, but nothing rendered differently until you released — it *looked* broken even though the drop-time reorder was correct. Fixed with a floating "ghost" (`draw_taskbar()`'s `dragged_ghost`): while `taskbar_drag_moved` is true, the dragged button is skipped in its normal slot (left as a faint outline placeholder so the other buttons don't jump around) and redrawn as a copy that tracks `self.prev_cx` horizontally, on top of everything else. Needed one more fix to actually animate: plain mouse movement only sets `mouse_dirty` (the cheap cursor-only partial-flush path, which never touches the taskbar), so `update_mouse()` now forces a full `self.dirty = true` every frame `taskbar_drag_moved` is set, so the ghost keeps up with the cursor in real time instead of only jumping on the next unrelated full redraw.
- **Pinned / minimized / running now look different**, not just "dimmed vs. not": pinned-but-never-opened renders as an outline-only "ghost" button (no fill); minimized (has windows, none visible) gets a dim fill plus a dim underline; running-but-unfocused gets the normal button fill with no underline; focused keeps the existing accent fill + bright underline.
- Verified with a temporary boot-time test: icon-grid-snap math against hand-computed expected cells; pin/unpin-to-taskbar round trip; pin/unpin-to-desktop round trip (unpin Settings → icon gone, re-pin → icon back); the file-manager rename prompt end-to-end producing the correct `PromptOutcome`; a full drag-reorder simulated through real `update_mouse()` calls (mousedown on the 3rd pinned button → drag to x=0 → release) confirming `PINNED_TASKBAR`'s order actually changed as expected. Full boot regression clean.
- **Double-clicking a file-backed desktop icon now actually opens it**: `Desktop::open_fs_entry_requested: Option<(ino, is_dir, name, parent_path)>` — set by `open_icon()`, since desktop.rs has no access to the block device or editor/image/audio state; `main.rs` polls it once per frame (same `*_requested` pattern as `new_window_requested`/`open_settings_requested`) and either spawns a new Files window navigated straight to that directory (`nav.ino`/`nav.path` set directly, skipping the normal click-to-navigate path) or dispatches the file by extension exactly like the file manager already does (`.bmp`→image viewer, `.wav`→audio player, else→editor). `parent_path` was added when the pin dock (below) started opening items from *outside* `/home/desktop` too — a desktop icon always hardcodes `"/home/desktop"`, but a pinned item could be from anywhere, and HepFS inodes have no reverse/parent pointer to reconstruct that later from just the ino.
- **Fixed: a fast double-click on a file/folder icon was opening the rename prompt instead of opening it.** Root cause was the item above, not the click-timing logic: `open_icon()` used to no-op entirely for `FsEntry` icons (nothing wired up yet), so a "successful" fast double-click produced no visible effect and left the icon still selected — a user's natural next click (retrying, since nothing seemed to happen) would then land inside `ICON_RENAME_TICKS`' slower window and trigger a rename, which looked indistinguishable from "double-click triggers rename." Now that opening actually works, `open_icon()` also explicitly clears `icon_selected` on success so a genuinely-successful open can never be mistaken for the start of a rename sequence by a stray follow-up click. Also widened `ICON_DBLCLICK_TICKS` from ~400ms to ~600ms as a separate safety margin, since 400ms is tight for a real double-click. Verified with a temporary boot-time test: two full mousedown/mouseup cycles with no delay between them (worst-case "as fast as possible" double-click) produces the correct open request, clears selection, and never opens a rename prompt.
- **What's still NOT done** (this was one slice of a much larger, staged request — see Next Steps): no drag-and-drop between multiple Files windows; the file manager is still a two-pane tree+list (now with real per-type icons on each row), not a large-icon grid with mouse-wheel scrolling.

---

## Left Pin Dock

A narrow strip fixed at the very left edge of the screen (`DOCK_W = 56`), holding pinned **files and directories only** — programs pin to the taskbar instead (`PINNED_TASKBAR`, see below), kept as a separate concept on purpose. Sits to the left of the desktop icon column, which is why `ICON_X` isn't `16` anymore (it shifted to `DOCK_W + 16` to make room — the only place that constant is used, so nothing else needed to change).

- `desktop::PINNED_FILES: Mutex<Vec<(ino, parent_path, name, is_dir)>>` — in-memory only, same lifetime as `PINNED_TASKBAR`. Stores `parent_path` (not just `parent_ino`) for the same reverse-pointer reason `open_fs_entry_requested` does; snapshotted at pin time, so renaming/moving/deleting the real entry elsewhere doesn't update or remove its pinned copy here (a known, documented simplification, not a bug).
- Single click on a dock item opens it — a launcher, not a browsable icon grid, so no select/double-click/rename dance like the desktop icons have.
- Populated from two places: the desktop icon / file-manager-row right-click menus' "Pin"/"Unpin" item (see below), which call `Desktop::toggle_pinned_file()` directly.

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
| `service <list\|status\|start\|stop\|enable\|disable> [name]` | Manage the 4 userspace driver services (`rtl8139d`/`hdad`/`ahcid`/`xhcid`) — see "Service management" below |
| `kill <pid>` | Stop a running driver service by PID (looks the PID up in the process table, maps it to a service name, same cooperative stop as `service stop`) |

---

### Service management (`service`/`kill` commands)

A lightweight, systemd-flavored front end over the 4 userspace driver processes migrated earlier (RTL8139/HDA/AHCI/XHCI) — `service list` shows all 4 with their enabled/running state, `service status/start/stop/enable/disable <name>` acts on one, and `kill <pid>` does the same `stop` after mapping a PID back to its service name via the process table.

- **Not a true forced kill — a cooperative shutdown, and that's a deliberate design choice, not a limitation worked around.** This scheduler has no safe way to preempt an arbitrary task from outside its own context: a driver process could be mid-syscall, holding its own module's kernel-side lock (`NIC`/`HDA`/`CONTROLLER`/`XHCI`), or mid register I/O — forcibly marking it `Dead` and reclaiming its resources at an arbitrary point could leave a lock permanently held or hardware in a half-configured state (see the `swapgs`/`user_rsp` Known Issues entries above for the kind of subtle, hard-to-reproduce bugs that already came from *less* invasive cross-task interactions in this kernel). Instead, `stop` adds a `stop: u32` field to each driver's existing `Mailbox` (the same shared-memory struct each already uses for its normal IPC) — the kernel sets it to 1 and the driver's own loop checks it once per iteration (alongside its existing `SYS_WAIT_IRQ` rate-limiting), calling `sys_exit(0)` itself if set. Safe by construction: the process only ever exits at a point *it* chose, mid-loop, never mid-operation. `hdad` additionally halts its DMA stream cleanly first if a clip is still playing, so hardware doesn't keep looping a buffer nobody's left to stop.
- **`enable`/`disable` are in-memory only for this session** (don't persist across a reboot) and — matching real `systemctl` semantics for the flag's *meaning*, just not its durability — only govern whether a *future* `start` is allowed to succeed; `disable` never touches an already-running instance.
- **`start` reuses the existing hardware bring-up** — the one-time PCI/DMA/controller setup each driver's `init()` already did at boot never needs to be redone, only the ring-3 process itself (relaunched with the same long-lived mailbox physical address, `stop` reset back to 0).
- **A real race found and fixed while testing this**: `process::exec_async_with_arg()` only *queues* a launch (pushes onto `PENDING_QUEUE` and spawns a scheduler task to service it) — the actual process doesn't register in the process table as "running" until that new task gets its own turn to run, which can take a little while. A first version of `start_service()` returned `Ok(())` the instant the queue push succeeded, so `is_running()` could still read `false` for a bit afterward — confirmed via a temporary boot-time test that called `stop_service()` then `start_service()` twice in a row: the second call's "already running?" check still saw `false` and launched a *second* concurrent `rtl8139d` instance (`ps` showed two `pid`s both `running=true`). Fixed two ways: a per-module `STARTING: AtomicBool` guard (via `compare_exchange`) closes the window between two *concurrent* `start_service()` calls, and `start_service()` now spin-waits (bounded, same pattern as every other mailbox wait in this codebase) for `is_running()` to actually flip `true` before returning `Ok` — so a caller never sees a false "success" for a launch that hasn't landed yet. Re-verified with the same test: the second `start_service()` call now correctly returns `Err("already running")`, and `ps` shows exactly one instance.
- **Scoped to the 4 driver services on purpose** — `kill <pid>` on a PID that isn't one of them (e.g. a one-shot `runtest`/`exec` job) reports that it can't be safely force-killed, rather than silently doing nothing or claiming success. Extending this to arbitrary processes would need each userspace program to cooperate with a similar flag (or a genuinely safe forced-preemption primitive this scheduler doesn't have yet) — out of scope for now since one-shot commands already run to completion quickly on their own.

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
- **Drag-and-drop between (or within) Files windows**: `main.rs`'s `FILE_DRAG: Mutex<Option<FileDrag>>` — armed on mousedown (see the click-deferral fix in Known Issues: the row's own click action is captured as `pending_action` and only actually runs on release if the row was never dragged), tracks whether the cursor moved past an 8px threshold (measured from the fixed mousedown position, not frame-to-frame — see the desktop-icon jankiness fix in Known Issues for why that distinction matters), and on release, if it moved, resolves via `resolve_file_drop()`: drops onto a specific directory *row* moves into that subdirectory; dropping anywhere else in a Files window's content area moves into whatever directory that window is currently browsing (the same window or a different one). The actual move is `hepfs::move_entry()` (new): re-parents the `DirEntry` from one directory to another without touching the inode or its contents/data blocks — same "just rewrite the directory entry" approach `rename()` already used, just changing parent instead of name. No cycle detection (moving a directory into its own descendant isn't guarded against) — not reachable through this UI today since a directory's own descendants never appear in its own pane, but worth adding real protection before anything ever calls `move_entry()` with arbitrary caller-supplied paths (e.g. a future `mv` shell command).
- **Shift+click range-select, now with real multi-move/multi-copy**: highlights every row between the last plain-clicked row and a Shift+clicked one, within the same pane (`HepfsNav::range_selected`). A plain click (no Shift) on a row that's *already* part of the current range keeps the whole group selected instead of narrowing to just that row — the same "click an unselected item narrows to it, click an already-selected one keeps the group" rule the desktop's own multi-select uses (found and fixed a real bug here: the click handler used to unconditionally clear `range_selected` on every plain click, before a drag/right-click on a selected row ever got a chance to see it was part of a group). Dragging any row in the group moves every row in it (`FileDrag::extra`, resolved at drag-arm time); right-clicking any row in the group and choosing "Copy" stages the whole group into `FS_CLIPBOARD` (which changed from `Option<(...)>` to `Vec<(...)>` to hold more than one entry).
- **A file drag now actually looks like a drag**: previously a row drag gave zero visual feedback until the drop — nothing indicated it had been picked up at all. `main.rs`'s new `draw_file_drag_ghost()` floats a small icon+label near the cursor (same style/icon glyph the row itself uses) whenever `FILE_DRAG` is armed and past the move threshold, drawn in the same render pass as the taskbar's own drag ghost. Needed the same fix that ghost did: a file drag on its own doesn't touch anything that marks the scene dirty, so `task_blink`'s full-vs-partial-redraw decision now also checks `FILE_DRAG`'s `moved` flag and forces a full redraw every frame while true.
- Verified with a temporary boot-time test: created two directories and a file in one, called `move_entry()`, confirmed the file disappeared from the source directory, appeared in the destination under the same inode, its content was untouched, and a no-op move (same source and destination) correctly refused. Full boot regression clean.
- **Copy/paste with Ctrl+C/Ctrl+V** (files-and-directories, separate from the plain-text clipboard editor/terminal Ctrl+C/V already used — `clipboard.rs`): Ctrl+C in a focused Files window copies whichever row is `nav.selected` into `desktop::FS_CLIPBOARD` (`(parent_ino, ino, name, is_dir)` — moved from a `main.rs`-private static to a `desktop.rs`-public one so the right-click "Copy"/"Paste" menu items below can share it, resolved via a new `selected_fs_entry()` helper — the same pane/row→entry traversal the click handler already did, just driven by the *last selected* row instead of a live click); Ctrl+V (or plain Ctrl+V — 0x16 — same as the terminal's own binding) pastes it into whichever Files window has focus, via `hepfs::copy_entry_unique()` (new — see Known Issues for why plain Ctrl+C needed a real fix here, not just this feature): unlike `move_entry()`, this makes a genuine duplicate — new inode, new data blocks, recursing into subdirectories — not just a re-pointed `DirEntry`. A name collision at the destination (e.g. pasting into the same directory it was copied from) is retried as `"<name> (1)"`, then `"(2)"`, and so on (capped at 100 attempts) until a free name is found — matches the numbered-duplicate convention rather than a single fixed `"(copy)"` suffix.
- **Right-click a row → Open/Pin/Unpin/Copy; right-click empty space → Paste**: new `ContextMenuKind::FsRow`/`FsPane` variants (the enum dropped its `Copy` derive to hold these — `name: String` isn't `Copy` — the one call site that relied on it now `.clone()`s instead). desktop.rs can detect *that* a Files window was right-clicked but can't resolve *which row* (needs the block device, which only `main.rs` touches) — so it just records `fs_context_menu_pending: (win_id, mx, my)`, and `main.rs` resolves that into the real `FsRow`/`FsPane` (with the row's ino/name/is_dir/parent path, or — for empty space or the ".." row — just `FsPane`) within the very same frame (main.rs's polling runs right after `update_mouse()` in the same loop iteration, so there's no visible one-frame flash). Clicking "Open" reuses `open_fs_entry_requested`; "Pin"/"Unpin" calls `Desktop::toggle_pinned_file()` (see "Left Pin Dock") directly — no bridging needed, since pin state is desktop.rs's own; "Copy"/"Paste" set/read `FS_CLIPBOARD` and call `hepfs::copy_entry_unique()` directly too, since `nvme::CONTROLLER` and the `hepfs` functions are already crate-visible — desktop.rs doesn't need main.rs's help for these at all, only for the row-resolution step above (which genuinely needs `HepfsNav`, a main.rs-only type).
- **Rename failures are now surfaced, not silent**: `hepfs::rename()` already refused a collision (returns `false` if the new name is taken) — but nothing ever told the user *why* a rename appeared to do nothing. `main.rs`'s prompt-result handler now checks the return value and calls a new `Desktop::show_message()` (factored out of the existing "Programs can't be renamed" toast so any caller can show one) with `"'<name>' already exists"` when it fails, for both the desktop's own rename and the file manager's.
- Verified with a temporary boot-time test: recursively copied a directory (containing a file and a nested subdirectory with its own file) into another directory — confirmed the original was untouched, the copy's file content matched, the nested subdirectory's file came along too, the copy got a genuinely different inode (not just a new name for the same one), and a same-name re-copy was correctly refused (the collision-retry path is exercised by the real Ctrl+V handler, not this test). A separate test drove the new context-menu item clicks through the real `update_mouse()`/`context_menu_item_at()` path (not just calling the underlying functions directly) for Open/Pin/Copy/Paste and the pin-dock click, all passing. Full boot regression clean.
- **Dragging a row (or multi-selection) out past any Files window now does something instead of silently failing**: `resolve_file_drop()` used to only ever look for a Files window under the drop point and give up (`return false`) if there wasn't one. It now falls through to a new `resolve_file_drop_outside_windows()` for exactly the two other places a drop can meaningfully land — dropping on the left pin dock (`desktop::in_pin_dock()`, a small public hit-test wrapping the dock's private `DOCK_W` constant so `main.rs` doesn't need its own copy of that geometry) pins the dragged entry/entries via a new `Desktop::pin_file()` (an *idempotent add*, deliberately not reusing `toggle_pinned_file()` — a drag's intent is always "pin", so toggling could unpin an already-pinned item the drag happened to land on); dropping on open desktop background (not the taskbar, not any window) moves the entry/entries into `/home/desktop` via `hepfs::lookup()` + the same `move_entry()` the inter-window drag already uses. Multi-selection drags carry every other selected row the same way `FileDrag::extra` already did for window-to-window drops. Needed a new `FileDrag::from_path` field (the dragged row's parent *path*, not just its ino) captured at drag-arm time, since `PINNED_FILES` stores pinned items by path (HepFS inodes have no reverse/parent pointer to reconstruct it later).
- Verified with a temporary boot-time test: created a directory with two files in it, built a `FileDrag` with one as the primary and the other as `extra`, dropped it (via `resolve_file_drop_outside_windows()` directly) on dock coordinates — both ended up pinned and neither moved out of the source directory — then dropped a fresh drag of the same two files on open desktop-background coordinates — both ended up under `/home/desktop` and gone from the source directory. Full boot regression clean.
- **Right pane is now a large-icon grid, with scrolling**: the right (full-listing) pane no longer draws 14px text rows — it wraps entries into a `GRID_CELL_W`×`GRID_CELL_H` (72×60) grid of icon+label cells (`icons::draw_file_icon` at 36px instead of 12px), same numbering the old row-list used (0 = ".." when not at root, then `hepfs::list_dir()` order) so every existing consumer of `(pane, idx)` — click, drag/drop target resolution, the right-click context menu — needed no changes except swapping their old `/14`-row-height math for a new shared `grid_idx_at()`/`grid_cols()` (the left tree pane is untouched: still a plain directory-name list, since it's always short — one level of subdirectories). Scrolling is in whole grid rows, not pixels (`HepfsNav::scroll: usize`, reset to 0 on every navigation) specifically so no row is ever partially clipped at the top — `render_hepfs_window()` clamps it to the actual content height every frame, and draws a track+thumb scrollbar along the pane's right edge (sized to the visible/total row ratio) whenever content overflows.
  - **Not literal mouse-wheel scrolling, despite that being the original ask — a deliberate substitution, not an oversight:** QEMU's `usb-tablet` device (this project's only pointer, see "XHCI USB Mouse Driver") reports a fixed 5-byte HID packet (buttons + absolute X/Y) with no scroll-wheel axis at all — there's no hardware signal to read here, unlike the USB-keyboard item where the gap was "untested," this one is "physically absent from the emulated device." Scrolling is driven by click-and-drag on the new scrollbar instead (`FS_SCROLL_DRAG` in `main.rs`, the same direct-manipulation-track model the Settings volume slider already used, just living in `main.rs` since it needs `hepfs::list_dir()` for the row count — `desktop.rs` doesn't touch the block device) — still "mouse-driven, sized to the window," just not the wheel specifically.
  - Verified with a temporary boot-time test: `grid_idx_at()`'s column/row math against known pixel offsets (col 0/row 0, col 1/row 0, col 0/row 1, and the same point with a 2-row scroll offset all resolved to the expected flat index); created a directory with 40 files and confirmed `hepfs_scroll_max()` reported real overflow for a small test window; forced `HepfsNav::scroll` to a wildly out-of-range value and confirmed a real `render_hepfs_window()` call clamped it back down to exactly `hepfs_scroll_max()`'s value. Full boot regression clean.
  - **Bug found right after landing (from a real screenshot, not a test): filenames overlapped into the next cell.** The label truncation cap was 9 chars (81px at 9px/char) — wider than `GRID_CELL_W` itself (72px) — so anything long enough to hit the cap (`kernel.txt` → `kernel.t…`) already overflowed its own cell before the centering math even ran; centering then clamped the resulting negative offset to 0 via `saturating_sub`, so the overflow spilled entirely rightward into the neighboring cell's label instead of splitting evenly. Fixed with a proper `GRID_LABEL_MAX_CHARS = GRID_CELL_W / 9 - 1` (one char short of the exact pixel fit, so there's still a sliver of margin between cells) and switched the truncation/centering math from byte slicing (`&name[..8]`) to `chars().count()`/`chars().take()`, since `draw_text` advances per *character* not per byte.
- **Rubber-band marquee select, matching the desktop's own** — clicking empty grid space in the right pane (an idx that doesn't resolve to a real entry) now arms `FS_MARQUEE: Mutex<Option<(win_id, start_x, start_y)>>` instead of doing nothing; every held frame after, a new `fs_marquee_hits()` recomputes which grid cells the rectangle from there to the current cursor overlaps (only considering rows actually on screen, same "fits" check `render_hepfs_window()` itself uses) and writes the result straight into `HepfsNav::selected`/`range_selected` — the *same* group-selection fields Shift+click already populates, so a marquee selection can immediately be dragged or right-click-copied as a group with zero extra plumbing (`resolve_fs_row()`/`selected_fs_entries()` don't know or care how the group was formed). Lives in `main.rs`, not `desktop.rs`, for the same reason the scrollbar drag does — it needs `hepfs::list_dir()` to know how many grid cells exist. The rectangle itself is drawn the same way the desktop's is (hollow, accent-colored, corner-to-cursor), inside `render_hepfs_window()`, reading the live cursor position directly since that function only receives a `win_id`. Scoped to the right (icon-grid) pane only — the left tree pane's empty space still does nothing on click, same as before (it's always short: one level of subdirectories, multi-select there wasn't part of the original ask).
- Verified with a temporary boot-time test: created a directory with 6 files, navigated a spawned Files window into it, and confirmed `fs_marquee_hits()` returned exactly the first visible row's two indices for a rectangle matching that row's exact pixel bounds — then, with the pane scrolled down one row, confirmed the *same on-screen rectangle* now hit the next real row's indices instead (proving the scroll offset is folded into the hit-test, not just the render). Full boot regression clean. This was also the last open item under "Real desktop shell, remaining pieces" in Next Steps — that item is now fully done and removed from the list.

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
| ~~Ctrl+C in a Files window did nothing~~ | Fixed — the real bug, not a routing/focus issue like the earlier terminal one. `ps2.rs`'s own ctrl-modifier handling turns plain Ctrl+`c` into the *control code* `0x03` before it ever reaches application code (`raw - 0x60` for a lowercase letter while Ctrl is held) — it never arrives as the literal char `'C'`. The file-manager handler only checked `c == 'C' && ctrl_held()` (true only for Ctrl+**Shift**+C, where Shift suppresses that conversion), so plain Ctrl+C — what anyone actually types — silently matched nothing. Ctrl+V happened to already work because its wrapper also checked the raw code (`c as u8 == 0x16`) as a fallback. Fixed by adding the same `c as u8 == 0x03` check for Copy. Files windows have no competing meaning for plain Ctrl+C (unlike the terminal's Ctrl+C = cancel-input, which is why *that* context deliberately only binds Ctrl+Shift+C), so there's no ambiguity to resolve here. |
| ~~Dragging a desktop icon was janky — worked on a fast flick, almost never on a slow, steady drag~~ | Fixed. `icon_drag_moved`'s threshold check compared the icon's newly-computed position against `icon.x`/`icon.y` — but those get overwritten to that same new position every single held frame, *before* the next frame's check runs. So the comparison was always against last frame's position, not the drag's actual start: only a single large jump between two consecutive polls (a fast flick) would ever exceed the 3px threshold, while a slow, controlled drag — arguably the more natural way to drag something — moved the icon in tiny per-frame increments that individually never crossed it. The practical effect: on release, `icon_drag_moved` was still `false`, so it fell through to click semantics (select/open/rename) instead of "the drag finished," which is what "sometimes works, sometimes doesn't, takes many attempts" was actually describing. Fixed by adding `icon_drag_start_pos` (captured once, at mousedown) and comparing cumulative displacement from *that*, not frame-to-frame. |
| ~~File manager row drag never actually moved the file, and dropping onto the ".." row didn't go to the parent directory~~ | The click-vs-drag ordering bug (see the taskbar/file-manager entries earlier in this table) explained why a drag felt broken, but there was also a second, separate gap: dropping specifically on the ".." row fell into the same `.unwrap_or(browsing_ino)` fallback as dropping on ordinary empty space, silently treating it as "move within the currently browsed directory" instead of "move up a level." `resolve_file_drop()` now special-cases `entry_idx == 0` when not at root, resolving the target to `nav.back.last()`'s inode (the same one clicking ".." itself would navigate to) instead of falling through. Also gave file drags actual visual feedback for the first time (a floating icon+label following the cursor, `draw_file_drag_ghost()`) — previously a drag looked completely inert until the drop, which was very easy to mistake for "not working" even once the underlying logic was fixed. |
| ~~Desktop icon dragging was janky — worked sometimes, took many attempts other times~~ | Fixed — a real bug, not flakiness. The "has this drag actually moved" check compared the new frame's position against `icon.x`/`icon.y`, which gets overwritten to that same new position every frame regardless of whether `icon_drag_moved` had been set yet — so the threshold was only ever measuring the *single most recent frame's* delta, never the cumulative distance from where the drag started. A fast flick (large single-frame delta) would cross the 3px threshold and register as a drag every time; a normal, steady drag (small per-frame deltas that add up to a large total distance) might never cross it at all, so release would fall through to click semantics (select/open/rename) instead of finishing the move — "sometimes works, sometimes doesn't" was really "fast drags work, slow ones mostly don't." Fixed by adding `icon_drag_start_pos`, captured once at mousedown, and measuring cumulative displacement against *that* fixed point instead of the continuously-updated current position. |
| ~~File manager row dragging still didn't work after the mousedown/release click-deferral fix~~ | The deferral fix (below) was real and necessary but not sufficient — investigated further by extracting the drop-resolution logic into a standalone `resolve_file_drop()` and testing it directly, which proved the move itself was already correct. The actual gap: dropping a file was only ever documented/implemented as "move into whichever directory the target window is currently browsing" — dropping directly *onto a specific subdirectory's row* (the first thing anyone would naturally try) did nothing different from dropping anywhere else in that window, so a drop that landed on a subfolder silently ended up in the parent instead of inside it, which reads as "dragging doesn't work" if that's the first thing you try. Fixed: `resolve_file_drop()` now also re-derives which row (if any) the drop point landed on, using the same nav-bar/pane/row geometry the click handler itself uses, and — if that row is a directory (and not the dragged item itself) — moves into it instead of the window's browsed directory. Verified with a temporary boot-time test: dropping a file into a subdirectory's row landed inside that subdirectory, not its parent. |
| ~~File manager had no Shift+click range-select~~ | Added. Shift+click on a row selects every row between the current anchor (`nav.selected`) and the clicked one, within the same pane — `HepfsNav::range_selected: Vec<usize>`, mirroring how the desktop's own Shift+click is a pure selection modifier. Cleared on any navigation (back/forward/`..`/into a directory) so a stale range from a different folder can't linger. Originally highlight-only (drag/Copy only ever acted on one row); a follow-up fixed the bug that caused that — the click handler unconditionally cleared `range_selected` on *every* plain click, including the very mousedown that arms a drag or opens a context menu on an already-selected row, so by the time anything checked "is this row part of a multi-selection" the answer was always no. Now a plain click on a row already in the group preserves it; dragging or right-click-Copy on any row in the group acts on the whole group (see "HepFS File Manager" above). |
| ~~Rename/New File/New Folder prompt only closed via Enter/Esc, not by clicking away~~ | Fixed. `Desktop::prompt_rect()` computes the open prompt's bounding box (mirroring `render()`'s inline-under-icon layout for `RenameIcon`, or the centered-modal layout everything else uses) and `update_mouse()` now checks any click — left or right — against it before anything else: outside dismisses (matching how the right-click context menu already dismissed on an outside click), inside falls through to normal handling. |
| ~~Dragging a taskbar button also minimized/focused/opened-a-jump-list-for the window it belonged to~~ | Fixed. The click action (open/focus/minimize/jump-list) used to fire immediately at mousedown, with drag-to-reorder armed alongside it as a "both fire from the same click" tradeoff — meaning starting a drag on a *focused* window's button minimized it the instant you pressed down, before the drag had gone anywhere. Restructured to defer the click action to release, exactly like desktop icons already did: mousedown now only arms `taskbar_dragging` and captures `taskbar_click_pending` (kind, slot, and whether this same click just dismissed a jump list); release runs `taskbar_button_click()` with that captured info *only if the button was never dragged* (`!taskbar_drag_moved`). Also gave the drag real visual feedback while at it — the button previously reordered silently on release with no feedback while held; now the dragged button lifts out into a floating "ghost" that tracks the cursor (`draw_taskbar()`'s `dragged_ghost`), which needed one more fix to actually animate: plain mouse movement only sets the cheap `mouse_dirty` partial-flush flag, not the full `dirty` redraw the taskbar needs, so `update_mouse()` now forces `dirty = true` every frame the drag is actively moving. |
| ~~File manager row drag-and-drop didn't work~~ | Fixed — same root cause as the taskbar bug above, just in `main.rs` instead of `desktop.rs`: a row's click action (navigate into a directory / open a file / start a rename) fired immediately at mousedown, so pressing down to start dragging a row would navigate into it or open it before the drag ever got anywhere, invalidating the drag (the row you meant to drag might not even be at that position anymore). `FileDrag` (armed on mousedown) now also carries `win_id`/`is_dir`/`pending_action: FileRowAction` computed once at mousedown time (the fast/slow-click timing check itself has to happen then, since it depends on *when* the click landed) but not executed until release, and only if the row was never dragged — mirroring the taskbar's fix exactly. Verified with a temporary boot-time test through real `update_mouse()` calls: Shift+click multi-select, a rubber-band marquee spanning exactly the intended icons (not their neighbors), a group drag moving every selected icon together, and — the specific regression check for this fix — dragging a *focused* window's taskbar button no longer leaves it minimized afterward. Full boot regression clean. |
| ~~Ctrl+C (and any other key) typed while a non-terminal, non-editor window had focus still landed in the main terminal~~ | Fixed. `main.rs`'s keyboard-routing `if/else` chain only special-cased the main Editor, extra editor windows, and extra terminal windows — everything else (Settings/Sysmon/Welcome/Files/ImageViewer/AudioPlayer focused, or nothing focused at all) fell through to an unconditional `terminal::TERMINAL.lock()...on_key(c)` at the very end, regardless of what was actually focused. So e.g. Ctrl+C typed while looking at the Settings window would silently clear the main terminal's input line and print `^C` there, even though the terminal wasn't visible or focused. Fixed by gating that fallback on `focused == Some(2)` (the main terminal's fixed window id) — a key typed while some other window has focus (or none does) now correctly goes nowhere instead of leaking into the terminal. |
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
| ~~With both `rtl8139d` and `hdad` running, `ps` only ever showed `hdad`, and `ping`/`wget` never worked~~ | Fixed — a real, previously-unnoticed `swapgs` bug in `scheduler::block_on_irq()` (the function backing `SYS_WAIT_IRQ`, which both driver processes now call every loop iteration to rate-limit their polling). `swapgs` is a hardware register-MSR *swap*, not a load: `syscall_entry`'s entry-side `swapgs` sets `GS.base = &PERCPU` and `IA32_KERNEL_GS_BASE` = whichever task's ring-3 GS it displaced. `block_on_irq()` was context-switching to a *different* task from inside that "mid-syscall" window — before the matching exit-side `swapgs` in `syscall_entry`'s epilogue ever ran — and `context_switch()` itself has zero GS-awareness (it only saves/restores RSP and a few GPRs). Whichever task became the second concurrent process to enter ring 3 inherited that stale, poisoned GS state on its very first syscall: its own entry-side `swapgs` flipped `GS.base` to the *blocked* task's old ring-3 GS (near zero) instead of `&PERCPU`, so `mov gs:[8], rsp` computed a physical address around `0x8` and page-faulted — root-caused by resolving the crash `rip` (`syscall::syscall_entry`, not `process::enter_ring3` as first assumed from proximity of the last diagnostic print) via a custom ELF `.symtab` scanner, since neither `objdump`/`nm`/`pyelftools` were available in this environment. `ps` only showing `hdad` was this crash killing `rtl8139d`'s very first syscall during boot, before it ever got the chance to register; every later `ps` output was already missing it. Fixed by bracketing `block_on_irq()`'s `context_switch()` call with an explicit `swapgs` immediately before (restoring "resting" GS state before yielding the CPU) and another immediately after it resumes (restoring "mid-syscall" state so the real exit-side `swapgs`+`sysretq` back in `syscall_entry` still sees what it expects). Two smaller, genuine bugs were found and fixed en route to this one, both necessary but not by themselves sufficient: (1) `async_task_entry()`'s `if let Some(x) = PENDING_QUEUE.lock().pop() { ...body... }` kept the `MutexGuard` alive for the *entire if-let body* (a real Rust temporary-lifetime footgun — the guard's scrutinee-expression lifetime extends through the block, not just the match), and since a *persistent* driver process's body never returns, this permanently deadlocked every other task's own attempt to pop a job; fixed by extracting `let popped = PENDING_QUEUE.lock().pop();` into its own statement first. (2) Every process shared one single global kernel/syscall stack (`syscall.rs`'s old `PERCPU.kernel_stack` + the TSS's `RSP0`) — a task blocked mid-syscall left its saved state pointing into a buffer a different task's next syscall/interrupt would silently overwrite; fixed by giving every `scheduler::Task` its own dedicated 16KB kernel stack (`_kstack`/`kstack_top`), switched via `scheduler::use_kstack()` at every scheduling decision point. Verified end-to-end post-fix: `ps` shows both `pid=1 name=<hdad>` and `pid=2 name=<rtl8139d>`, and a real `ping 10.0.2.2` completes with an actual `"reply from 10.0.2.2: seq=0"` (confirmed via the genuine production `net::poll()` call site in `task_blink`, which delivers into the real terminal window — not a synthetic test harness). Full clean regression boot with all temporary diagnostics removed. |
| ~~With 4 concurrent `SYS_WAIT_IRQ`-polling driver processes (once `xhcid` joined `rtl8139d`/`hdad`/`ahcid`), a driver would page-fault at a seemingly arbitrary address near the top of its own user stack shortly after its first real hardware event~~ | Fixed — a second, closely-related bug the `swapgs` fix above didn't cover: `gs:[8]` (`PERCPU.user_rsp`) is a *single* per-CPU scratch slot, not per-task. `syscall_entry`'s asm stashes the calling task's user-mode RSP there on entry and restores it from there on `sysretq`. `block_on_irq()` (backing `SYS_WAIT_IRQ`) already correctly brackets the GS *toggle* around its mid-syscall `context_switch()` (see the `swapgs` fix above), but did nothing about this *separate* shared slot — if a blocked task's `context_switch()` handed the CPU to a different task that itself later made *any* syscall (routine, once several drivers all call `SYS_WAIT_IRQ` every loop iteration), that other task's own syscall entry overwrote `gs:[8]` with *its* user RSP. When the first task was eventually resumed and its own `sysretq` fired, it restored whatever was *currently* sitting in `gs:[8]` — not its own RSP — silently handing that ring-3 code a corrupted stack pointer. Root-caused via the exact same class of evidence as the `swapgs` bug (`cr2` landing suspiciously close to, but not exactly at, `USER_STACK_TOP`, varying slightly between otherwise-identical boots) plus a systematic elimination pass: verified the newly-added `xhcid`'s own mailbox-handoff values were sane (temporary kernel-side print of every field written into the `Mailbox`), verified the crash reproduced identically with or without extra diagnostic `println!`s in `xhcid` (ruling out format-machinery stack usage), and verified `rip` stayed inside `xhcid`'s own code via the same ELF `.symtab`-scanning approach used for the original `swapgs` bug — the consistent "near-but-not-at stack top, same `rsp` across different boots" signature pointed at stack-pointer corruption from *outside* the faulting code itself, not a bug in the code at that `rip`. Fixed the same way as the `swapgs` toggle: added `syscall::get_user_rsp()`/`set_user_rsp()`, and `block_on_irq()` now saves its own `gs:[8]` value to a local before yielding (safe — nothing else can have touched it yet, since it's this task's own value freshly written by its own entry) and writes it back immediately after resuming, before anything else gets a chance to read it via this task's own eventual `sysretq`. This bug was very likely already latent for `rtl8139d`/`hdad`/`ahcid` (all three also call `SYS_WAIT_IRQ` every loop iteration) but hadn't surfaced as an observed crash — with only 2-3 concurrent blocking tasks the race window was apparently narrow/lucky enough not to trigger visibly; `xhcid` becoming the 4th pushed the odds over the edge. Verified via QEMU monitor-injected synthetic mouse events (`mouse_move`/`mouse_button` over a temporary `-monitor telnet:...` flag) reliably reaching `xhcid` → mailbox → kernel's `handle_mouse_report()` with zero crashes across repeated injections, where the exact same test crashed reliably before the fix. |
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
    - **Process execution now runs as its own scheduler task instead of blocking the desktop — the first of the three remaining sub-items above is done.** `process::exec_blocking()` (renamed from the old public `exec()`, now private) still does exactly what it always did — load the ELF, `iretq` into ring 3, block until exit — but nothing outside `process.rs` calls it directly anymore. The new `exec_async(issuer, name, data)` marks a single-job-at-a-time guard (`ASYNC_BUSY`, same restriction `net.rs`'s `start_ping`/`start_wget` already use, for the same reason: the process-global state `exec_blocking()` depends on — `USER_RUNNING`, `MMIO_NEXT_VA`, `CURRENT_PID` — was never built for two processes in flight at once), queues the job in a mailbox (`PENDING`), and hands it to `scheduler::spawn()` as a brand-new task whose entire body (`async_task_entry()`) is "run the job, stash the result, exit." `task_blink` (the desktop loop) keeps round-robining with it via ordinary timer preemption exactly the way it already shares the CPU with `task_idle` — the underlying primitive (a timer interrupt firing *while ring 3 is running* and `context_switch`ing away from it, then `iretq`-ing back in on resume) was already claimed supported by an old — and it turns out stale — comment in `process.rs`; this is the first thing that ever actually exercised it doing real, sustained concurrent work instead of a call so fast no preemption had a realistic chance to land mid-flight.
      - The terminal's `runtest`/`runhello`/`runhwtest`/`exec` commands now call `run_test_async()`/`run_hello_async()`/`run_hwtest_async()`/`exec_async()` (all take the issuing window's id) and return immediately with a "Launching..." line instead of blocking until the process exits; the eventual "`<name> exited: N`" line (with any captured stdout ahead of it) arrives later via a new `process::poll_async()`, polled once per `main.rs` frame right alongside the existing `net::poll()` for async network jobs — same delivery contract (`Option<(issuer_win_id, message)>`, single-shot, printed via `Terminal::print_async()`), so a background process and an in-flight ping/wget can coexist without either blocking the other or blocking the desktop.
      - **Bug found by the boot-time test itself, not a hunch:** the module's own top-of-file doc comment claimed "the APIC timer is masked for the duration [of `run_elf`] so the scheduler does not try to context-switch away from a process it doesn't know about" — the exact opposite of what the code actually did (the timer was already left unmasked, per a *second*, correct comment a few lines further down at the `write_cr3` call site). Harmless as dead documentation while `exec()` only ever ran from a single call site with nothing else meaningfully concurrent, but exactly backwards for reasoning about this change — fixed by rewriting the module doc to match reality and explain why that matters here.
      - Verified with a temporary boot-time test hooked into `task_blink`'s own loop (the only place the scheduler is actually live — a plain sequential kmain-time test, this project's usual pattern, can't observe scheduler behavior since the timer isn't even running yet at that point): launched `run_test_async()` and checked `job_in_progress()` in the *same expression* right after the call returned (no race — `exec_async()` never yields, so the spawned task provably hadn't run a single instruction yet; this is the one assertion that would have read `false` under the old blocking model) — then, across many further natural loop iterations with **no nested busy-wait**, confirmed `job_in_progress()` eventually went `false` on its own. A serial trace of every step inside `async_task_entry()`/`run_elf()` (added temporarily, then removed) confirmed the process genuinely ran to completion — output `"Hello from ring 3!\n"`, exit code 0 — proving `finished` wasn't a false positive from something else silently clearing the flag. (One test-design wrinkle along the way: the test's own attempt to also call `poll_async()` to double-check the result text raced against — and reliably lost to — the *production* poll site later in the same loop, since that one drains the mailbox unconditionally every frame rather than waiting on `job_in_progress()` first; not a product bug, just two consumers of one single-shot mailbox, so the test was simplified to not compete for it.) Also found and cleaned up an unrelated, already-dead `unused import: apic` warning in the same file while in there. Full boot regression clean.
    - **Permission/capability scoping for the MMIO/port-I/O syscalls — the second of the three remaining sub-items is done.** `syscall.rs` gained a fixed allowlist (`ALLOWED_PORTS`/`ALLOWED_MMIO`) that `sys_mmap_mmio`/`sys_port_in`/`sys_port_out` now check before doing anything privileged — previously any process reaching these syscalls could touch any physical address or I/O port at all. A disallowed port request returns a new `-EPERM`; a disallowed MMIO request returns `0`, the same "fail" value `sys_mmap_mmio` already used for a too-large/zero/no-process-running request, so callers don't need to distinguish the reason. `mmio_allowed()` requires the *entire* `[phys, phys+len)` range to fall inside one allowed span — a request straddling into disallowed territory is refused outright rather than partially granted.
      - **Scoped simplification, not hidden:** the allowlist is one fixed table shared by every process, not a per-process *granted* capability set (there's no manifest/registration mechanism for a process to declare, or be handed, its own narrower or wider range) — it covers exactly what this project's one real userspace client (`userspace/hwtest`) actually uses: the RTC index/data ports (`0x70`/`0x71`) and the Local APIC's 4 KB MMIO page (`0xFEE00000`). A real capability system, letting different processes see different hardware, is worth building before a second concurrently-untrusted userspace driver ever needs different ranges than this one does — not needed yet since only one process type exists.
      - Verified with a temporary boot-time test calling `syscall_dispatch()` directly (same function the real SYSCALL entry stub calls, not a reimplementation): confirmed the RTC port (allowed) and the Local APIC MMIO page (allowed, faking `USER_RUNNING` since a real request needs a live process) still succeed exactly as before, while the PIT channel-0 port `0x40` (a real, otherwise-harmless port never allowlisted) and physical address `0` (real low memory) are both refused. Full boot regression clean; `runhwtest` still exercises the exact allowed path end-to-end.
    - **IRQ-delivery-to-userspace syscall — the third and last of the three remaining sub-items is done.** A new `SYS_WAIT_IRQ(vector)` blocks the calling process until interrupt `vector` fires, instead of busy-polling for it — a real driver's normal "sleep until my device interrupts, then service it" loop, not a spin-loop. Backed by two new `scheduler.rs` primitives mirroring the existing `sleep_ms()`: `block_on_irq(vector)` marks the calling task `Blocked` with a new `waiting_irq: Option<u8>` field set (and `wake_at` pinned to `u64::MAX` so the ordinary time-based wake check in `next()` can't accidentally promote it early) and context-switches away; `wake_irq_waiters(vector)` — called from `tick()`, since the timer is the only interrupt with a real handler at all today — promotes every task blocked on that vector back to `Ready`. Exposed to userspace as `hepos_rt::sys_wait_irq(vector)`, same thin-wrapper style as the existing `sys_port_in`/`sys_mmap_mmio`.
      - **Scoped simplification, not hidden:** no real device in this kernel is interrupt-driven yet (XHCI/HDA/NVMe/AHCI/RTL8139 are all polled from `task_blink`), so there's no actual driver IRQ to hand this to today — `userspace/hwtest` (the only real consumer) waits on the timer's vector (`0x20`, hardcoded to match `apic::TIMER_VECTOR` the same way its RTC ports/APIC physical address already were, since userspace can't `use` a kernel-crate constant) purely to prove the block/wake mechanism against a real, always-firing interrupt. Wiring an actual device's IRQ to call `wake_irq_waiters()` is a separate, currently-unneeded follow-up.
      - Verified by rebuilding `userspace/hwtest` (added the `SYS_WAIT_IRQ` call and rebuilt the userspace workspace directly — `build.sh` only bakes in whatever ELF is already sitting under `userspace/target/...`, it doesn't rebuild userspace itself) and launching it as a real background process via a temporary boot-time hook into `task_blink` (same pattern the async-exec test used). Confirmed via the raw serial log — every `SYS_WRITE` already echoes straight to serial (see `sys_write`), so this didn't need to compete with the production `poll_async()` consumer the way an earlier test's mailbox check did — that the real ring-3 process printed `"hwtest: waiting for a timer interrupt via SYS_WAIT_IRQ..."`, genuinely blocked, then printed `"hwtest: woke up from SYS_WAIT_IRQ"` followed by `"hwtest: done"` and exited normally, all without the desktop loop's own iteration counter ever stalling. Full boot regression clean.
      - **All three of item #1's original remaining sub-items — async process execution, MMIO/port-I/O permission scoping, and IRQ delivery to userspace — are now done.**
    - **Real multi-process support — added when migrating RTL8139 to userspace turned out to need it.** A NIC driver needs to run *forever*, servicing send/receive continuously — fundamentally unlike `hello`/`hwtest`/`exec <file>`, which run once and exit. The process model up to this point assumed exactly one process in flight at a time (`USER_RUNNING`, `KERNEL_RETURN_RSP`, `EXIT_CODE`, `MMIO_NEXT_VA`, `CURRENT_PID` were all single globals) — a persistent driver process would have occupied that one slot forever, making `runtest`/`runhello`/`exec` permanently unusable the moment networking started. Rather than accept that regression, this makes concurrent processes a real, first-class thing:
      - Every one of those former globals is now a per-task `ProcSlot` (`process.rs`), indexed by a new `scheduler::current_task_index()` (the task's bounded *array position*, not its ever-incrementing `id` — stable and small even after a long session's worth of spawns). Each `exec_async()` call gets its own scheduler task with its own private page tables, so several processes can genuinely be in flight at once: one blocked in `SYS_WAIT_IRQ`, another mid-syscall, another just starting.
      - **A real, previously-latent correctness gap this surfaced and fixed:** `context_switch()` never saved/restored CR3. That was invisible with only one process ever active — every *other* task (idle/blink) implicitly shared whatever CR3 that one process last set, and it never actually mattered which task ran under the "wrong" page tables because everything they needed lived in the shared kernel high-half every process's PML4 already copies. With two *different* private low-half address spaces now possibly both live, that stops being an accident that happens to work — `scheduler::Task` gained a `cr3` field, `next()` returns the target task's CR3 alongside its RSP, and all four blocking call sites (`tick`/`exit_current`/`sleep_ms`/`block_on_irq`) `write_cr3()` before switching stacks. `process.rs` calls a new `scheduler::set_current_cr3()` right after every `write_cr3()` it does itself, so a task's own address-space changes stay in sync with what a future preemption restores.
      - `enter_ring3`'s naked ASM used to stash the return RSP into a single fixed global symbol (`KERNEL_RETURN_RSP`) — now takes a third argument, a raw pointer into the *calling task's own* `ProcSlot`, computed by ordinary Rust in `run_elf()` before the call (much simpler and safer than trying to do a per-task lookup from inside the naked function itself, which was the first design considered and discarded — juggling registers around an inserted function call in the middle of an already-delicate hand-written prologue was too fragile to be worth it for what a caller-computed pointer accomplishes in one line).
      - The old single-job guard is gone: `exec_async()` no longer refuses a second launch. `PENDING_QUEUE`/`DONE_QUEUE` (both now `Vec`s, not single `Option`s) hold as many in-flight jobs as there are tasks running them; `job_in_progress()` is now an informational count (`ACTIVE_JOBS`), not a launch-blocking guard. The per-process stdout capture buffer (`PROC_OUT`) moved from one shared `Vec<u8>` to a sparse `Vec<(task_index, Vec<u8>)>` — the same "list of (key, value)" shape `HEPFS_NAVS`/`EXTRA_TERMINALS` already use elsewhere in this codebase — so two processes' captured output can no longer interleave into each other's.
      - Verified with a temporary boot-time test: launched `hwtest` (which blocks on `SYS_WAIT_IRQ` for at least one tick) immediately followed by the plain `<test>` ELF on a second task, and confirmed via `job_in_progress()` that the second launch was accepted *while the first was still running* (the old guard would have refused it outright). The raw serial log showed both processes' expected output completing correctly and interleaved in scheduling order rather than strictly sequential (`<test>`'s "Hello from ring 3!" line landed *before* `hwtest`'s own first line, despite `hwtest` having been launched first) — real evidence of concurrent execution, not two processes that merely happened to queue back-to-back. No crash, no corruption of either process's captured output or CR3 state. Full boot regression clean.
    - **RTL8139 migrated to userspace — the first real driver migration, not just a proof-of-concept.** `kernel/src/rtl8139.rs`'s one-time hardware bring-up (PCI enable, BAR discovery, reset, DMA buffer allocation, initial register programming) stays in the kernel — it needs `pmm`/PCI access no ring-3 process has — but the *ongoing* per-packet TX/RX polling that used to run in-kernel now runs as a new persistent userspace process, `userspace/rtl8139d`, launched once at boot and never expected to exit. `net.rs` needed **zero changes**: `Rtl8139::send()`/`recv()`/`.mac` kept their exact old signatures, just reimplemented underneath to write/read a shared-memory `Mailbox` page instead of poking hardware registers directly.
      - **Two new pieces of infrastructure this needed, neither of which existed before:**
        - **Dynamic (runtime-granted) allowlist entries** (`syscall.rs`): the existing `ALLOWED_PORTS`/`ALLOWED_MMIO` were fixed compile-time tables, fine for hardware at a known-in-advance address (RTC ports, the Local APIC) but useless for RTL8139's I/O base (a PCI BAR) or its DMA buffer physical addresses (only known once `pmm::alloc_contiguous()` actually allocates them at boot). New `syscall::grant_port_range()`/`grant_mmio_range()` let the kernel grant exactly the ranges a specific driver needs, once, right after discovering them — `rtl8139::init()` calls both for the I/O base, the mailbox page, and the TX/RX DMA buffers before ever spawning the driver process.
        - **A launch argument for new processes**: `rtl8139d` needs to learn its mailbox's physical address somehow, and that address doesn't exist until runtime — there's no way to bake it into the ELF. `enter_ring3()` gained a fourth argument (`arg`, landing in RDI so `_start(arg: u64)` sees it as an ordinary first SysV parameter — existing zero-argument `_start()`s just ignore it), threaded through `run_elf()`/`exec_blocking()`/a new `exec_async_with_arg()`. `rtl8139::init()` passes the mailbox's physical address this way; every other process (`hello`/`hwtest`/`exec <file>`) just passes 0.
      - **The `Mailbox`**: one shared physical page — `io_base`/`tx_phys`/`rx_phys` (written once by the kernel before the driver starts, so the driver knows where its own resources live), a `tx_len`/`tx_buf` slot (kernel writes a length + fills the buffer to request a send; the driver clears the length back to 0 once it's actually handed the packet to hardware), and an `rx_ready`/`rx_len`/`rx_buf` slot (the reverse — driver fills it when a real packet arrives, kernel clears `rx_ready` once it's copied the packet out). Its `#[repr(C)]` layout is duplicated byte-for-byte between the kernel (`kernel/src/rtl8139.rs`) and the driver (`userspace/rtl8139d/src/main.rs`) — there's no shared crate between them to enforce this, since userspace crates can't depend on kernel code at all (different target, no `std`, different address space).
      - **A real bug found (and fixed) proving this wasn't a rubber-stamp migration:** the very first attempt spawned `rtl8139d` from directly inside `rtl8139::init()` — which runs during early hardware bring-up, *before* `kmain` registers the scheduler's idle/blink tasks. `scheduler::spawn()` that early collides with the "task 0 becomes kmain's own execution context" bootstrap trick (see `main.rs`'s scheduler-registration comment): the driver's freshly-built task slot landed at array index 0, got silently overwritten by kmain's own live context on the very first preemption, and the driver never ran at all — confirmed by its startup `println!` never appearing in the serial log even after many seconds of uptime. Fixed by deferring the actual spawn: `init()` now just stashes the mailbox's physical address in `PENDING_DRIVER_MAILBOX`, and a new `rtl8139::spawn_pending_driver()` (a cheap no-op after the first call) does the real spawn from inside `task_blink`'s own loop, where the scheduler is guaranteed to already be fully live.
      - **A second, subtler bug found via real networking, not a synthetic test:** with the driver correctly launched, TX and RX both worked individually (confirmed via temporary serial tracing — the driver correctly detected and even processed one incoming packet), but a real `ping 10.0.2.2` round-trip only completed reliably when execution was slowed down by heavy diagnostic logging — with logging removed, it could take an unbounded amount of real time to resolve, if it resolved at all. Root cause: `rtl8139d`'s poll loop never yielded at all (tight spin, checking hardware every single iteration) — added as a *third* always-runnable task, it now shared the round-robin with `task_blink`, but the CPU never went idle even when neither task had real work to do, since `task_idle`'s `hlt` was previously the only thing giving the host machine real breathing room. Under this project's single-vCPU TCG emulation, a guest that's 100% CPU-pegged can starve QEMU's own host-side I/O/SLiRP-handling thread from ever getting scheduled by the host OS — meaning the actual network round-trip (not just in-guest scheduling) was being delayed indefinitely. Fixed by having `rtl8139d` call the already-built `SYS_WAIT_IRQ` on the timer vector once per loop iteration, rate-limiting it to about once per ~10ms tick instead of spinning — the same primitive `hwtest` already proved out, just used here for its rate-limiting side effect rather than to wait on a real device interrupt (this NIC has none — see below). Real cost: ~10ms of added latency per send/receive, a trivial trade for a NIC that isn't remotely latency-critical.
      - Verified with `ping 10.0.2.2` end-to-end through the complete real pipeline — kernel `Rtl8139::send()` → mailbox → `rtl8139d` → real `SYS_PORT_OUT` register writes → actual RTL8139 hardware TX → QEMU SLiRP → real ICMP echo reply → actual RTL8139 hardware RX → `rtl8139d` polling `SYS_PORT_IN`/reading the mapped RX ring via `SYS_MMAP_MMIO` → mailbox → kernel `Rtl8139::recv()` → `net.rs`'s existing ICMP-reply matching — producing a genuine `"reply from 10.0.2.2: seq=N"`, confirmed reproducible across multiple independent clean boots with all temporary diagnostic logging removed. Full boot regression clean.
      - **Scoped simplification, not hidden:** RTL8139 has no real interrupt of its own to wait on (`IMR` is left at 0, same as the old in-kernel driver) — `rtl8139d` polls, same fundamental strategy the kernel driver always used, just now rate-limited via the timer rather than a busy loop. A real interrupt-driven driver (waiting on *its own device's* IRQ instead of the timer) needs actual IRQ routing from a PCI device to a vector `SYS_WAIT_IRQ` can wait on — not built here, since this NIC doesn't use interrupts at all in this driver's design.
    - **HDA audio migrated to userspace too — the second real driver migration, reusing the exact RTL8139 pattern.** `kernel/src/hda.rs`'s one-time bring-up (PCI enable, MMIO mapping, controller reset, codec presence check, TSC calibration) stays in the kernel; the ongoing work — sending codec verbs via the Immediate Command interface, programming the stream descriptor, and tracking playback position through a zero-buffer→drain→stop sequence — now runs in a new persistent process, `userspace/hdad`. Every caller of `hda`'s public API (`beep`/`play_pcm`/`poll`/`is_playing`/`progress_ms`/`set_volume`/`get_volume`/`is_available` — used by the terminal's `beep`/`volume` commands, the Audio Player window, and the desktop's volume slider) needed **zero changes**, same as `net.rs` needed none for RTL8139.
      - Same shared-`Mailbox`-page shape as RTL8139's: `mmio_phys`/`buf_phys`/`bdl_phys`/`sd_off`/`tsc_per_ms` written once by the kernel so `hdad` knows where its resources live, a `play_request` slot (kernel writes a sample count once it's copied PCM data into the shared 1 MB reusable buffer — pre-allocated once at `init()`, reused for every clip as a fixed DMA buffer rather than allocated fresh per call, so no allowlist grant is ever needed after boot), a `volume` field the driver re-applies via verb whenever it changes, and `is_playing`/`elapsed_ms`/`total_ms` status fields the kernel-side `is_playing()`/`progress_ms()` now just read straight out of the mailbox.
      - Reused, rather than rediscovered, two lessons from RTL8139 up front: the driver spawn is deferred to `task_blink`'s loop via the identical `spawn_pending_driver()` pattern (avoiding the "task 0 bootstrap corruption" bug found the hard way there), and `hdad`'s loop calls `SYS_WAIT_IRQ` on the timer vector every iteration from the start, instead of spinning at 100% CPU and only discovering the host-scheduler-starvation problem after it broke something.
      - **One new thing this migration needed that RTL8139's didn't:** TSC access from ring 3. `hdad`'s position-tracking state machine needs wall-clock time without any syscall round-trip overhead (checked every loop iteration). Turns out no new plumbing was needed at all — `rdtsc` is an unprivileged instruction and this kernel never sets `CR4.TSD`, so ring 3 can already execute it directly, exactly like ring 0 does. `hda::init()` calibrates `TSC_PER_MS` once (still needs PIT port I/O, kernel-only) and hands the value to `hdad` via the mailbox.
      - Verified with a temporary boot-time test: called the real `hda::beep(440, 300)`, confirmed `is_playing()` flipped true with `progress_ms()` correctly reporting `(0, 300)` (matching the requested 300ms duration), and confirmed it later flipped back to false on its own once the clip (plus drain) finished — the complete real round trip: kernel writes samples into the shared PCM buffer → mailbox `play_request` → `hdad` sends real codec verbs via `SYS_PORT`-free MMIO (`SYS_MMAP_MMIO`-backed reads/writes) → real stream DMA → position tracked entirely in `hdad` via its own `rdtsc()` → status mirrored back through the mailbox → kernel's `is_playing()`/`progress_ms()`. Full boot regression clean.
    - **AHCI (SATA) migrated to userspace too — the third real driver migration, and the first with a *synchronous* request/response shape instead of RTL8139/HDA's fire-and-forget async-polled work.** `kernel/src/ahci.rs`'s one-time bring-up (PCI enable, ABAR mapping, port reset, CLB/FB/CTBA allocation, IDENTIFY) stays in the kernel; the ongoing per-request work — building the command FIS + PRDT, issuing the command, polling for completion — now runs in a new persistent process, `userspace/ahcid`. Chosen as the next migration specifically because, unlike NVMe (the actual HepFS boot disk in this dev setup), AHCI is **not on the critical path today** — `hepfs::BlockDev::Ahci` exists as an enum arm but nothing in `main.rs` ever constructs it (HepFS always runs on `BlockDev::Nvme`) — so a bug here couldn't break the filesystem, making it a genuinely lower-blast-radius choice than XHCI/NVMe/GOP despite disk I/O's synchronous shape sounding scarier at first glance.
      - **The synchronous-IPC shape**: `read_sectors()`/`write_sectors()` keep their exact old blocking signatures — callers expect data back immediately, not a poll-later state machine — so unlike RTL8139/HDA (which just write/read a mailbox and return), these now write a request into the mailbox and then spin-wait (bounded, ~500M iterations) on a `status` field going non-zero. This works without deadlocking because the spin-wait does nothing to *cause* `ahcid` to run — this project's real preemptive scheduler (see the `TICK_COUNT`/`swapgs` fixes above) naturally context-switches to `ahcid` on the next timer tick regardless of what the spinning kernel code is doing, the same way any two ordinary tasks share the CPU.
      - **The `Mailbox`**: `abar_phys`/`clb_phys`/`ctba_phys`/`port_base`/`sector_size` (written once so `ahcid` knows where its resources live — note `FB`'s physical address is *not* included, since neither side ever reads the FIS-receive area), an `op`/`status`/`lba`/`count` request/response pair, and a fixed 4096-byte `data` bounce buffer — deliberately sized to exactly match `hepfs::BLOCK_SIZE`, the only transfer size any real caller ever uses, so every request fits in one mailbox and `ahcid` never needs a fresh MMIO grant for the caller's own arbitrary per-call physical buffer (transfers always bounce through the mailbox's own fixed buffer; the kernel `memcpy`s into/out of it before/after the mailbox round-trip, since the kernel — unlike `ahcid` — can already touch any physical page directly). A new `AHCI_IO_LOCK` serializes whole transactions (request write → spin-wait → result copy) against each other, since `CONTROLLER`'s own lock is deliberately *not* held across the spin (so an unrelated caller checking `is_available()`/`sectors` mid-transfer doesn't block on a disk operation that might still be spinning) — without a dedicated lock, two concurrent callers could interleave their request fields or stomp each other's still-in-flight `data`.
      - **A real bug found and fixed before this ever booted correctly:** the `Mailbox` struct's header fields plus its 4096-byte `data` buffer add up to just over one page (4152 bytes) — `pmm::alloc_page()` (what RTL8139/HDA's smaller mailboxes could get away with) only allocates one page, so the kernel silently under-granted the MMIO range (`grant_mmio_range(mailbox_phys, 4096)`) and under-sized the actual allocation, and `ahcid`'s own `SYS_MMAP_MMIO` request for the full `size_of::<Mailbox>()` was rejected by the allowlist check (`end <= hi` failed) — surfaced immediately and unambiguously as `"ahcid: failed to map mailbox — exiting"` in the very first boot test, rather than as silent corruption, since the syscall layer's allowlist check fails closed. Fixed by switching to `pmm::alloc_contiguous(MAILBOX_PAGES)` (2 pages) with a compile-time `assert!(size_of::<Mailbox>() <= MAILBOX_PAGES * 4096)` guard against this ever silently regressing again.
      - Reused, rather than rediscovered, the RTL8139/HDA lessons up front: `spawn_pending_driver()` deferred to `task_blink`'s loop from the start, and `ahcid`'s loop calls `SYS_WAIT_IRQ` on the timer vector every iteration from the start (~10ms of added latency per disk operation — a trivial trade for a driver that was never going to be latency-critical here).
      - Verified with a temporary boot-time test: wrote a known 0xAB byte pattern to a scratch LBA on the SATA disk via the real `ahci::write_sectors()` → mailbox → `ahcid` → real command-issue-and-poll → actual AHCI hardware pipeline, then read it back via the same real pipeline in reverse and confirmed every byte matched — `write_ok=true read_ok=true data_matches=true`. Full boot regression clean with all temporary diagnostics removed.
      - **What's still not done (before XHCI below):** every other in-kernel driver (XHCI, NVMe, PCI, ACPI, GOP). RTL8139, HDA, and AHCI were all deliberately chosen as low-blast-radius pilots (networking, audio, and a disk that isn't actually the boot disk — nothing that would break input, the real filesystem, or rendering if something went wrong). The pattern established across all three (mailbox IPC, dynamic allowlist grants, launch arguments, rate-limited polling via `SYS_WAIT_IRQ`, deferred task spawn, and now synchronous spin-wait-on-mailbox for request/response-shaped hardware) should transfer to the next one, but each remaining driver has its own hardware-specific hot-path logic to port, and the higher-stakes ones (XHCI for mouse/keyboard, NVMe for the actual HepFS boot disk, GOP for the framebuffer) warrant real caution before attempting — a bug there breaks the desktop's basic usability, not just one feature.
    - **XHCI (USB HID) migrated to userspace too — the fourth real driver migration, and the first genuinely higher-stakes one (a bug here breaks all mouse/keyboard input, not just an unused code path).** `kernel/src/xhci.rs`'s one-time bring-up (HC reset, port power/reset, and the Enable-Slot/Address-Device/Configure-Endpoint command sequence for the mouse and optional keyboard) stays in the kernel — every step there is a synchronous command/wait-for-completion exchange only ever run once per device at boot, needing `pmm`/PCI access no ring-3 process has. The *ongoing* work — draining the event ring for completed HID interrupt-IN transfers and re-queuing the next one — now runs in a new persistent process, `userspace/xhcid`, reusing RTL8139's async fire-and-forget mailbox shape (not AHCI's synchronous request/response one): `xhcid` copies each completed report's raw 8 bytes into a small ring in the mailbox and moves on; the kernel drains that ring once per frame (`poll_mouse()`, called from the same place it always was). Critically, **the actual HID→PS/2/mouse-position translation logic (`handle_mouse_report()`/`handle_kbd_report()`) never moved and needed zero changes** — it's still fed raw bytes, just sourced from the mailbox instead of read directly off `hid_buf_v`, so `main.rs`'s only caller (`xhci::poll_mouse(fb_w, fb_h)`) needed zero changes either.
      - **The `Mailbox`**: `bar_phys`/`evt_phys`/`cap_len`/`db_off`/`rt_off` (so `xhcid` can compute the same `op`/`db`/`rt` register-set offsets within one 65536-byte BAR mapping the kernel already discovered), the event ring's *current* consumer position (`evt_i`/`evt_c` — not reset to 0/1, since the kernel's own bring-up already advanced past several command-completion events), and one `DeviceInfo` per HID device (`present`/`slot`/`hid_i`/`hid_c`/`hid_phys`/`hid_buf_phys` — again carrying the ring position the kernel's own initial `queue_hid()` call already advanced past, which `xhcid` must continue from rather than re-initialize). A 32-entry SPSC report ring (`head`/`tail` + `[Report; 32]`, each just a `kind`+8 raw bytes) carries completed reports from `xhcid` to the kernel — sized generously since, unlike a dropped network packet, a dropped keyboard edge would be a real regression, not just a performance blip; in practice the kernel drains it far faster than `xhcid`'s own `SYS_WAIT_IRQ`-rate-limited poll can fill it.
      - **A real, previously-latent cross-task kernel bug this migration's testing surfaced (not a bug in `xhci.rs`/`xhcid` itself)** — see the Known Issues entry above: `PERCPU.user_rsp` (`gs:[8]`) turned out to be a second piece of per-CPU-not-per-task state the earlier `swapgs` fix didn't fully cover, and `xhcid` becoming the 4th concurrent `SYS_WAIT_IRQ`-polling driver was what finally made the race land reliably enough to reproduce and fix. Root-caused via QEMU-monitor-injected synthetic mouse events (a temporary `-monitor telnet:127.0.0.1:4445,server,nowait` flag plus a small Python script sending `mouse_move`/`mouse_button` HMP commands) reliably reproducing a page fault near the top of `xhcid`'s own user stack on its very first real HID transfer — and, after the fix, reliably *not* reproducing it across repeated injections.
      - Verified end-to-end post-fix with the same QEMU-monitor injection technique: synthetic `mouse_move`/`mouse_button` events flow through the real pipeline — QEMU `usb-tablet` → actual XHCI hardware interrupt-IN completion → `xhcid` dequeuing the event ring → mailbox → kernel's `poll_mouse()` draining the report ring → the unchanged `handle_mouse_report()` — with zero crashes across repeated injections, all temporary diagnostics removed. Full clean regression boot with all four userspace drivers (`rtl8139d`/`hdad`/`ahcid`/`xhcid`) launching successfully. (Real keyboard input wasn't exercised in this pass — the shared dev QEMU scripts don't attach `usb-kbd` by default, same tradeoff noted elsewhere in this doc — but the driver correctly detects and reports `kbd false` when absent, matching the pre-migration behavior exactly.)
      - **What's still not done:** NVMe (the actual HepFS boot disk), PCI/ACPI (one-shot enumeration/shutdown, not really "drivers" with an ongoing hot path to migrate), and GOP (the framebuffer). NVMe and GOP remain the two highest-stakes migrations left — a bug in either breaks the desktop's basic usability (no real filesystem, or no display at all) rather than one feature.
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
