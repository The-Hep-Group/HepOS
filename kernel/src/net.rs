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
    match proto {
        1  => handle_icmp(src_ip, eth_src, &data[ihl..]),
        17 => handle_udp(&data[ihl..]),
        _  => {}
    }
}

// ── UDP ───────────────────────────────────────────────────────────────────────

pub fn udp_send(dst_ip: [u8; 4], dst_mac: [u8; 6], src_port: u16, dst_port: u16, data: &[u8]) {
    let len = (8 + data.len()) as u16;
    let mut seg = vec![0u8; len as usize];
    seg[0..2].copy_from_slice(&src_port.to_be_bytes());
    seg[2..4].copy_from_slice(&dst_port.to_be_bytes());
    seg[4..6].copy_from_slice(&len.to_be_bytes());
    // checksum is optional for IPv4 UDP — leave as 0
    seg[8..].copy_from_slice(data);
    ip_send(dst_ip, dst_mac, 17, &seg);
}

fn handle_udp(_data: &[u8]) {
    // Placeholder — incoming UDP is not yet dispatched to any service.
}

// ── TCP ───────────────────────────────────────────────────────────────────────

const TCP_FIN: u8 = 0x01;
const TCP_SYN: u8 = 0x02;
const TCP_RST: u8 = 0x04;
const TCP_PSH: u8 = 0x08;
const TCP_ACK: u8 = 0x10;

fn tcp_checksum(src_ip: [u8; 4], dst_ip: [u8; 4], tcp_seg: &[u8]) -> u16 {
    // Pseudo-header: src_ip, dst_ip, 0x00, proto=6, tcp_length
    let tcp_len = tcp_seg.len() as u16;
    let mut buf = vec![0u8; 12 + tcp_seg.len()];
    buf[0..4].copy_from_slice(&src_ip);
    buf[4..8].copy_from_slice(&dst_ip);
    buf[8] = 0;
    buf[9] = 6;
    buf[10..12].copy_from_slice(&tcp_len.to_be_bytes());
    buf[12..].copy_from_slice(tcp_seg);
    ip_checksum(&buf)
}

fn tcp_send_seg(
    dst_ip: [u8; 4], dst_mac: [u8; 6],
    src_port: u16, dst_port: u16,
    seq: u32, ack: u32, flags: u8,
    data: &[u8],
) {
    let mut seg = vec![0u8; 20 + data.len()];
    seg[0..2].copy_from_slice(&src_port.to_be_bytes());
    seg[2..4].copy_from_slice(&dst_port.to_be_bytes());
    seg[4..8].copy_from_slice(&seq.to_be_bytes());
    seg[8..12].copy_from_slice(&ack.to_be_bytes());
    seg[12] = 0x50;                                         // data offset = 5 (20 byte header)
    seg[13] = flags;
    seg[14..16].copy_from_slice(&65535u16.to_be_bytes());   // window
    // [16..18] = checksum placeholder (zero)
    seg[20..].copy_from_slice(data);
    let ck = tcp_checksum(MY_IP, dst_ip, &seg);
    seg[16..18].copy_from_slice(&ck.to_be_bytes());
    ip_send(dst_ip, dst_mac, 6, &seg);
}

fn tcp_parse(data: &[u8]) -> Option<(u16, u16, u32, u8, &[u8])> {
    if data.len() < 20 { return None; }
    let src_port = u16::from_be_bytes([data[0], data[1]]);
    let dst_port = u16::from_be_bytes([data[2], data[3]]);
    let seq      = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    let flags    = data[13];
    let doff     = ((data[12] >> 4) as usize) * 4;
    if doff > data.len() { return None; }
    Some((src_port, dst_port, seq, flags, &data[doff..]))
}

/// Parse a dotted-quad IP address string into bytes.
pub fn parse_ip(s: &str) -> Option<[u8; 4]> {
    let mut it = s.split('.');
    let a = it.next()?.parse::<u8>().ok()?;
    let b = it.next()?.parse::<u8>().ok()?;
    let c = it.next()?.parse::<u8>().ok()?;
    let d = it.next()?.parse::<u8>().ok()?;
    if it.next().is_some() { return None; }
    Some([a, b, c, d])
}

/// Open a TCP connection to `dst_ip:dst_port`, send `request`, collect the full
/// response (until the server sends FIN or we time out after ~5 s), then close.
/// Routes via the SLiRP gateway MAC so external IPs work out of the box.
static NEXT_SRC_PORT: core::sync::atomic::AtomicU16 =
    core::sync::atomic::AtomicU16::new(49152);

