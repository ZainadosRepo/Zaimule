//! Passerelle NAT sortante (0 port ouvert) pour le mode **Point d'Accès Virtuel Nintendo WFC**
//! (`ZaimuleAP`) de **Zaimule Phase 4**.
//!
//! - Ne lie **aucun port d'écoute** sur la machine ni sur la box du joueur : ouvre uniquement
//!   des sockets UDP / TCP sortants éphémères vers le serveur DNS/WFC choisi par l'utilisateur.
//! - Traduit les paquets IPv4 UDP & TCP émis par le contrôleur Wi-Fi ARM7 de la Nintendo DS
//!   (`nds_core::wifi`) et réinjecte les réponses IPv4 (sommes de contrôle IP/UDP/TCP recalculées)
//!   dans le tampon circulaire RX 802.11b de la console.
//! - Borné strictement en mémoire (max 16 flux UDP, max 16 sessions TCP, expiration 30 s).

use std::collections::{HashMap, VecDeque};
use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream, UdpSocket};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const MAX_UDP_FLOWS: usize = 16;
const MAX_TCP_FLOWS: usize = 16;
const FLOW_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_INBOUND_IP_QUEUE: usize = 64;
const MAX_TCP_SEGMENT_PAYLOAD: usize = 1360;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct FlowKey {
    ds_ip: [u8; 4],
    ds_port: u16,
    remote_ip: [u8; 4],
    remote_port: u16,
}

struct UdpFlow {
    socket: UdpSocket,
    last_active: Instant,
}

struct TcpFlow {
    stream: TcpStream,
    my_seq: u32,
    peer_seq: u32,
    established: bool,
    fin_sent: bool,
    last_active: Instant,
}

struct GatewayInner {
    udp_flows: HashMap<FlowKey, UdpFlow>,
    tcp_flows: HashMap<FlowKey, TcpFlow>,
    inbound_ip: VecDeque<Vec<u8>>,
    ip_ident: u16,
    next_seq_seed: u32,
    forwarded_out: u64,
    forwarded_in: u64,
}

impl Default for GatewayInner {
    fn default() -> Self {
        Self {
            udp_flows: HashMap::new(),
            tcp_flows: HashMap::new(),
            inbound_ip: VecDeque::with_capacity(32),
            ip_ident: 1,
            next_seq_seed: 0x4E44_5301,
            forwarded_out: 0,
            forwarded_in: 0,
        }
    }
}

/// Passerelle NAT sortante non-bloquante pour le trafic IPv4 externe du Point d'Accès Virtuel DS.
#[derive(Default)]
pub struct NdsWfcGateway {
    inner: Mutex<GatewayInner>,
}

impl NdsWfcGateway {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ferme immédiatement tous les sockets NAT actifs et vide les files d'attente.
    pub fn reset(&self) {
        if let Ok(mut g) = self.inner.lock() {
            g.udp_flows.clear();
            g.tcp_flows.clear();
            g.inbound_ip.clear();
        }
    }

    /// Transmet un paquet IPv4 sortant émis par la Nintendo DS (UDP ou TCP) vers sa cible distante.
    pub fn submit_outbound_ipv4(&self, ip: &[u8]) {
        if ip.len() < 20 || (ip[0] >> 4) != 4 {
            return;
        }
        let ihl = usize::from(ip[0] & 0x0F) * 4;
        let total_len = usize::from(u16::from_be_bytes([ip[2], ip[3]])).min(ip.len());
        if ihl < 20 || total_len < ihl {
            return;
        }
        let proto = ip[9];
        let src_ip = [ip[12], ip[13], ip[14], ip[15]];
        let dst_ip = [ip[16], ip[17], ip[18], ip[19]];
        let l4 = &ip[ihl..total_len];

        let Ok(mut g) = self.inner.lock() else {
            return;
        };
        let now = Instant::now();
        g.prune_idle(now);

        match proto {
            17 if l4.len() >= 8 => {
                let ds_port = u16::from_be_bytes([l4[0], l4[1]]);
                let remote_port = u16::from_be_bytes([l4[2], l4[3]]);
                let payload = &l4[8..];
                g.handle_outbound_udp(
                    FlowKey {
                        ds_ip: src_ip,
                        ds_port,
                        remote_ip: dst_ip,
                        remote_port,
                    },
                    payload,
                    now,
                );
            }
            6 if l4.len() >= 20 => {
                let ds_port = u16::from_be_bytes([l4[0], l4[1]]);
                let remote_port = u16::from_be_bytes([l4[2], l4[3]]);
                let seq = u32::from_be_bytes([l4[4], l4[5], l4[6], l4[7]]);
                let data_off = usize::from((l4[12] >> 4) & 0x0F) * 4;
                if data_off < 20 || data_off > l4.len() {
                    return;
                }
                let flags = l4[13];
                let payload = &l4[data_off..];
                g.handle_outbound_tcp(
                    FlowKey {
                        ds_ip: src_ip,
                        ds_port,
                        remote_ip: dst_ip,
                        remote_port,
                    },
                    seq,
                    flags,
                    payload,
                    now,
                );
            }
            _ => {}
        }
    }

