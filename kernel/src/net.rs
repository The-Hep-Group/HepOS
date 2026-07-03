//! Minimal network stack: ARP + ICMP ping.
//! Static config: IP = 10.0.2.15, GW = 10.0.2.2, netmask = 255.255.255.0.

use alloc::{vec, vec::Vec};
// Use RTL8139 if available, otherwise e1000
fn with_nic<T, F: FnOnce(&mut dyn NicOps) -> T>(f: F) -> Option<T> {
    if let Some(n) = crate::rtl8139::NIC.lock().as_mut() {
        return Some(f(n));
    }
    if let Some(n) = crate::e1000::NIC.lock().as_mut() {
        return Some(f(n));
    }
    None
}
trait NicOps { fn send(&mut self, d: &[u8]); fn recv(&mut self) -> Option<alloc::vec::Vec<u8>>; }
impl NicOps for crate::rtl8139::Rtl8139 { fn send(&mut self,d:&[u8]){self.send(d)} fn recv(&mut self)->Option<alloc::vec::Vec<u8>>{self.recv()} }
impl NicOps for crate::e1000::E1000 { fn send(&mut self,d:&[u8]){self.send(d)} fn recv(&mut self)->Option<alloc::vec::Vec<u8>>{self.recv()} }

// Our static network config (QEMU SLiRP defaults)
pub const MY_IP:  [u8; 4] = [10, 0, 2, 15];
pub const GW_IP:  [u8; 4] = [10, 0, 2, 2];
pub const BCAST:  [u8; 4] = [10, 0, 2, 255];
pub const MASK:   [u8; 4] = [255, 255, 255, 0];

pub fn my_mac_pub() -> [u8; 6] { my_mac() }
fn my_mac() -> [u8; 6] {
    if let Some(n) = crate::rtl8139::NIC.lock().as_ref() { return n.mac; }
    if let Some(n) = crate::e1000::NIC.lock().as_ref()   { return n.mac; }
    [0x52,0x54,0,0x12,0x34,0x56]
}

// ── Ethernet ─────────────────────────────────────────────────────────────────
fn eth_send(dst_mac: [u8; 6], etype: u16, payload: &[u8]) {
    let mut f = Vec::with_capacity(14 + payload.len());
    f.extend_from_slice(&dst_mac);
    f.extend_from_slice(&my_mac());
    f.push((etype >> 8) as u8);
    f.push(etype as u8);
    f.extend_from_slice(payload);
    with_nic(|n| n.send(&f));
}

// ── ARP ───────────────────────────────────────────────────────────────────────
pub fn arp_announce() {
    // Gratuitous ARP
    let mut pkt = [0u8; 28];
    pkt[0..2].copy_from_slice(&[0, 1]);  // HTYPE Ethernet
    pkt[2..4].copy_from_slice(&[8, 0]);  // PTYPE IPv4
    pkt[4] = 6; pkt[5] = 4;             // HLEN=6 PLEN=4
    pkt[6..8].copy_from_slice(&[0, 1]);  // ARP REQUEST
    pkt[8..14].copy_from_slice(&my_mac());
    pkt[14..18].copy_from_slice(&MY_IP);
    pkt[18..24].copy_from_slice(&[0xFF; 6]);
    pkt[24..28].copy_from_slice(&MY_IP);
    eth_send([0xFF; 6], 0x0806, &pkt);
}

/// Send an ARP request to resolve `ip` → MAC.
pub fn arp_request(ip: [u8; 4]) {
    let mut pkt = [0u8; 28];
    pkt[0..2].copy_from_slice(&[0, 1]);
    pkt[2..4].copy_from_slice(&[8, 0]);
    pkt[4] = 6; pkt[5] = 4;
    pkt[6..8].copy_from_slice(&[0, 1]);
    pkt[8..14].copy_from_slice(&my_mac());
    pkt[14..18].copy_from_slice(&MY_IP);
    pkt[18..24].copy_from_slice(&[0; 6]);
    pkt[24..28].copy_from_slice(&ip);
    eth_send([0xFF; 6], 0x0806, &pkt);
}