pub fn tcp_get(dst_ip: [u8; 4], dst_port: u16, request: &[u8]) -> Result<Vec<u8>, &'static str> {
    let gw_mac   = [0x52u8, 0x55, 0x0a, 0x00, 0x02, 0x02];
    // Rotate through ephemeral ports to avoid TIME_WAIT collisions on repeated calls.
    let src_port = {
        let p = NEXT_SRC_PORT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        if p == 0 { NEXT_SRC_PORT.store(49153, core::sync::atomic::Ordering::Relaxed); 49152 } else { p }
    };
    // Use TSC as a varying ISN so SLiRP doesn't confuse this with an old connection.
    let mut seq  = crate::hda::rdtsc() as u32 | 1;
    let mut ack  = 0u32;
    let mut state: u8 = 0; // 0 = SYN_SENT, 1 = ESTABLISHED, 2 = done
    let mut rx   = Vec::new();

    tcp_send_seg(dst_ip, gw_mac, src_port, dst_port, seq, 0, TCP_SYN, &[]);
    crate::serial::print("tcp: SYN sent\n");

    // TSC-based 8-second timeout — immune to QEMU speed variations.
    let tsc_per_ms = crate::hda::TSC_PER_MS.load(core::sync::atomic::Ordering::Relaxed);
    let deadline   = crate::hda::rdtsc().wrapping_add(tsc_per_ms.saturating_mul(3_000));

    'poll: loop {
        // Timeout check
        if crate::hda::rdtsc().wrapping_sub(deadline) < u64::MAX / 2 { break 'poll; }

        loop {
            let frame = with_nic(|n| n.recv()).flatten();
            let Some(f) = frame else { break };
            if f.len() < 14 { continue; }
            let etype = u16::from_be_bytes([f[12], f[13]]);
            if etype != 0x0800 {
                crate::serial::print("tcp: non-IP frame etype=");
                crate::serial::print_hex("", etype as u64);
                crate::serial::print("\n");
                continue;
            }

            let ip = &f[14..];
            if ip.len() < 20 { continue; }
            // ICMP unreachable (type=3) → treat as hard error
            if ip[9] == 1 {
                let ihl2 = ((ip[0] & 0x0F) as usize) * 4;
                if ip.len() > ihl2 && ip[ihl2] == 3 { return Err("tcp: unreachable"); }
                continue;
            }
            if ip[9] != 6 {
                crate::serial::print("tcp: IP proto not TCP: ");
                crate::serial::print_hex("", ip[9] as u64);
                crate::serial::print("\n");
                continue;
            }
            let src_ip: [u8; 4] = ip[12..16].try_into().unwrap_or([0; 4]);
            crate::serial::print("tcp: TCP from ");
            crate::serial::print_hex("", src_ip[0] as u64); crate::serial::print(".");
            crate::serial::print_hex("", src_ip[1] as u64); crate::serial::print(".");
            crate::serial::print_hex("", src_ip[2] as u64); crate::serial::print(".");
            crate::serial::print_hex("", src_ip[3] as u64);
            crate::serial::print(" flags=");
            if ip.len() > 20 { crate::serial::print_hex("", ip[33] as u64); }
            crate::serial::print("\n");
            if src_ip != dst_ip { continue; }
            if ip[16..20] != MY_IP { continue; }

            let ihl = ((ip[0] & 0x0F) as usize) * 4;
            let Some((sp, dp, seg_seq, flags, payload)) = tcp_parse(&ip[ihl..]) else { continue };
            if sp != dst_port || dp != src_port { continue; }

            if flags & TCP_RST != 0 { return Err("tcp: connection refused"); }

            match state {
                0 => {
                    if flags & (TCP_SYN | TCP_ACK) == (TCP_SYN | TCP_ACK) {
                        seq   = seq.wrapping_add(1);
                        ack   = seg_seq.wrapping_add(1);
                        tcp_send_seg(dst_ip, gw_mac, src_port, dst_port, seq, ack, TCP_ACK, &[]);
                        tcp_send_seg(dst_ip, gw_mac, src_port, dst_port, seq, ack, TCP_PSH | TCP_ACK, request);
                        seq   = seq.wrapping_add(request.len() as u32);
                        state = 1;
                    }
                }
                1 => {
                    if !payload.is_empty() {
                        rx.extend_from_slice(payload);
                        ack = seg_seq.wrapping_add(payload.len() as u32);
                        tcp_send_seg(dst_ip, gw_mac, src_port, dst_port, seq, ack, TCP_ACK, &[]);
                    }
                    if flags & TCP_FIN != 0 {
                        ack = ack.wrapping_add(1);
                        tcp_send_seg(dst_ip, gw_mac, src_port, dst_port, seq, ack, TCP_FIN | TCP_ACK, &[]);
                        state = 2;
                        break 'poll;
                    }
                }
                _ => break 'poll,
            }
        }

        // Nothing in ring right now — brief pause, then re-check.
        for _ in 0..10_000u32 { core::hint::spin_loop(); }
    }

    if state == 0 { return Err("tcp: timeout — no SYN-ACK received"); }
    if rx.is_empty() { return Err("tcp: connected but no data received"); }
    Ok(rx)
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