    /// Sondage non-bloquant de tous les sockets NAT actifs et récupération des paquets IPv4 entrants.
    pub fn poll_inbound_ipv4(&self) -> Vec<Vec<u8>> {
        let Ok(mut g) = self.inner.lock() else {
            return Vec::new();
        };
        let now = Instant::now();
        g.poll_sockets(now);
        g.prune_idle(now);
        g.inbound_ip.drain(..).collect()
    }
}

impl GatewayInner {
    fn next_ip_id(&mut self) -> u16 {
        let id = self.ip_ident;
        self.ip_ident = self.ip_ident.wrapping_add(1);
        id
    }

    fn push_inbound(&mut self, pkt: Vec<u8>) {
        if self.inbound_ip.len() >= MAX_INBOUND_IP_QUEUE {
            let _ = self.inbound_ip.pop_front();
        }
        self.inbound_ip.push_back(pkt);
        self.forwarded_in = self.forwarded_in.saturating_add(1);
    }

    fn prune_idle(&mut self, now: Instant) {
        self.udp_flows
            .retain(|_, f| now.duration_since(f.last_active) <= FLOW_IDLE_TIMEOUT);
        self.tcp_flows
            .retain(|_, f| now.duration_since(f.last_active) <= FLOW_IDLE_TIMEOUT);
    }

    fn handle_outbound_udp(&mut self, key: FlowKey, payload: &[u8], now: Instant) {
        if !self.udp_flows.contains_key(&key) {
            if self.udp_flows.len() >= MAX_UDP_FLOWS {
                return;
            }
            let Ok(sock) = UdpSocket::bind("0.0.0.0:0") else {
                return;
            };
            if sock.set_nonblocking(true).is_err() {
                return;
            }
            let target = SocketAddrV4::new(Ipv4Addr::from(key.remote_ip), key.remote_port);
            if sock.connect(target).is_err() {
                return;
            }
            self.udp_flows.insert(
                key,
                UdpFlow {
                    socket: sock,
                    last_active: now,
                },
            );
        }
        if let Some(flow) = self.udp_flows.get_mut(&key) {
            if flow.socket.send(payload).is_ok() {
                flow.last_active = now;
                self.forwarded_out = self.forwarded_out.saturating_add(1);
            }
        }
    }

    fn handle_outbound_tcp(
        &mut self,
        key: FlowKey,
        seq: u32,
        flags: u8,
        payload: &[u8],
        now: Instant,
    ) {
        let syn = (flags & 0x02) != 0;
        let fin = (flags & 0x01) != 0;
        let rst = (flags & 0x04) != 0;

        if rst {
            self.tcp_flows.remove(&key);
            return;
        }

        if syn {
            if self.tcp_flows.len() >= MAX_TCP_FLOWS && !self.tcp_flows.contains_key(&key) {
                return;
            }
            let target = SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::from(key.remote_ip),
                key.remote_port,
            ));
            // Connexion sortante courte non-bloquante (timeout 250 ms max pour ne pas bloquer une frame)
            let Ok(stream) = TcpStream::connect_timeout(&target, Duration::from_millis(250)) else {
                let rst_pkt = self.wrap_tcp_ipv4(key, 0, seq.wrapping_add(1), 0x14, &[]);
                self.push_inbound(rst_pkt);
                return;
            };
            let _ = stream.set_nonblocking(true);
            let _ = stream.set_nodelay(true);
            let my_seq = self.next_seq_seed;
            self.next_seq_seed = self.next_seq_seed.wrapping_add(0x0001_0000);
            let peer_seq = seq.wrapping_add(1);