fn handle_arp(data: &[u8]) {
    if data.len() < 28 { return; }
    let op = u16::from_be_bytes([data[6], data[7]]);
    let target_ip = &data[24..28];
    if op == 1 && target_ip == MY_IP {
        // ARP request for us → reply
        let mut rep = [0u8; 28];
        rep[0..2].copy_from_slice(&[0, 1]);
        rep[2..4].copy_from_slice(&[8, 0]);
        rep[4] = 6; rep[5] = 4;
        rep[6..8].copy_from_slice(&[0, 2]); // REPLY
        rep[8..14].copy_from_slice(&my_mac());
        rep[14..18].copy_from_slice(&MY_IP);
        rep[18..24].copy_from_slice(&data[8..14]); // sender MAC
        rep[24..28].copy_from_slice(&data[14..18]); // sender IP
        let dst_mac: [u8; 6] = data[8..14].try_into().unwrap_or([0; 6]);
        eth_send(dst_mac, 0x0806, &rep);
    }
}

// ── IP / ICMP ─────────────────────────────────────────────────────────────────
fn ip_checksum(data: &[u8]) -> u16 {
    let mut sum = 0u32;
    let mut i = 0;
    while i + 1 < data.len() {
        sum += u16::from_be_bytes([data[i], data[i + 1]]) as u32;
        i += 2;
    }
    if i < data.len() { sum += (data[i] as u32) << 8; }
    while sum >> 16 != 0 { sum = (sum & 0xFFFF) + (sum >> 16); }
    !(sum as u16)
}

fn ip_send(dst_ip: [u8; 4], dst_mac: [u8; 6], proto: u8, payload: &[u8]) {
    let total = 20 + payload.len();
    let mut pkt = vec![0u8; total];
    pkt[0] = 0x45; // version=4, IHL=5
    pkt[1] = 0;
    pkt[2..4].copy_from_slice(&(total as u16).to_be_bytes());
    pkt[4..6].copy_from_slice(&[0, 1]); // id
    pkt[6..8].copy_from_slice(&[0x40, 0]); // DF flag
    pkt[8] = 64;  // TTL
    pkt[9] = proto;
    pkt[10..12].copy_from_slice(&[0, 0]); // checksum placeholder
    pkt[12..16].copy_from_slice(&MY_IP);
    pkt[16..20].copy_from_slice(&dst_ip);
    let cksum = ip_checksum(&pkt[..20]);
    pkt[10..12].copy_from_slice(&cksum.to_be_bytes());
    pkt[20..].copy_from_slice(payload);
    eth_send(dst_mac, 0x0800, &pkt);
}

static PING_SEQ:  core::sync::atomic::AtomicU16 = core::sync::atomic::AtomicU16::new(0);
static PING_REPLY: spin::Mutex<Option<u16>>     = spin::Mutex::new(None);

/// Send an ICMP echo request to `dst_ip` (which must resolve to `dst_mac`).
pub fn ping_send(dst_ip: [u8; 4], dst_mac: [u8; 6]) -> u16 {
    let seq = PING_SEQ.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    let mut icmp = [0u8; 8 + 8]; // header + 8 bytes of data
    icmp[0] = 8;  // ICMP Echo Request
    icmp[1] = 0;
    icmp[2..4].copy_from_slice(&[0, 0]); // checksum
    icmp[4..6].copy_from_slice(&[0, 1]); // id
    icmp[6..8].copy_from_slice(&seq.to_be_bytes());
    icmp[8..16].copy_from_slice(b"HepOS!  ");
    let ck = ip_checksum(&icmp);
    icmp[2..4].copy_from_slice(&ck.to_be_bytes());
    ip_send(dst_ip, dst_mac, 1, &icmp);
    seq
}

