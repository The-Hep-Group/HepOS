#!/usr/bin/env bash
set -e
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Ensure cargo is on PATH
export PATH="$HOME/.cargo/bin:$PATH"

# ── 1. Build kernel ───────────────────────────────────────────────────────────
cd "$SCRIPT_DIR/kernel"
cargo +nightly build --release
cd "$SCRIPT_DIR"

KERNEL_ELF="$SCRIPT_DIR/kernel/target/x86_64-unknown-none/release/hepos-kernel"
if [ ! -f "$KERNEL_ELF" ]; then
    echo "ERROR: kernel build failed" >&2; exit 1
fi

# ── 2. Build HepBL (our UEFI bootloader) ─────────────────────────────────────
rustup target add x86_64-unknown-uefi --toolchain nightly >/dev/null 2>&1 || true
cd "$SCRIPT_DIR/hepbl"
cargo +nightly build --release
cd "$SCRIPT_DIR"

HEPBL_EFI="$SCRIPT_DIR/hepbl/target/x86_64-unknown-uefi/release/hepbl.efi"
if [ ! -f "$HEPBL_EFI" ]; then
    echo "ERROR: HepBL build failed" >&2; exit 1
fi

# ── 3. Assemble ESP directory (QEMU exposes it as a FAT disk via VVFAT) ──────
ESP="$SCRIPT_DIR/esp"
mkdir -p "$ESP/EFI/BOOT"
cp "$HEPBL_EFI"  "$ESP/EFI/BOOT/BOOTX64.EFI"
cp "$KERNEL_ELF" "$ESP/kernel.elf"
echo "ESP assembled: $ESP (HepBL + kernel.elf)"

# ── 4. UEFI firmware (OVMF/edk2, ships with QEMU) ────────────────────────────
QEMU_BIN="$(command -v qemu-system-x86_64 || true)"
if [ -n "$QEMU_BIN" ]; then
    QEMU_SHARE="$(dirname "$QEMU_BIN")/share"
else
    QEMU_SHARE="/c/Program Files/qemu/share"
    QEMU_BIN="/c/Program Files/qemu/qemu-system-x86_64.exe"
fi
CODE_FD="$QEMU_SHARE/edk2-x86_64-code.fd"
if [ ! -f "$CODE_FD" ]; then
    echo "ERROR: UEFI firmware not found: $CODE_FD" >&2; exit 1
fi
# Writable NVRAM copy (template ships as edk2-i386-vars.fd, valid for x86_64)
VARS_FD="$SCRIPT_DIR/hepbl_vars.fd"
if [ ! -f "$VARS_FD" ]; then
    cp "$QEMU_SHARE/edk2-i386-vars.fd" "$VARS_FD"
fi

# ── 5. Create NVMe disk image if needed ──────────────────────────────────────
DISK="$SCRIPT_DIR/hepos_disk.img"
if [ ! -f "$DISK" ]; then
    echo "Creating 512 MB NVMe disk..."
    qemu-img create -f raw "$DISK" 512M
fi

# ── 6. Run in QEMU (UEFI boot via HepBL) ─────────────────────────────────────
# X-PciMmio64Mb=0 keeps all PCI BARs below 4 GiB (kernel reads 32-bit BARs,
# and HepBL's HHDM covers 0..4 GiB).
"$QEMU_BIN" \
    -M q35 \
    -cpu qemu64,+x2apic \
    -m 256M \
    -drive if=pflash,format=raw,readonly=on,file="$CODE_FD" \
    -drive if=pflash,format=raw,file="$VARS_FD" \
    -drive format=raw,file=fat:rw:esp \
    -fw_cfg name=opt/ovmf/X-PciMmio64Mb,string=0 \
    -drive file="$DISK",if=none,id=nvme0,format=raw \
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
