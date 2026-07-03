$ErrorActionPreference = "Stop"
$root = $PSScriptRoot

# Ensure cargo is on PATH (Rust installer adds it to user env, but not always to this session)
$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"

# ── 1. Build kernel ──────────────────────────────────────────────────────────
Push-Location "$root\kernel"
cargo +nightly build --release
Pop-Location

$kernel_elf = "$root\kernel\target\x86_64-unknown-none\release\hepos-kernel"
if (-not (Test-Path $kernel_elf)) { Write-Error "kernel build failed"; exit 1 }

# ── 2. Fetch Limine if missing ───────────────────────────────────────────────
$limine_dir = "$root\limine"
if (-not (Test-Path $limine_dir)) {
    Write-Host "Cloning Limine..."
    git clone https://github.com/limine-bootloader/limine.git --branch=v9.x-binary --depth=1 $limine_dir
}

# ── 3. Build ISO image ───────────────────────────────────────────────────────
$iso_root  = "$root\iso_root"
$iso_boot  = "$iso_root\boot"
$iso_limine = "$iso_boot\limine"

Remove-Item -Recurse -Force $iso_root -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $iso_limine | Out-Null
New-Item -ItemType Directory -Force "$iso_root\EFI\BOOT" | Out-Null

Copy-Item $kernel_elf         "$iso_boot\hepos-kernel"
Copy-Item "$root\bootloader\limine.conf" "$iso_limine\limine.conf"
Copy-Item "$limine_dir\limine-bios.sys"        "$iso_limine\"
Copy-Item "$limine_dir\limine-bios-cd.bin"     "$iso_limine\"
Copy-Item "$limine_dir\limine-uefi-cd.bin"     "$iso_limine\"
Copy-Item "$limine_dir\BOOTX64.EFI"            "$iso_root\EFI\BOOT\"

$iso = "$root\hepos.iso"

# Convert Windows paths to Unix paths for xorriso (MSYS2 tool)
$unix_iso_root = & C:\msys64\usr\bin\cygpath.exe -u $iso_root
$unix_iso      = & C:\msys64\usr\bin\cygpath.exe -u $iso

& "C:\msys64\usr\bin\xorriso.exe" -as mkisofs -b boot/limine/limine-bios-cd.bin `
    -no-emul-boot -boot-load-size 4 -boot-info-table `
    --efi-boot boot/limine/limine-uefi-cd.bin `
    -efi-boot-part --efi-boot-image --protective-msdos-label `
    $unix_iso_root -o $unix_iso

# Install Limine BIOS stage
& "$limine_dir\limine.exe" bios-install $iso

Write-Host "ISO built: $iso"

# ── 4. Create NVMe disk image if needed ─────────────────────────────────────
$disk = "$root\hepos_disk.img"
$qemu_img = "C:\Program Files\qemu\qemu-img.exe"
if (-not (Test-Path $disk)) {
    Write-Host "Creating 512MB NVMe disk..."
    & $qemu_img create -f raw $disk 512M
}

# ── 5. Run in QEMU ──────────────────────────────────────────────────────────
$qemu = "C:\Program Files\qemu\qemu-system-x86_64.exe"
& $qemu `
    -M q35 `
    -cpu qemu64,+x2apic `
    -m 256M `
    -cdrom $iso `
    -boot d `
    -drive file=$disk,if=none,id=nvme0,format=raw `
    -device nvme,serial=heposv1,drive=nvme0 `
    -netdev user,id=net0 `
    -device rtl8139,netdev=net0 `
    -device qemu-xhci,id=xhci `
    -device usb-tablet,bus=xhci.0 `
    -vga std `
    -display sdl,window-close=off `
    -serial stdio `
    -no-reboot `
    -no-shutdown
