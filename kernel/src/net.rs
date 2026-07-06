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
    // (dns_resolve / udp_send_recv poll the NIC directly for their own reply.)
}

/// Route a destination IP the same way ping/wget/DNS do: everything goes via
/// the SLiRP gateway MAC, since QEMU's user-mode networking doesn't ARP-reply
/// for anything but the gateway itself.
pub const GW_MAC: [u8; 6] = [0x52, 0x55, 0x0a, 0x00, 0x02, 0x02];

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

/// Parse a raw DNS response body (past the UDP header) looking for the
/// first A record. `None` covers both "not a valid/complete answer" and
/// NXDOMAIN — callers can't tell the difference, same as before.
fn parse_dns_answer(dns: &[u8]) -> Option<[u8; 4]> {
    if dns.len() < 12 { return None; }
    let ancount = u16::from_be_bytes([dns[6], dns[7]]) as usize;
    if ancount == 0 { return None; }

    // Skip header + question section (QNAME + QTYPE + QCLASS)
    let mut pos = 12usize;
    loop {
        if pos >= dns.len() { return None; }
        let l = dns[pos] as usize;
        if l == 0 { pos += 1; break; }
        if l & 0xC0 == 0xC0 { pos += 2; break; }
        pos += 1 + l;
    }
    pos += 4; // QTYPE + QCLASS

    // Walk answer RRs looking for an A record
    for _ in 0..ancount {
        if pos + 10 > dns.len() { break; }
        if dns[pos] & 0xC0 == 0xC0 { pos += 2; }
        else {
            while pos < dns.len() && dns[pos] != 0 { pos += 1 + dns[pos] as usize; }
            pos += 1;
        }
        if pos + 10 > dns.len() { break; }
        let rtype = u16::from_be_bytes([dns[pos],   dns[pos+1]]);
        let rdlen = u16::from_be_bytes([dns[pos+8], dns[pos+9]]) as usize;
        pos += 10;
        if rtype == 1 && rdlen == 4 && pos + 4 <= dns.len() {
            return Some([dns[pos], dns[pos+1], dns[pos+2], dns[pos+3]]);
        }
        pos += rdlen;
    }
    None
}

// Rotate through ephemeral ports to avoid TIME_WAIT collisions on repeated calls.
static NEXT_SRC_PORT: core::sync::atomic::AtomicU16 =
    core::sync::atomic::AtomicU16::new(49152);

