# HepOS

A custom x86\_64 operating system written in Rust from scratch — no Linux, no POSIX, no libc.

HepOS is an **exokernel**: the kernel only multiplexes hardware (memory, storage, input, display). All OS abstractions (filesystem, windowing, shell) live in a kernel-space libOS for now, with userspace planned as the next major milestone.

---

## Features

- **Graphical desktop** — floating window manager, drag-to-move, drag-to-resize, z-order compositor
- **Start menu & taskbar** — lists all programs, shows only open windows, live clock
- **Terminal shell** — 30+ commands, command history, left/right cursor movement, tab completion
- **Text editor** — syntax-free editor with Ctrl+F find, PgUp/Dn, Ctrl+Home/End, F2 save
- **HepFS file manager** — directory navigation, back/forward/path bar, click files to open
- **HepFS filesystem** — custom flat-inode FS on NVMe, files up to ~4.1 MB (12 direct + 1024 indirect blocks)
- **XHCI USB driver** — USB HID tablet for absolute mouse coordinates in QEMU
- **NVMe driver** — admin + IO queues, custom queue management
- **Networking** — RTL8139/e1000 drivers, ARP, ICMP ping (TX works; RX broken on QEMU/Windows SLiRP)
- **Sysmon window** — live RAM bar, uptime, PCI device list, storage/net status
- **Settings app** — background picker (dark gradient / Windows-XP-style Bliss), right-click desktop for a context menu
- **Preemptive scheduler** — round-robin, APIC timer, context switch in naked asm
- **x2APIC** — MSR-mode APIC (no MMIO mapping needed)
- **PS/2 keyboard** — full scancode set 1, shift/caps/ctrl, extended keys
- **Ring-3 userspace** — SYSCALL/SYSRET, per-process page tables, ELF loader, `exec`/`ps` from the shell
- **HepBL** — our own UEFI bootloader, written from scratch in Rust (no Limine, no external bootloader crates)
- **Networking** — hand-written TCP stack, DNS resolver, `wget <host>[:<port>]` HTTP client
- **Intel HDA audio** — `beep [hz] [ms]` via a real DMA PCM stream

---

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│  libOS (kernel space, Rust)                             │
│  desktop · terminal · editor · HepFS navigator          │
├─────────────────────────────────────────────────────────┤
│  Kernel                                                 │
│  PMM · VMM · Heap · GDT · IDT · APIC · Scheduler       │
│  Syscall gate · Per-process page tables · ELF loader    │
├────────────┬────────────┬────────────┬──────────────────┤
│  Storage   │  Display   │  Input     │  Network         │
│  NVMe      │  GOP FB    │  PS/2 kbd  │  RTL8139 / e1000 │
│  HepFS     │  SW render │  XHCI USB  │  TCP/DNS/wget    │
└────────────┴────────────┴────────────┴──────────────────┘
        ↑ handed off from kernel.elf's entry point
┌─────────────────────────────────────────────────────────┐
│  HepBL — our own UEFI bootloader (hepbl/, pure Rust)    │
│  GOP mode select · ELF64 loader · page tables + HHDM    │
└─────────────────────────────────────────────────────────┘
        ↑ loaded from \EFI\BOOT\BOOTX64.EFI
Hardware: x86_64, UEFI boot (OVMF/TianoCore firmware under QEMU)
```

**Language:** Rust nightly (`no_std` + `alloc`)  
**Bootloader:** [HepBL](hepbl/) — written from scratch for this project, in Rust, UEFI-only  
**Target:** `x86_64-unknown-none` (kernel), `x86_64-unknown-uefi` (bootloader)

---

## Prerequisites

### Windows

| Tool | Where to get |
|------|-------------|
| Rust (nightly) | https://rustup.rs — then `rustup toolchain install nightly` |
| `x86_64-unknown-none` target | `rustup target add x86_64-unknown-none` |
| `x86_64-unknown-uefi` target | `rustup target add x86_64-unknown-uefi --toolchain nightly` (build.ps1 also adds this automatically) |
| rust-src component | `rustup component add rust-src --toolchain nightly` |
| QEMU | https://www.qemu.org/download/#windows — install to `C:\Program Files\qemu\` (ships with OVMF/edk2 firmware, used automatically) |
| Git | https://git-scm.com |

> MSYS2/xorriso are **no longer needed** — HepOS dropped the ISO + Limine pipeline in favor of HepBL + a plain FAT boot directory that QEMU mounts directly.

### Linux (Debian/Ubuntu)

```bash
# Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup toolchain install nightly
rustup target add x86_64-unknown-none
rustup target add x86_64-unknown-uefi --toolchain nightly
rustup component add rust-src --toolchain nightly