fn handle_icmp(src_ip: [u8; 4], src_mac: [u8; 6], data: &[u8]) {
    if data.is_empty() { return; }
    match data[0] {
        8 => {
            // Echo Request → send Reply
            let mut rep = data.to_vec();
            rep[0] = 0; // Echo Reply
            rep[2] = 0; rep[3] = 0;
            let ck = ip_checksum(&rep);
            rep[2..4].copy_from_slice(&ck.to_be_bytes());
            ip_send(src_ip, src_mac, 1, &rep);
        }
        0 => {
            // Echo Reply → record
            if data.len() >= 8 {
                let seq = u16::from_be_bytes([data[6], data[7]]);
                *PING_REPLY.lock() = Some(seq);
            }
        }
        _ => {}
    }
}

fn handle_ip(eth_src: [u8; 6], data: &[u8]) {
    if data.len() < 20 { return; }
    let dst_ip: [u8; 4] = data[16..20].try_into().unwrap_or([0; 4]);
    if dst_ip != MY_IP { return; }
    let src_ip: [u8; 4] = data[12..16].try_into().unwrap_or([0; 4]);
    let proto = data[9];
    let ihl = (data[0] & 0x0F) as usize * 4;
    if data.len() < ihl { return; }
    if proto == 1 { handle_icmp(src_ip, eth_src, &data[ihl..]); }
}

/// Process one incoming Ethernet frame. Call this from your polling loop.
pub fn handle_frame(frame: &[u8]) {
    if frame.len() < 14 { return; }
    let etype = u16::from_be_bytes([frame[12], frame[13]]);
    let src_mac: [u8; 6] = frame[6..12].try_into().unwrap_or([0; 6]);
    match etype {
        0x0806 => handle_arp(&frame[14..]),
        0x0800 => handle_ip(src_mac, &frame[14..]),
        _ => {}
    }
}

/// High-level ping. Sends request, polls for reply up to ~250 ms.
/// Returns round-trip string or error.
pub fn ping(target_ip: [u8; 4]) -> alloc::string::String {
    use alloc::format;
    // NIC check
    if crate::rtl8139::NIC.lock().is_none() && crate::e1000::NIC.lock().is_none() {
        return format!("ping: no NIC found");
    }

    // Routing:
    //   on-subnet destination  → would normally ARP, but SLiRP only provides
    //                            the gateway host; ARP for other 10.0.2.x won't reply.
    //                            Use gateway MAC for gateway IP, else best-effort.
    //   off-subnet destination → route via gateway (use gateway MAC).
    // In all QEMU SLiRP cases the known gateway MAC is 52:55:0a:00:02:02.
    let gw_mac = [0x52u8, 0x55, 0x0a, 0x00, 0x02, 0x02];
    let on_subnet = (target_ip[0] == MY_IP[0]) && (target_ip[1] == MY_IP[1])
                 && (target_ip[2] == MY_IP[2]);
    let dst_mac = gw_mac; // gateway MAC works for all QEMU SLiRP targets
    let _ = on_subnet;    // routing note: on-subnet uses gateway MAC too (SLiRP limitation)

    // Send echo request
    *PING_REPLY.lock() = None;
    let seq = ping_send(target_ip, dst_mac);
    let start = crate::rtc::now();

    // Step 3: poll for ICMP reply (~250ms total)
    for _ in 0..500u32 {
        let frame = with_nic(|n| n.recv()).flatten();
        if let Some(f) = frame { handle_frame(&f); }
        for _ in 0..40_000u32 { core::hint::spin_loop(); }
        if let Some(got_seq) = *PING_REPLY.lock() {
            if got_seq == seq {
                let end = crate::rtc::now();
                let ms = (end.sec as i32 - start.sec as i32).abs() * 1000;
                return format!("reply from {}.{}.{}.{}: seq={} time={}ms",
                    target_ip[0], target_ip[1], target_ip[2], target_ip[3],
                    seq, ms);
            }
        }
    }
    format!("ping: timeout ({}.{}.{}.{})",
        target_ip[0], target_ip[1], target_ip[2], target_ip[3])
}