fn next_src_port() -> u16 {
    let p = NEXT_SRC_PORT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    if p == 0 { NEXT_SRC_PORT.store(49153, core::sync::atomic::Ordering::Relaxed); 49152 } else { p }
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

// ── Async network jobs ────────────────────────────────────────────────────────
// ping/wget/udp used to block the whole main loop (spin-waiting for a reply
// or timeout) — since HepOS's rendering, mouse, and every other window are
// all driven from that same single-threaded loop, a slow/unreachable host
// froze the entire desktop for the duration. These commands now kick off a
// `NetJob`, return immediately, and get polled once per frame from `poll()`
// (called alongside `hda::poll()` in main.rs) until they finish or time out
// — the same "state machine polled once per frame" pattern as HDA playback.

use alloc::string::String;

// Deadlines are TSC-based, not `scheduler::TICK_COUNT`-based: the APIC timer
// only actually advances TICK_COUNT once (the single tick that bootstraps
// kmain into task_blink) — see PLAN.md Known Issues. TSC has no such
// dependency on the timer interrupt actually firing repeatedly.
fn tsc_deadline(ms: u64) -> u64 {
    let tsc_per_ms = crate::hda::TSC_PER_MS.load(core::sync::atomic::Ordering::Relaxed);
    crate::hda::rdtsc().wrapping_add(tsc_per_ms.saturating_mul(ms))
}
fn tsc_expired(deadline: u64) -> bool {
    crate::hda::rdtsc().wrapping_sub(deadline) < u64::MAX / 2
}

/// What to do once a hostname resolves — carries the params needed to start
/// the TCP or UDP job the user actually asked for.
enum NextAction {
    Tcp { port: u16, req: Vec<u8> },
    Udp { port: u16, msg: Vec<u8> },
}

enum NetJob {
    Ping {
        target: [u8; 4], seq: u16, deadline: u64,
    },
    Resolve {
        hostname: String, dns_ip: [u8; 4], src_port: u16, txid: u16,
        deadline: u64, next: NextAction,
    },
    Tcp {
        ip: [u8; 4], port: u16, src_port: u16,
        seq: u32, ack: u32, state: u8, rx: Vec<u8>, req: Vec<u8>, deadline: u64,
    },
    Udp {
        src_port: u16, deadline: u64,
    },
}

/// (issuing terminal window id, job) — only one network operation
/// system-wide at a time, same as the old blocking version (you couldn't
/// run two pings at once either).
static NET_JOB: spin::Mutex<Option<(usize, NetJob)>> = spin::Mutex::new(None);

pub fn job_in_progress() -> bool { NET_JOB.lock().is_some() }

fn build_resolve_job(hostname: &str, next: NextAction) -> NetJob {
    let dns_ip   = [10u8, 0, 2, 3];
    let src_port = 53000u16;
    let txid     = (crate::hda::rdtsc() & 0xFFFF) as u16;

    let mut q: Vec<u8> = Vec::new();
    q.extend_from_slice(&txid.to_be_bytes());
    q.extend_from_slice(&[0x01, 0x00]);
    q.extend_from_slice(&[0x00, 0x01]);
    q.extend_from_slice(&[0x00, 0x00]);
    q.extend_from_slice(&[0x00, 0x00]);
    q.extend_from_slice(&[0x00, 0x00]);
    for label in hostname.split('.') {
        if label.is_empty() { continue; }
        q.push(label.len() as u8);
        q.extend_from_slice(label.as_bytes());
    }
    q.push(0);
    q.extend_from_slice(&[0x00, 0x01]);
    q.extend_from_slice(&[0x00, 0x01]);
    udp_send(dns_ip, GW_MAC, src_port, 53, &q);

    NetJob::Resolve { hostname: String::from(hostname), dns_ip, src_port, txid,
                       deadline: tsc_deadline(2_000), next }
}

fn build_tcp_job(ip: [u8; 4], port: u16, req: Vec<u8>) -> NetJob {
    let src_port = next_src_port();
    let isn = crate::hda::rdtsc() as u32 | 1;
    tcp_send_seg(ip, GW_MAC, src_port, port, isn, 0, TCP_SYN, &[]);
    NetJob::Tcp { ip, port, src_port, seq: isn, ack: 0, state: 0, rx: Vec::new(), req,
                  deadline: tsc_deadline(3_000) }
}

fn build_udp_job(ip: [u8; 4], port: u16, msg: Vec<u8>) -> NetJob {
    let src_port = 51000u16 + (crate::hda::rdtsc() as u16 % 1000);
    udp_send(ip, GW_MAC, src_port, port, &msg);
    NetJob::Udp { src_port, deadline: tsc_deadline(3_000) }
}

/// Start a ping — `issuer` is the terminal window id `poll()` should deliver
/// the eventual result to.
pub fn start_ping(issuer: usize, target: [u8; 4]) -> Result<(), &'static str> {
    if job_in_progress() { return Err("a network operation is already in progress"); }
    if crate::rtl8139::NIC.lock().is_none() && crate::e1000::NIC.lock().is_none() {
        return Err("no NIC found");
    }
    *PING_REPLY.lock() = None;
    let seq = ping_send(target, GW_MAC);
    *NET_JOB.lock() = Some((issuer, NetJob::Ping { target, seq, deadline: tsc_deadline(250) }));
    Ok(())
}

pub fn start_wget(issuer: usize, host_str: &str, port: u16, path: &str) -> Result<(), &'static str> {
    if job_in_progress() { return Err("a network operation is already in progress"); }
    let req = alloc::format!(
        "GET {} HTTP/1.0\r\nHost: {}\r\nConnection: close\r\n\r\n", path, host_str
    ).into_bytes();
    let job = match parse_ip(host_str) {
        Some(ip) => build_tcp_job(ip, port, req),
        None      => build_resolve_job(host_str, NextAction::Tcp { port, req }),
    };
    *NET_JOB.lock() = Some((issuer, job));
    Ok(())
}

pub fn start_udp_cmd(issuer: usize, host_str: &str, port: u16, msg: &str) -> Result<(), &'static str> {
    if job_in_progress() { return Err("a network operation is already in progress"); }
    let msg = msg.as_bytes().to_vec();
    let job = match parse_ip(host_str) {
        Some(ip) => build_udp_job(ip, port, msg),
        None      => build_resolve_job(host_str, NextAction::Udp { port, msg }),
    };
    *NET_JOB.lock() = Some((issuer, job));
    Ok(())
}

enum JobOutcome { Pending, Done(String), Replace(NetJob) }

fn format_wget_result(body: &[u8]) -> String {
    let limit = body.len().min(4096);
    let s = core::str::from_utf8(&body[..limit]).unwrap_or("(non-UTF-8 response)");
    if body.len() > limit {
        alloc::format!("{}\n[… {} bytes truncated]\n", s, body.len() - limit)
    } else {
        alloc::format!("{}\n", s)
    }
}