# Build tools
sudo apt install qemu-system-x86 ovmf gcc git make
```

### Linux (Arch)

```bash
rustup toolchain install nightly
rustup target add x86_64-unknown-none
rustup target add x86_64-unknown-uefi --toolchain nightly
rustup component add rust-src --toolchain nightly

sudo pacman -S qemu-system-x86 edk2-ovmf gcc git make
```

> **Note:** `build.sh`'s OVMF auto-detection currently looks for `edk2-x86_64-code.fd` next to the `qemu-system-x86_64` binary (matching the bundled Windows QEMU package layout this project was developed against). Native Debian/Arch OVMF packages install to `/usr/share/OVMF/` or `/usr/share/edk2-ovmf/` with different filenames — if the script can't find the firmware, point `CODE_FD`/`VARS_FD` in `build.sh` at your distro's actual OVMF paths.

---

## Building & Running

### Clone

```bash
git clone https://github.com/The-Hep-Group/HepOS.git
cd HepOS
```

No submodules or extra clones needed — HepBL replaced Limine, so there's no external bootloader binary to fetch.

### Windows

```powershell
.\build.ps1
```

That's it. The script:
1. Builds the userspace workspace, then the kernel (`cargo +nightly build --release`)
2. Builds HepBL, our own UEFI bootloader (`hepbl/`, target `x86_64-unknown-uefi`)
3. Assembles an `esp/` directory: `EFI/BOOT/BOOTX64.EFI` (HepBL) + `kernel.elf`
4. Copies OVMF firmware (ships with QEMU) and creates a writable NVRAM file (`hepbl_vars.fd`) if missing
5. Creates `hepos_disk.img` (512 MB NVMe image) if missing
6. Launches QEMU, booting UEFI → HepBL → HepOS

**Requirements:** QEMU at `C:\Program Files\qemu\` (its bundled `share/edk2-x86_64-code.fd` is used as the UEFI firmware).

### Linux

```bash
chmod +x build.sh
./build.sh
```

Same steps as the Windows script.

### Build only (no QEMU launch)

```bash
# Windows
Push-Location kernel
cargo +nightly build --release
Pop-Location

# Linux
cd kernel && cargo +nightly build --release && cd ..
```

Output: `kernel/target/x86_64-unknown-none/release/hepos-kernel`

---

## QEMU Command (manual)

If you want to run a pre-built `esp/` directory without the build script:

```bash
qemu-system-x86_64 \
  -M q35 \
  -cpu qemu64,+x2apic \
  -m 256M \
  -drive if=pflash,format=raw,readonly=on,file=edk2-x86_64-code.fd \
  -drive if=pflash,format=raw,file=hepbl_vars.fd \
  -drive format=raw,file=fat:rw:esp \
  -fw_cfg name=opt/ovmf/X-PciMmio64Mb,string=0 \
  -drive file=hepos_disk.img,if=none,id=nvme0,format=raw \
  -device nvme,serial=heposv1,drive=nvme0 \
  -netdev user,id=net0 \
  -device rtl8139,netdev=net0 \
  -device intel-hda \
  -device hda-duplex \
  -device qemu-xhci,id=xhci \
  -device usb-tablet,bus=xhci.0 \
  -vga std \
  -display sdl \
  -serial stdio \
  -no-reboot \
  -no-shutdown
