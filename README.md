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
- **Preemptive scheduler** — round-robin, APIC timer, context switch in naked asm
- **x2APIC** — MSR-mode APIC (no MMIO mapping needed)
- **PS/2 keyboard** — full scancode set 1, shift/caps/ctrl, extended keys

---

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│  libOS (kernel space, Rust)                             │
│  desktop · terminal · editor · HepFS navigator          │
├─────────────────────────────────────────────────────────┤
│  Kernel                                                 │
│  PMM · VMM · Heap · GDT · IDT · APIC · Scheduler       │
├────────────┬────────────┬────────────┬──────────────────┤
│  Storage   │  Display   │  Input     │  Network         │
│  NVMe      │  GOP FB    │  PS/2 kbd  │  RTL8139         │
│  HepFS     │  SW render │  XHCI USB  │  e1000           │
└────────────┴────────────┴────────────┴──────────────────┘
Hardware: x86_64, BIOS boot via Limine v9
```

**Language:** Rust nightly (`no_std` + `alloc`)  
**Bootloader:** [Limine](https://github.com/limine-bootloader/limine) v9.x (BIOS + UEFI)  
**Target:** `x86_64-unknown-none`

---

## Prerequisites

### Windows

| Tool | Where to get |
|------|-------------|
| Rust (nightly) | https://rustup.rs — then `rustup toolchain install nightly` |
| `x86_64-unknown-none` target | `rustup target add x86_64-unknown-none` |
| rust-src component | `rustup component add rust-src --toolchain nightly` |
| MSYS2 (for xorriso) | https://www.msys2.org — then `pacman -S xorriso` in MSYS2 |
| QEMU | https://www.qemu.org/download/#windows — install to `C:\Program Files\qemu\` |
| Git | https://git-scm.com |

### Linux (Debian/Ubuntu)

```bash
# Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup toolchain install nightly
rustup target add x86_64-unknown-none
rustup component add rust-src --toolchain nightly

# Build tools
sudo apt install xorriso qemu-system-x86 gcc git make
```

### Linux (Arch)

```bash
rustup toolchain install nightly
rustup target add x86_64-unknown-none
rustup component add rust-src --toolchain nightly

sudo pacman -S xorriso qemu-system-x86 gcc git make
```

---

## Building & Running

### Clone

```bash
git clone https://github.com/The-Hep-Group/HepOS.git
cd HepOS
git clone https://github.com/limine-bootloader/limine.git --branch=v9.x-binary --depth=1 limine
```

> The `limine/` directory (bootloader binaries) is not committed to the repo — extra clone needed (done in above).

### Windows

```powershell
.\build.ps1
```

That's it. The script:
1. Builds the kernel with `cargo +nightly build --release`
2. Assembles the ISO with xorriso (from MSYS2)
3. Installs the Limine BIOS stage onto the ISO
4. Creates `hepos_disk.img` (512 MB NVMe image) if missing
5. Launches QEMU

**Requirements:** MSYS2 must be at `C:\msys64\` and QEMU at `C:\Program Files\qemu\`.

### Linux

```bash
chmod +x build.sh
./build.sh
```

The script does the same steps as the Windows one. On Linux it also compiles the `limine` installer tool from the included `limine/limine.c` source (`make -C limine` runs automatically if the binary is missing).

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

If you want to run a pre-built ISO without the build script:

```bash
qemu-system-x86_64 \
  -M q35 \
  -cpu qemu64,+x2apic \
  -m 256M \
  -cdrom hepos.iso \
  -boot d \
  -drive file=hepos_disk.img,if=none,id=nvme0,format=raw \
  -device nvme,serial=heposv1,drive=nvme0 \
  -netdev user,id=net0 \
  -device rtl8139,netdev=net0 \
  -device qemu-xhci,id=xhci \
  -device usb-tablet,bus=xhci.0 \
  -vga std \
  -display sdl \
  -serial stdio \
  -no-reboot \
  -no-shutdown