            self.tcp_flows.insert(
                key,
                TcpFlow {
                    stream,
                    my_seq: my_seq.wrapping_add(1),
                    peer_seq,
                    established: true,
                    fin_sent: false,
                    last_active: now,
                },
            );
            let syn_ack = self.wrap_tcp_ipv4(key, my_seq, peer_seq, 0x12, &[]);
            self.push_inbound(syn_ack);
            return;
        }

        let mut ack_to_send: Option<(u32, u32, u8)> = None;
        let mut remove_flow = false;
        if let Some(flow) = self.tcp_flows.get_mut(&key) {
            flow.last_active = now;
            if !payload.is_empty() && flow.established {
                match flow.stream.write_all(payload) {
                    Ok(()) => {
                        flow.peer_seq = seq.wrapping_add(payload.len() as u32);
                        self.forwarded_out = self.forwarded_out.saturating_add(1);
                        ack_to_send = Some((flow.my_seq, flow.peer_seq, 0x10));
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                    Err(_) => {
                        remove_flow = true;
                        ack_to_send = Some((flow.my_seq, flow.peer_seq, 0x14));
                    }
                }
            }
            if fin {
                flow.peer_seq = seq.wrapping_add(payload.len() as u32 + 1);
                let _ = flow.stream.shutdown(std::net::Shutdown::Both);
                ack_to_send = Some((flow.my_seq, flow.peer_seq, 0x11));
                remove_flow = true;
            }
        }
        if remove_flow {
            self.tcp_flows.remove(&key);
        }
        if let Some((my_seq, peer_seq, fl)) = ack_to_send {
            let pkt = self.wrap_tcp_ipv4(key, my_seq, peer_seq, fl, &[]);
            self.push_inbound(pkt);
        }
    }

    fn poll_sockets(&mut self, now: Instant) {
        let mut buf = [0u8; MAX_TCP_SEGMENT_PAYLOAD];

        // 1. Sondage non-bloquant des flux UDP
        let mut udp_ready: Vec<(FlowKey, Vec<u8>)> = Vec::new();
        for (&key, flow) in &mut self.udp_flows {
            for _ in 0..4 {
                match flow.socket.recv(&mut buf) {
                    Ok(n) if n > 0 => {
                        flow.last_active = now;
                        udp_ready.push((key, buf[..n].to_vec()));
                    }
                    _ => break,
                }
            }
        }
        for (key, data) in udp_ready {
            let pkt = self.wrap_udp_ipv4(key, &data);
            self.push_inbound(pkt);
        }

        // 2. Sondage non-bloquant des flux TCP
        let mut tcp_ready: Vec<(FlowKey, u32, u32, u8, Vec<u8>)> = Vec::new();
        let mut closed_keys: Vec<FlowKey> = Vec::new();
        for (&key, flow) in &mut self.tcp_flows {
            if !flow.established || flow.fin_sent {
                continue;
            }
            match flow.stream.read(&mut buf) {
                Ok(0) => {
                    flow.fin_sent = true;
                    tcp_ready.push((key, flow.my_seq, flow.peer_seq, 0x11, Vec::new()));
                    flow.my_seq = flow.my_seq.wrapping_add(1);
                    closed_keys.push(key);
                }
                Ok(n) => {
                    flow.last_active = now;
                    let seg_seq = flow.my_seq;
                    flow.my_seq = flow.my_seq.wrapping_add(n as u32);
                    tcp_ready.push((key, seg_seq, flow.peer_seq, 0x18, buf[..n].to_vec()));
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                Err(_) => {
                    tcp_ready.push((key, flow.my_seq, flow.peer_seq, 0x14, Vec::new()));
                    closed_keys.push(key);
                }
            }
        }
        for key in closed_keys {
            self.tcp_flows.remove(&key);
        }
        for (key, my_seq, peer_seq, flags, data) in tcp_ready {
            let pkt = self.wrap_tcp_ipv4(key, my_seq, peer_seq, flags, &data);
            self.push_inbound(pkt);
        }
    }

    fn wrap_udp_ipv4(&mut self, key: FlowKey, payload: &[u8]) -> Vec<u8> {
        let udp_len = (8 + payload.len()) as u16;
        let mut udp = Vec::with_capacity(usize::from(udp_len));
        udp.extend_from_slice(&key.remote_port.to_be_bytes());
        udp.extend_from_slice(&key.ds_port.to_be_bytes());
        udp.extend_from_slice(&udp_len.to_be_bytes());
        udp.extend_from_slice(&[0x00, 0x00]);
        udp.extend_from_slice(payload);
        let chk = transport_checksum(key.remote_ip, key.ds_ip, 17, &udp);
        let chk = if chk == 0 { 0xFFFF } else { chk };
        udp[6..8].copy_from_slice(&chk.to_be_bytes());
        self.build_ipv4(key.remote_ip, key.ds_ip, 17, &udp)
    }

    fn wrap_tcp_ipv4(
        &mut self,
        key: FlowKey,
        seq: u32,
        ack: u32,
        flags: u8,
        payload: &[u8],
    ) -> Vec<u8> {
        let mut tcp = Vec::with_capacity(20 + payload.len());
        tcp.extend_from_slice(&key.remote_port.to_be_bytes());
        tcp.extend_from_slice(&key.ds_port.to_be_bytes());
        tcp.extend_from_slice(&seq.to_be_bytes());
        tcp.extend_from_slice(&ack.to_be_bytes());
        tcp.push(0x50);
        tcp.push(flags);
        tcp.extend_from_slice(&4096u16.to_be_bytes());
        tcp.extend_from_slice(&[0x00, 0x00]);
        tcp.extend_from_slice(&[0x00, 0x00]);
        tcp.extend_from_slice(payload);
        let chk = transport_checksum(key.remote_ip, key.ds_ip, 6, &tcp);
        tcp[16..18].copy_from_slice(&chk.to_be_bytes());
        self.build_ipv4(key.remote_ip, key.ds_ip, 6, &tcp)
    }

    fn build_ipv4(&mut self, src: [u8; 4], dst: [u8; 4], proto: u8, payload: &[u8]) -> Vec<u8> {
        let total_len = (20 + payload.len()) as u16;
        let id = self.next_ip_id();
        let mut ip = Vec::with_capacity(usize::from(total_len));
        ip.push(0x45);
        ip.push(0x00);
        ip.extend_from_slice(&total_len.to_be_bytes());
        ip.extend_from_slice(&id.to_be_bytes());
        ip.extend_from_slice(&0x4000u16.to_be_bytes());
        ip.push(64);
        ip.push(proto);
        ip.extend_from_slice(&[0x00, 0x00]);
        ip.extend_from_slice(&src);
        ip.extend_from_slice(&dst);
        let chk = ipv4_checksum(&ip[..20]);
        ip[10..12].copy_from_slice(&chk.to_be_bytes());
        ip.extend_from_slice(payload);
        ip
    }
}

