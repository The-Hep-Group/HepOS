use alloc::vec::Vec;
use core::arch::asm;

// PCI config space via I/O ports 0xCF8 (address) / 0xCFC (data)
const PCI_ADDR: u16 = 0xCF8;
const PCI_DATA: u16 = 0xCFC;

fn config_addr(bus: u8, dev: u8, func: u8, offset: u8) -> u32 {
    (1 << 31)
        | ((bus  as u32) << 16)
        | ((dev  as u32) << 11)
        | ((func as u32) << 8)
        | ((offset & 0xFC) as u32)
}

fn outl(port: u16, val: u32) {
    unsafe { asm!("out dx, eax", in("dx") port, in("eax") val, options(nomem, nostack)); }
}
fn inl(port: u16) -> u32 {
    let v: u32;
    unsafe { asm!("in eax, dx", out("eax") v, in("dx") port, options(nomem, nostack)); }
    v
}

pub fn config_read32(bus: u8, dev: u8, func: u8, offset: u8) -> u32 {
    outl(PCI_ADDR, config_addr(bus, dev, func, offset));
    inl(PCI_DATA)
}

pub fn config_read16(bus: u8, dev: u8, func: u8, offset: u8) -> u16 {
    let v = config_read32(bus, dev, func, offset & !3);
    (v >> ((offset & 2) * 8)) as u16
}

pub fn config_read8(bus: u8, dev: u8, func: u8, offset: u8) -> u8 {
    let v = config_read32(bus, dev, func, offset & !3);
    (v >> ((offset & 3) * 8)) as u8
}

pub fn config_write32(bus: u8, dev: u8, func: u8, offset: u8, val: u32) {
    outl(PCI_ADDR, config_addr(bus, dev, func, offset));
    outl(PCI_DATA, val);
}

#[derive(Debug, Clone)]
pub struct PciDevice {
    pub bus:      u8,
    pub dev:      u8,
    pub func:     u8,
    pub vendor_id: u16,
    pub device_id: u16,
    pub class:    u8,
    pub subclass: u8,
    pub prog_if:  u8,
    pub header:   u8,
}

impl PciDevice {
    pub fn bar(&self, n: u8) -> u32 {
        config_read32(self.bus, self.dev, self.func, 0x10 + n * 4)
    }
}

pub fn enumerate() -> Vec<PciDevice> {
    let mut devices = Vec::new();
    for bus in 0u8..=255 {
        for dev in 0u8..32 {
            let id = config_read32(bus, dev, 0, 0x00);
            if id == 0xFFFF_FFFF { continue; } // no device

            let vendor_id = id as u16;
            let device_id = (id >> 16) as u16;

            let class_reg = config_read32(bus, dev, 0, 0x08);
            let header_reg = config_read32(bus, dev, 0, 0x0C);

            let prog_if = (class_reg >> 8)  as u8;
            let subclass = (class_reg >> 16) as u8;
            let class    = (class_reg >> 24) as u8;
            let header   = (header_reg >> 16) as u8;

            let func_count = if header & 0x80 != 0 { 8u8 } else { 1u8 };

            for func in 0..func_count {
                let fid = config_read32(bus, dev, func, 0x00);
                if fid == 0xFFFF_FFFF { continue; }

                let fc = config_read32(bus, dev, func, 0x08);
                devices.push(PciDevice {
                    bus, dev, func,
                    vendor_id: fid as u16,
                    device_id: (fid >> 16) as u16,
                    class:    (fc >> 24) as u8,
                    subclass: (fc >> 16) as u8,
                    prog_if:  (fc >> 8)  as u8,
                    header,
                });
            }
        }
    }
    devices
}

// Common class codes
pub const CLASS_STORAGE:  u8 = 0x01;
pub const CLASS_NETWORK:  u8 = 0x02;
pub const CLASS_DISPLAY:  u8 = 0x03;
pub const CLASS_BRIDGE:   u8 = 0x06;
pub const CLASS_SERIAL:   u8 = 0x0C;

pub const SUB_NVME:       u8 = 0x08; // storage subclass
pub const SUB_USB:        u8 = 0x03; // serial bus subclass
pub const SUB_ETHERNET:   u8 = 0x00; // network subclass

pub fn class_name(class: u8, sub: u8) -> &'static str {
    match (class, sub) {
        (CLASS_STORAGE, 0x01) => "IDE",
        (CLASS_STORAGE, 0x06) => "SATA/AHCI",
        (CLASS_STORAGE, SUB_NVME) => "NVMe",
        (CLASS_NETWORK, SUB_ETHERNET) => "Ethernet",
        (CLASS_DISPLAY, 0x00) => "VGA",
        (CLASS_DISPLAY, 0x02) => "Display (3D)",
        (CLASS_BRIDGE,  0x00) => "Host Bridge",
        (CLASS_BRIDGE,  0x01) => "ISA Bridge",
        (CLASS_BRIDGE,  0x04) => "PCI-PCI Bridge",
        (CLASS_SERIAL,  SUB_USB) => "USB",
        _ => "Unknown",
    }
}