```

> `-device usb-tablet` gives absolute mouse coordinates via XHCI — the mouse works out of the box without grabbing.  
> `-serial stdio` prints kernel debug output (boot messages, panics) to your terminal.

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
│   │   ├── desktop.rs     # WM: windows, taskbar, start menu, compositor
│   │   ├── terminal.rs    # Shell with 30+ commands, tab completion
│   │   ├── editor.rs      # Text editor with find mode
│   │   ├── hepfs.rs       # Custom filesystem (flat inode, 4 KB blocks, indirect blocks)
│   │   ├── framebuffer.rs # GOP pixel renderer, 8×8 bitmap font
│   │   ├── nvme.rs        # NVMe host controller driver
│   │   ├── xhci.rs        # XHCI USB host controller, USB HID tablet
│   │   ├── ps2.rs         # PS/2 keyboard (scancode set 1 + extended)
│   │   ├── apic.rs        # x2APIC timer (MSR mode)
│   │   ├── scheduler.rs   # Preemptive round-robin, context switch
│   │   ├── pmm.rs         # Bitmap physical memory manager
│   │   ├── heap.rs        # Bump allocator (GlobalAlloc)
│   │   ├── pci.rs         # PCI config-space enumeration
│   │   ├── net.rs         # ARP + ICMP stack
│   │   ├── rtl8139.rs     # RTL8139 NIC driver
│   │   ├── e1000.rs       # Intel e1000 NIC driver
│   │   └── ...            # gdt, idt, vmm, paging, rtc, serial, panic
│   ├── linker.ld          # Custom linker script (Limine-compatible)
│   ├── build.rs           # Emits linker script path (cross-platform)
│   └── Cargo.toml
├── bootloader/
│   └── limine.conf        # Boot entry: loads /boot/hepos-kernel
├── limine/                # Limine v9.x binary release (committed)
│   ├── limine-bios.sys    # BIOS stage 2
│   ├── limine-bios-cd.bin # El Torito BIOS boot image
│   ├── limine-uefi-cd.bin # El Torito UEFI boot image
│   ├── BOOTX64.EFI        # UEFI application
│   ├── limine.exe         # Windows installer tool
│   ├── limine.c           # Installer source (compiled on Linux by build.sh)
│   └── Makefile           # Builds limine installer on Linux/macOS
├── build.ps1              # Windows: build + launch QEMU
├── build.sh               # Linux: build + launch QEMU
└── PLAN.md                # Architecture reference and development roadmap
```

---

## Limine Bootloader

HepOS uses [Limine](https://github.com/limine-bootloader/limine) v9.x as its bootloader.

**The `limine/` directory contains pre-compiled binary blobs only** — there is no bootloader assembly source in this repository. Limine's source code (which is written in C and x86 assembly) lives in the main Limine repository on a different branch.

The files committed here are from Limine's `v9.x-binary` release branch:

| File | Purpose |
|------|---------|
| `limine-bios.sys` | BIOS stage 2 — loaded by the boot sector, sets up Limine protocol |
| `limine-bios-cd.bin` | El Torito boot image for BIOS CD boot |
| `limine-uefi-cd.bin` | El Torito boot image for UEFI CD boot |
| `BOOTX64.EFI` | UEFI application (x86\_64) |
| `limine.exe` | Windows tool: installs BIOS stage onto a disk/ISO |
| `limine.c` + `Makefile` | Source to compile the installer on Linux/macOS |

The kernel communicates with Limine at boot time via the **Limine boot protocol** — a set of request/response structs placed in special ELF sections. Limine reads these before jumping to `kmain`, fills in the responses (framebuffer address, HHDM offset, memory map, etc.), and hands off execution.

If you want to read Limine's assembly: `git clone https://github.com/limine-bootloader/limine --branch=v9.x-release-binary` and look at the `common/` and `stage2/` directories.

---

## Known Issues

| Issue | Status |
|-------|--------|
| Network RX broken on QEMU/Windows | QEMU SLiRP issue — TX works fine; test on Linux/KVM for full networking |
| Heap cannot free memory | Bump allocator by design — slab allocator planned |
| Files max ~4.1 MB | Single indirect block only — double indirect not yet implemented |
| Terminal doesn't reflow text on resize | Column count adapts for new input; existing output stays at old width |
| ACPI shutdown only works on QEMU | Hardcoded port 0x604 (QEMU PIIX4) — real hardware needs FADT parsing |

---

## Roadmap

See [PLAN.md](PLAN.md) for the full architecture reference and prioritised next-steps list.

High-level upcoming work:
- Syscall interface (SYSCALL/SYSRET) + per-process page tables
- ELF loader → userspace processes
- `std` shim → unlock Rust crates
- Slab allocator (free memory)
- Intel HDA audio
- Double-indirect blocks (files up to ~4 GB)

---

## License

MIT — see [LICENSE](LICENSE).
