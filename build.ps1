$ErrorActionPreference = "Stop"
$root = $PSScriptRoot

# Ensure cargo is on PATH (Rust installer adds it to user env, but not always to this session)
$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"

# ── 1. Build userspace (hepos-rt + hepos-std + hello) ───────────────────────
Push-Location "$root\userspace"
cargo +nightly build --release
Pop-Location

$hello_elf = "$root\userspace\target\x86_64-unknown-none\release\hello"
if (-not (Test-Path $hello_elf)) { Write-Error "userspace build failed"; exit 1 }
Write-Host "Userspace built: $hello_elf"

# ── 2. Build kernel ──────────────────────────────────────────────────────────
Push-Location "$root\kernel"
cargo +nightly build --release
Pop-Location

$kernel_elf = "$root\kernel\target\x86_64-unknown-none\release\hepos-kernel"
if (-not (Test-Path $kernel_elf)) { Write-Error "kernel build failed"; exit 1 }

# ── 3. Build HepBL (our UEFI bootloader, written from scratch) ───────────────
try { rustup target add x86_64-unknown-uefi --toolchain nightly *>$null } catch {}
Push-Location "$root\hepbl"
cargo +nightly build --release
Pop-Location

$hepbl_efi = "$root\hepbl\target\x86_64-unknown-uefi\release\hepbl.efi"
if (-not (Test-Path $hepbl_efi)) { Write-Error "HepBL build failed"; exit 1 }
Write-Host "HepBL built: $hepbl_efi"

# ── 4. Assemble ESP directory (QEMU exposes it as a FAT disk via VVFAT) ──────
$esp = "$root\esp"
New-Item -ItemType Directory -Force "$esp\EFI\BOOT" | Out-Null
Copy-Item $hepbl_efi  "$esp\EFI\BOOT\BOOTX64.EFI"
Copy-Item $kernel_elf "$esp\kernel.elf"
Write-Host "ESP assembled: $esp (HepBL + kernel.elf)"

# ── 5. UEFI firmware (OVMF/edk2, ships with QEMU) ────────────────────────────
$qemu_share = "C:\Program Files\qemu\share"
$code_fd = "$qemu_share\edk2-x86_64-code.fd"
if (-not (Test-Path $code_fd)) { Write-Error "UEFI firmware not found: $code_fd"; exit 1 }
# Writable NVRAM copy (template ships as edk2-i386-vars.fd, valid for x86_64)
$vars_fd = "$root\hepbl_vars.fd"
if (-not (Test-Path $vars_fd)) {
    Copy-Item "$qemu_share\edk2-i386-vars.fd" $vars_fd
}

# ── 6. Create NVMe + SATA disk images if needed ─────────────────────────────
$disk = "$root\hepos_disk.img"
$qemu_img = "C:\Program Files\qemu\qemu-img.exe"
if (-not (Test-Path $disk)) {
    Write-Host "Creating 512MB NVMe disk..."
    & $qemu_img create -f raw $disk 512M
}
$sata_disk = "$root\hepos_sata.img"
if (-not (Test-Path $sata_disk)) {
    Write-Host "Creating 64MB SATA disk..."
    & $qemu_img create -f raw $sata_disk 64M
}

# ── 7. Run in QEMU (UEFI boot via HepBL) ─────────────────────────────────────
# X-PciMmio64Mb=0 keeps all PCI BARs below 4 GiB (kernel reads 32-bit BARs,
# and HepBL's HHDM covers 0..4 GiB).
$qemu = "C:\Program Files\qemu\qemu-system-x86_64.exe"
& $qemu `
    -M q35 `
    -cpu qemu64,+x2apic `
    -m 256M `
    -drive if=pflash,format=raw,readonly=on,file=$code_fd `
    -drive if=pflash,format=raw,file=$vars_fd `
    -drive format=raw,file=fat:rw:$esp `
    -fw_cfg name=opt/ovmf/X-PciMmio64Mb,string=0 `
    -drive file=$disk,if=none,id=nvme0,format=raw `
    -device nvme,serial=heposv1,drive=nvme0 `
    -device ahci,id=ahci0 `
    -drive file=$sata_disk,if=none,id=sata0,format=raw `
    -device ide-hd,drive=sata0,bus=ahci0.0 `
    -netdev user,id=net0 `
    -device rtl8139,netdev=net0 `
    -device intel-hda `
    -device hda-output `
    -device qemu-xhci,id=xhci `
    -device usb-tablet,bus=xhci.0 `
    -vga std `
    -display sdl,window-close=off `
    -serial stdio `
    -no-reboot `
    -no-shutdown