fn ipv4_checksum(data: &[u8]) -> u16 {
    let mut sum = 0u32;
    for &pair in data.as_chunks::<2>().0 {
        sum += u32::from(u16::from_be_bytes(pair));
    }
    if data.len() & 1 != 0 {
        sum += u32::from(data[data.len() - 1]) << 8;
    }
    while (sum >> 16) != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

fn transport_checksum(src: [u8; 4], dst: [u8; 4], proto: u8, seg: &[u8]) -> u16 {
    let mut sum = 0u32;
    sum += u32::from(u16::from_be_bytes([src[0], src[1]]));
    sum += u32::from(u16::from_be_bytes([src[2], src[3]]));
    sum += u32::from(u16::from_be_bytes([dst[0], dst[1]]));
    sum += u32::from(u16::from_be_bytes([dst[2], dst[3]]));
    sum += u32::from(proto);
    sum += seg.len() as u32;
    for &pair in seg.as_chunks::<2>().0 {
        sum += u32::from(u16::from_be_bytes(pair));
    }
    if seg.len() & 1 != 0 {
        sum += u32::from(seg[seg.len() - 1]) << 8;
    }
    while (sum >> 16) != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outbound_wfc_nat_gateway_roundtrips_udp_datagram_without_listening_ports() {
        let echo_server = UdpSocket::bind("127.0.0.1:0").expect("bind local udp echo");
        let port = echo_server.local_addr().unwrap().port();
        echo_server
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();

        let server_thread = std::thread::spawn(move || {
            let mut buf = [0u8; 256];
            let (n, peer) = echo_server.recv_from(&mut buf).expect("recv udp");
            let mut reply = b"WFC:".to_vec();
            reply.extend_from_slice(&buf[..n]);
            echo_server.send_to(&reply, peer).expect("send echo");
        });

        let gw = NdsWfcGateway::new();
        let mut inner = GatewayInner::default();
        let out_pkt = inner.wrap_udp_ipv4(
            FlowKey {
                ds_ip: [127, 0, 0, 1],
                ds_port: port,
                remote_ip: [10, 64, 0, 11],
                remote_port: 55123,
            },
            b"PING",
        );
        gw.submit_outbound_ipv4(&out_pkt);

        let mut received = Vec::new();
        for _ in 0..40 {
            let batch = gw.poll_inbound_ipv4();
            if !batch.is_empty() {
                received = batch;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        server_thread.join().unwrap();

        assert_eq!(received.len(), 1);
        let resp_ip = &received[0];
        assert_eq!(&resp_ip[12..16], &[127, 0, 0, 1]);
        assert_eq!(&resp_ip[16..20], &[10, 64, 0, 11]);
        assert_eq!(&resp_ip[28..], b"WFC:PING");
    }
}