/// Feed one received Ethernet frame to the in-progress job. Returns whether
/// the job finished, is still pending, or transitioned into a new job (DNS
/// resolution completing and handing off into the real TCP/UDP job).
fn process_frame(job: &mut NetJob, frame: &[u8]) -> JobOutcome {
    if frame.len() < 14 { return JobOutcome::Pending; }
    let etype = u16::from_be_bytes([frame[12], frame[13]]);
    if etype != 0x0800 { return JobOutcome::Pending; }
    let ip_hdr = &frame[14..];
    if ip_hdr.len() < 20 { return JobOutcome::Pending; }

    match job {
        NetJob::Ping { target, seq, .. } => {
            handle_frame(frame); // updates PING_REPLY via the normal ICMP path
            if let Some(got) = *PING_REPLY.lock() {
                if got == *seq {
                    return JobOutcome::Done(alloc::format!(
                        "reply from {}.{}.{}.{}: seq={}",
                        target[0], target[1], target[2], target[3], seq));
                }
            }
            JobOutcome::Pending
        }
        NetJob::Resolve { dns_ip, src_port, txid, next, .. } => {
            if ip_hdr[9] != 17 { return JobOutcome::Pending; }
            let src_ip: [u8; 4] = ip_hdr[12..16].try_into().unwrap_or([0; 4]);
            if src_ip != *dns_ip { return JobOutcome::Pending; }
            let ihl = ((ip_hdr[0] & 0x0F) as usize) * 4;
            if ip_hdr.len() < ihl + 8 { return JobOutcome::Pending; }
            let udp = &ip_hdr[ihl..];
            if u16::from_be_bytes([udp[0], udp[1]]) != 53 { return JobOutcome::Pending; }
            if u16::from_be_bytes([udp[2], udp[3]]) != *src_port { return JobOutcome::Pending; }
            let dns = &udp[8..];
            if dns.len() < 12 { return JobOutcome::Pending; }
            if u16::from_be_bytes([dns[0], dns[1]]) != *txid { return JobOutcome::Pending; }
            if dns[2] & 0x80 == 0 { return JobOutcome::Pending; } // not a response
            match parse_dns_answer(dns) {
                None => JobOutcome::Done(String::from("could not resolve host")),
                Some(ip) => {
                    let taken = core::mem::replace(next, NextAction::Udp { port: 0, msg: Vec::new() });
                    JobOutcome::Replace(match taken {
                        NextAction::Tcp { port, req } => build_tcp_job(ip, port, req),
                        NextAction::Udp { port, msg } => build_udp_job(ip, port, msg),
                    })
                }
            }
        }
        NetJob::Tcp { ip, port, src_port, seq, ack, state, rx, req, .. } => {
            if ip_hdr[9] == 1 { // ICMP — port/host unreachable
                let ihl2 = ((ip_hdr[0] & 0x0F) as usize) * 4;
                if ip_hdr.len() > ihl2 && ip_hdr[ihl2] == 3 {
                    return JobOutcome::Done(String::from("wget: unreachable"));
                }
                return JobOutcome::Pending;
            }
            if ip_hdr[9] != 6 { return JobOutcome::Pending; }
            let src_ip: [u8; 4] = ip_hdr[12..16].try_into().unwrap_or([0; 4]);
            if src_ip != *ip || ip_hdr[16..20] != MY_IP { return JobOutcome::Pending; }
            let ihl = ((ip_hdr[0] & 0x0F) as usize) * 4;
            // Ethernet frames are padded to a minimum size — trim to the IP
            // header's own Total Length field first, or that padding leaks
            // into the TCP payload as phantom trailing zero bytes.
            let ip_total_len = u16::from_be_bytes([ip_hdr[2], ip_hdr[3]]) as usize;
            let ip_body = &ip_hdr[..ip_total_len.min(ip_hdr.len())];
            if ip_body.len() < ihl { return JobOutcome::Pending; }
            let Some((sp, dp, seg_seq, flags, payload)) = tcp_parse(&ip_body[ihl..]) else {
                return JobOutcome::Pending;
            };
            if sp != *port || dp != *src_port { return JobOutcome::Pending; }
            if flags & TCP_RST != 0 { return JobOutcome::Done(String::from("wget: connection refused")); }

            match *state {
                0 => {
                    if flags & (TCP_SYN | TCP_ACK) == (TCP_SYN | TCP_ACK) {
                        *seq = seq.wrapping_add(1);
                        *ack = seg_seq.wrapping_add(1);
                        tcp_send_seg(*ip, GW_MAC, *src_port, *port, *seq, *ack, TCP_ACK, &[]);
                        tcp_send_seg(*ip, GW_MAC, *src_port, *port, *seq, *ack, TCP_PSH | TCP_ACK, req);
                        *seq = seq.wrapping_add(req.len() as u32);
                        *state = 1;
                    }
                    JobOutcome::Pending
                }
                1 => {
                    if !payload.is_empty() {
                        rx.extend_from_slice(payload);
                        *ack = seg_seq.wrapping_add(payload.len() as u32);
                        tcp_send_seg(*ip, GW_MAC, *src_port, *port, *seq, *ack, TCP_ACK, &[]);
                    }
                    if flags & TCP_FIN != 0 {
                        *ack = ack.wrapping_add(1);
                        tcp_send_seg(*ip, GW_MAC, *src_port, *port, *seq, *ack, TCP_FIN | TCP_ACK, &[]);
                        JobOutcome::Done(format_wget_result(rx))
                    } else {
                        JobOutcome::Pending
                    }
                }
                _ => JobOutcome::Pending,
            }
        }
        NetJob::Udp { src_port, .. } => {
            if ip_hdr[9] != 17 { return JobOutcome::Pending; }
            let ihl = ((ip_hdr[0] & 0x0F) as usize) * 4;
            if ip_hdr.len() < ihl + 8 { return JobOutcome::Pending; }
            let udp = &ip_hdr[ihl..];
            if u16::from_be_bytes([udp[2], udp[3]]) != *src_port { return JobOutcome::Pending; }
            let ulen = u16::from_be_bytes([udp[4], udp[5]]) as usize;
            if ulen < 8 || udp.len() < ulen { return JobOutcome::Pending; }
            let sp = u16::from_be_bytes([udp[0], udp[1]]);
            let src_ip: [u8; 4] = ip_hdr[12..16].try_into().unwrap_or([0; 4]);
            let payload = &udp[8..ulen];
            JobOutcome::Done(alloc::format!(
                "Reply from {}.{}.{}.{}:{} ({} bytes):\n{}\n",
                src_ip[0], src_ip[1], src_ip[2], src_ip[3], sp, payload.len(),
                core::str::from_utf8(payload).unwrap_or("(non-UTF-8)")))
        }
    }
}