```

> `-drive format=raw,file=fat:rw:esp` mounts the `esp/` directory as a virtual FAT disk (QEMU's VVFAT) — no ISO or disk image needed for booting; just drop files into `esp/EFI/BOOT/` and `esp/kernel.elf`.  
> `-device usb-tablet` gives absolute mouse coordinates via XHCI — the mouse works out of the box without grabbing.  
> `-serial stdio` prints kernel debug output (boot messages, panics) to your terminal — this also captures HepBL's own boot log (GOP mode pick, kernel load, page table setup) before the kernel takes over.

---

## Usage

### Mouse

Click any window to focus it and bring it to the front. Click the title bar and drag to move. Drag the small handle at the bottom-right corner to resize.

### Taskbar

- **HepOS button** (left) — opens the start menu listing all programs
- **Window buttons** — one per open (non-minimized) window; click to focus, click again to minimize
- **Clock** (right) — live RTC time

### Terminal Commands

| Command | Description |
|---------|-------------|
| `help` | List all commands |
| `ls [path]` | List directory |
| `cd <dir>` | Change directory |
| `cat <file>` | Print file |
| `mkdir / touch / rm` | Create / delete |
| `cp <src> <dst>` | Copy file |
| `mv <src> <dst>` | Move / rename |
| `write <file> <text>` | Write text to file |
| `edit <file>` | Open in text editor |
| `lspci` | List PCI devices |
| `sysinfo` | Kernel info |
| `mem` | RAM usage |
| `date` | Current date/time |
| `ping <ip>` | ICMP ping |
| `ifconfig` | Network info |
| `shutdown / reboot` | Power off / restart |

**Terminal shortcuts:** `Tab` = complete command or filename · `↑/↓` = history · `←/→` = move cursor · `Ctrl+A/E` = line start/end · `Ctrl+C` = cancel · `Ctrl+L` = clear

### Text Editor

Open with `edit <filename>` in the terminal, or click a file in HepFS.

| Key | Action |
|-----|--------|
| `F2` / `Ctrl+S` | Save |
| `F10` / `Ctrl+Q` | Close |
| `Ctrl+F` | Find (type query, `Enter`/`Ctrl+G` = next, `ESC` = close) |
| `PgUp` / `PgDn` | Scroll one screen |
| `Ctrl+Home` / `Ctrl+End` | File start / end |

### Sysmon Window

Open from the start menu. Shows live RAM usage bar (colour-coded green/orange/red), uptime counter, NVMe and network status, and a full PCI device list.

---

## Project Structure

```
HepOS/
├── kernel/
│   ├── src/
│   │   ├── main.rs        # kmain, task_blink, global state, window rendering
│   │   ├── bootinfo.rs    # HepBL boot protocol structs (kept in sync with hepbl/src/main.rs)
│   │   ├── desktop.rs     # WM: windows, taskbar, start menu, compositor, wallpaper, context menu
│   │   ├── terminal.rs    # Shell with 30+ commands, tab completion
│   │   ├── editor.rs      # Text editor with find mode
│   │   ├── hepfs.rs       # Custom filesystem (flat inode, 4 KB blocks, indirect blocks)
│   │   ├── framebuffer.rs # Pixel renderer, 8×8 bitmap font, built from HepBL's BootInfo
│   │   ├── nvme.rs        # NVMe host controller driver
│   │   ├── xhci.rs        # XHCI USB host controller, USB HID tablet
│   │   ├── ps2.rs         # PS/2 keyboard (scancode set 1 + extended)
│   │   ├── apic.rs        # x2APIC timer (MSR mode)
│   │   ├── scheduler.rs   # Preemptive round-robin, context switch
│   │   ├── pmm.rs         # Bitmap physical memory manager (reads HepBL's memory map)
│   │   ├── heap.rs        # Slab allocator (GlobalAlloc), full dealloc
│   │   ├── pci.rs         # PCI config-space enumeration
│   │   ├── net.rs         # ARP, ICMP, IP, TCP, DNS resolver, HTTP GET (wget)
│   │   ├── rtl8139.rs     # RTL8139 NIC driver
│   │   ├── e1000.rs       # Intel e1000 NIC driver
│   │   ├── syscall.rs     # SYSCALL/SYSRET gate, dispatcher
│   │   ├── process.rs     # Ring-3 process table, user PML4, exec/ps
│   │   ├── elf.rs         # ELF64 loader
│   │   ├── hda.rs         # Intel HDA driver — beep() via DMA PCM stream
│   │   └── ...            # gdt, idt, vmm, paging, rtc, serial, panic
│   ├── linker.ld          # Custom linker script (higher-half, ENTRY(kmain))
│   ├── build.rs           # Emits linker script path (cross-platform)
│   └── Cargo.toml
├── hepbl/                 # HepBL — our own UEFI bootloader, written from scratch
│   ├── src/main.rs        # Hand-written UEFI FFI, GOP mode select, ELF64 loader,
│   │                      # page table + HHDM builder, ExitBootServices, asm handoff
│   └── Cargo.toml         # No dependencies
├── userspace/              # Ring-3 workspace (hepos-rt, hepos-std, hello demo)
├── build.ps1              # Windows: build + assemble ESP + launch QEMU
├── build.sh               # Linux: same
└── PLAN.md                # Architecture reference and development roadmap
```

---

## HepBL — Our Own Bootloader

Earlier versions of HepOS used [Limine](https://github.com/limine-bootloader/limine). It has since been fully replaced by **HepBL**, a UEFI bootloader written from scratch for this project in `hepbl/`.

**Why UEFI-only, and (almost) no assembly:** a UEFI application already runs in 64-bit long mode when the firmware hands it control, so there's no need to hand-roll real-mode → protected-mode → long-mode transitions the way a BIOS bootloader would. That leaves HepBL free to be ordinary Rust almost everywhere — the UEFI FFI (`extern "efiapi"` function pointers, hand-written from the UEFI spec, no external crate) reads like any other FFI call. The **entire assembly footprint is five instructions**, at the very end of `efi_main_inner`:

```rust
core::arch::asm!(
    "cli",
    "mov cr3, {pml4}",   // switch to HepBL's page tables
    "mov rsp, {stack}",  // switch to the kernel's stack
    "xor rbp, rbp",
    "jmp {entry}",       // jump to kernel.elf's entry point
    pml4 = in(reg) pml4, stack = in(reg) stack_top, entry = in(reg) entry,
    in("rdi") bi_virt,   // RDI = &BootInfo, per the SysV calling convention
    options(noreturn),
);
```

What HepBL does before that handoff:
1. Locates the **GOP** (Graphics Output Protocol) and picks the best available video mode (prefers 1280×800, then 1024×768)
2. Reads `\kernel.elf` off the boot volume via **SimpleFileSystem**
3. Parses the ELF64 header and `PT_LOAD` segments, allocating and mapping each one
4. Builds its own page tables: an HHDM (higher-half direct map) of physical memory at `0xffff800000000000`, using 4 KiB pages throughout (so the kernel's own `map_page`/`map_mmio` can walk and extend them later), plus a transitional identity map that the kernel clears during early boot
5. Reads the UEFI memory map, calls `ExitBootServices`
6. Hands off via the 5-instruction asm block above, passing a `BootInfo` struct (framebuffer info, HHDM offset, memory map) in `RDI`

The `BootInfo` protocol is defined twice — once in `hepbl/src/main.rs`, once in `kernel/src/bootinfo.rs` — and the two must be kept in sync by hand (no shared crate; HepBL and the kernel target different platforms, `x86_64-unknown-uefi` vs `x86_64-unknown-none`).

Before HepBL runs, **OVMF** (the QEMU build of [TianoCore/EDK2](https://github.com/tianocore/edk2), the open-source reference UEFI firmware) does the actual hardware POST and sets up UEFI boot/runtime services — TianoCore's job ends the moment `ExitBootServices` succeeds inside HepBL.

---

## Known Issues

| Issue | Status |
|-------|--------|
| Files max ~4.1 MB | Single indirect block only — double indirect not yet implemented |
| Terminal doesn't reflow text on resize | Column count adapts for new input; existing output stays at old width |
| ACPI shutdown only works on QEMU | Hardcoded port 0x604 (QEMU PIIX4) — real hardware needs FADT parsing |
| NVMe size reported as 0 MB | Identify Namespace command hangs; workaround: hardcoded 512B/block |
| PCI BAR addresses vary by boot | OVMF's PCI allocator assigns them dynamically (not fixed like under BIOS/Limine); `map_mmio` handles any address transparently |

---

## Roadmap

See [PLAN.md](PLAN.md) for the full architecture reference and prioritised next-steps list.

High-level upcoming work:
- `std` shim — enough of `std` (alloc/io/fs stubs) for external Rust crates to link
- Double-indirect blocks (files up to ~4 GB)
- General-purpose UDP stack
- RTL8169 / real-hardware NIC support
- Image viewer, audio player, more Settings pages

---

## License

MIT — see [LICENSE](LICENSE).