fn job_deadline(job: &NetJob) -> u64 {
    match job {
        NetJob::Ping { deadline, .. }    => *deadline,
        NetJob::Resolve { deadline, .. } => *deadline,
        NetJob::Tcp { deadline, .. }     => *deadline,
        NetJob::Udp { deadline, .. }     => *deadline,
    }
}

fn job_timeout_message(job: &NetJob) -> String {
    match job {
        NetJob::Ping { target, .. } => alloc::format!(
            "ping: timeout ({}.{}.{}.{})", target[0], target[1], target[2], target[3]),
        NetJob::Resolve { hostname, .. } => alloc::format!("could not resolve host: {}", hostname),
        NetJob::Tcp { .. } => String::from("wget: timeout — no response"),
        NetJob::Udp { .. } => String::from("udp: no reply within timeout (datagram sent either way)"),
    }
}

/// Advance the in-progress network job by one step, if any. Call this once
/// per main-loop frame (alongside `hda::poll()`). Returns `Some((issuer,
/// result))` exactly once, when the job finishes (success, error, or
/// timeout) — the caller is responsible for printing `result` into the
/// issuing terminal window.
pub fn poll() -> Option<(usize, String)> {
    // Drain every currently queued frame (recv() is itself non-blocking —
    // it returns None once the ring is empty, so this loop is bounded).
    loop {
        if NET_JOB.lock().is_none() { return None; }
        let frame = with_nic(|n| n.recv()).flatten();
        let Some(f) = frame else { break; };

        let mut guard = NET_JOB.lock();
        let Some((issuer, job)) = guard.as_mut() else { return None; };
        let issuer = *issuer;
        match process_frame(job, &f) {
            JobOutcome::Pending => {}
            JobOutcome::Done(msg) => { *guard = None; return Some((issuer, msg)); }
            JobOutcome::Replace(newjob) => { *guard = Some((issuer, newjob)); }
        }
    }

    let mut guard = NET_JOB.lock();
    if let Some((issuer, job)) = guard.as_ref() {
        if tsc_expired(job_deadline(job)) {
            let issuer = *issuer;
            let msg = job_timeout_message(job);
            *guard = None;
            return Some((issuer, msg));
        }
    }
    None
}
