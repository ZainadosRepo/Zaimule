//! Contrôleur matériel Wi-Fi ARM7 Nintendo DS (`0x0480_0000`–`0x0480_FFFF`) & Point d'Accès
//! Virtuel Nintendo WFC (`ZaimuleAP`) pour **Zaimule Phase 4**.
//!
//! Architecture fidèle GBATEK (clean-room Rust) :
//! - **RAM Wi-Fi circulaire 8 KiB** (`0x0480_4000..0x0480_5FFF`, miroir `0x0480_6000..0x0480_7FFF`).
//! - **Registres I/O Wi-Fi** (`0x0480_0000..0x0480_0FFF` & miroir `0x0480_8000..0x0480_8FFF`) :
//!   - IRQ Wi-Fi (`W_IF` / `W_IE` → ARM7 `IF` bit 24 `IRQ_WIFI`),
//!   - Tampon circulaire RX (`W_RXBUF_BEGIN`, `W_RXBUF_END`, `W_RXBUF_WRCSR`, `W_RXBUF_WR_ADDR`,
//!     `W_RXBUF_RD_ADDR`, `W_RXBUF_READCSR`, `W_RXBUF_COUNT`, `W_RXBUF_RD_DATA`, `W_RXBUF_GAP`),
//!   - FIFO d'écriture TX (`W_TXBUF_WR_ADDR`, `W_TXBUF_COUNT`, `W_TXBUF_WR_DATA`, `W_TXBUF_GAP`),
//!   - Slots d'émission (`W_TXBUF_LOC1..3`, `W_TXBUF_BEACON`, `W_TXBUF_CMD`, `W_TXBUF_REPLY1..2`),
//!   - Horloge microseconde 64 bits (`W_US_COUNT0..3`, `W_US_COMPARE0..3`, `W_BEACONCOUNT1..2`),
//!   - Baseband (`W_BB_CNT/WRITE/READ`, `BB[0]=0x6D`) et synthétiseur RF (`W_RF_DATA1/2/CNT`).
//! - **Deux modes temps réel sans aucun port ouvert** :
//!   1. **`NdsWifiMode::LocalNiFi`** : pont IEEE 802.11b + Multi-Play (`CMD`/`REPLY`/`ACK`) chiffré
//!      de bout en bout (`ZMR1`, `RelayChannel::NdsWifi = 6`) entre joueurs du salon (Mario Kart DS,
//!      Pokémon Salle Union / Colisée, New Super Mario Bros. VS).
//!   2. **`NdsWifiMode::WfcAccessPoint`** : Point d'Accès Virtuel `ZaimuleAP` (Beacon 802.11b,
//!      Probe, Open Auth, Association `AID=0xC001`, LLC/SNAP, ARP, DHCP, ICMP, DNS, répondeur
//!      `conntest.nintendowifi.net:80`, LAN virtuel `10.64.0.x` sur `ZMR1`, et passerelle NAT
//!      sortante vers serveur WFC privé ou communautaire).

use std::collections::VecDeque;
use zaimule_core::netplay::{
    NdsWifiMode, NdsWifiPacket, NdsWifiTelemetry, NDS_WIFI_KIND_MP_ACK, NDS_WIFI_KIND_MP_CMD,
    NDS_WIFI_KIND_MP_REPLY, NDS_WIFI_KIND_RAW_80211, NDS_WIFI_KIND_WFC_IPV4,
    NDS_WIFI_MAX_FRAME_BYTES,
};

pub const WIFI_RAM_SIZE: usize = 0x2000;
const WIFI_HW_HEADER_LEN: usize = 12;
const MAX_OUTBOUND_QUEUE: usize = 64;

/// Bits d'interruption `W_IF` / `W_IE` (GBATEK `0x0480_8010` / `0x0480_8012`).
pub const WIF_RX_COMPLETE: u16 = 1 << 0;
pub const WIF_TX_COMPLETE: u16 = 1 << 1;
pub const WIF_RX_EVENT: u16 = 1 << 2;
pub const WIF_TX_ERROR: u16 = 1 << 3;
pub const WIF_RX_OVERFLOW: u16 = 1 << 4;
pub const WIF_RX_START: u16 = 1 << 6;
pub const WIF_TX_START: u16 = 1 << 7;
pub const WIF_MP_CMD_DONE: u16 = 1 << 12;
pub const WIF_POST_BEACON: u16 = 1 << 13;
pub const WIF_BEACON_TIMESLOT: u16 = 1 << 14;
pub const WIF_PRE_BEACON: u16 = 1 << 15;

/// Adresse MAC matérielle du Point d'Accès virtuel intégré `ZaimuleAP`.
pub const VIRTUAL_AP_MAC: [u8; 6] = [0x00, 0xF0, 0x5A, 0x41, 0x49, 0x01];
pub const VIRTUAL_AP_SSID: &str = "ZaimuleAP";
pub const VIRTUAL_AP_IP: [u8; 4] = [10, 64, 0, 1];
pub const VIRTUAL_SUBNET_MASK: [u8; 4] = [255, 255, 255, 0];

/// Adresse multicast Multi-Play Nintendo DS : `CMD` (`..00`), `REPLY` (`..01`), `ACK` (`..03`).
const NINTENDO_MP_CMD_MAC: [u8; 6] = [0x03, 0x09, 0xBF, 0x00, 0x00, 0x00];
const NINTENDO_MP_ACK_MAC: [u8; 6] = [0x03, 0x09, 0xBF, 0x00, 0x00, 0x03];

/// Calcule une adresse MAC Nintendo DS déterministe et unique par pair de salon (`peer_id`).
pub fn derive_console_mac(peer_id: u8) -> [u8; 6] {
    let suffix = if peer_id == 0 { 0x01 } else { peer_id };
    [0x00, 0x09, 0xBF, 0x5A, 0x4D, suffix]
}

/// Calcule l'adresse IP virtuelle dans `10.64.0.0/24` attribuée à un pair de salon (`10.64.0.10..`).
pub fn derive_peer_wfc_ip(peer_id: u8) -> [u8; 4] {
    let host = 10u8.saturating_add(peer_id.min(200));
    [10, 64, 0, host]
}

/// Formate une adresse MAC 6 octets en notation hexadécimale `AA:BB:CC:DD:EE:FF`.
pub fn format_mac(mac: [u8; 6]) -> String {
    format!(
        "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    )
}

/// Formate une adresse IPv4 `[a, b, c, d]` en chaîne lisible.
pub fn format_ipv4(ip: [u8; 4]) -> String {
    format!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3])
}

/// CRC-16 Nintendo DS Firmware Wi-Fi (polynôme `0xA001`, valeur initiale `0x0000`).
pub fn crc16_nds_wifi(bytes: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &b in bytes {
        crc ^= u16::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xA001
            } else {
                crc >> 1
            };
        }
    }
    crc
}

/// Patche le firmware DS en mémoire pour :
/// 1. Injecter une adresse MAC unique par pair dans le bloc de calibration Wi-Fi (`0x36..0x3C`)
///    et recalculer son CRC-16 (`0x2A..0x2C`), évitant les collisions entre joueurs d'un même salon.
/// 2. En mode `WfcAccessPoint`, auto-configurer le slot CWF #1 (`0x3FA00` sur 256 KiB / `0x7FA00`
///    sur 512 KiB) sur `ZaimuleAP` (ouvert, DHCP, DNS WFC optionnel, CRC-16 valide).
pub fn patch_firmware_for_wifi(
    fw: &mut [u8],
    mac: [u8; 6],
    mode: NdsWifiMode,
    wfc_dns: Option<[u8; 4]>,
    custom_firmware: bool,
) {
    if fw.len() < 0x400 {
        return;
    }

    // 1. Bloc de calibration Wi-Fi (MAC + CRC16)
    let cal_len = usize::from(u16::from_le_bytes([fw[0x2C], fw[0x2D]]));
    if (0x40..=0x1C0).contains(&cal_len) && 0x2C + cal_len <= fw.len() {
        fw[0x36..0x3C].copy_from_slice(&mac);
        let crc = crc16_nds_wifi(&fw[0x2C..0x2C + cal_len]);
        fw[0x2A..0x2C].copy_from_slice(&crc.to_le_bytes());
    }

    // 2. Slot d'accès Internet WFC #1 (`fw.len() - 0x600`, soit 0x3FA00 sur 256 KiB)
    if fw.len() < 0x1000 {
        return;
    }
    let ap_base = fw.len() - 0x600;
    if mode == NdsWifiMode::WfcAccessPoint {
        let slot = &mut fw[ap_base..ap_base + 0x100];
        slot.fill(0x00);
        let ssid_bytes = VIRTUAL_AP_SSID.as_bytes();
        slot[0x40..0x40 + ssid_bytes.len()].copy_from_slice(ssid_bytes);
        // DHCP automatique (IP=0, Gateway=0, Subnet=0) + DNS WFC explicite si fourni
        if let Some(dns) = wfc_dns {
            slot[0xC8..0xCC].copy_from_slice(&dns);
            slot[0xCC..0xD0].copy_from_slice(&VIRTUAL_AP_IP);
        }
        slot[0xE6] = 0x00; // WEP désactivé (Open)
        slot[0xE7] = 0x00; // Slot configuré et actif
        slot[0xE8] = ssid_bytes.len() as u8;
        slot[0xEA..0xEC].copy_from_slice(&1400u16.to_le_bytes()); // MTU standard DS
        let crc = crc16_nds_wifi(&slot[..0xFE]);
        slot[0xFE..0x100].copy_from_slice(&crc.to_le_bytes());
    } else if !custom_firmware {
        // Firmware synthétique hors mode WFC : laisser le slot #1 proprement non configuré (0xFF)
        let slot = &mut fw[ap_base..ap_base + 0x100];
        slot.fill(0x00);
        slot[0xE7] = 0xFF;
        let crc = crc16_nds_wifi(&slot[..0xFE]);
        slot[0xFE..0x100].copy_from_slice(&crc.to_le_bytes());
    }
}

/// État interne du Point d'Accès Virtuel 802.11b + pile IPv4/DHCP/ARP/DNS/HTTP `ZaimuleAP`.
#[derive(Debug, Clone)]
struct VirtualWfcAp {
    associated: bool,
    client_mac: [u8; 6],
    client_ip: [u8; 4],
    assigned_ip: Option<[u8; 4]>,
    wfc_dns: [u8; 4],
    beacon_accum_us: u64,
    tx_seq: u16,
    ip_ident: u16,
    /// Session TCP locale unique pour `conntest.nintendowifi.net:80` (Test de connexion WFC).
    conntest_seq: u32,
}

impl Default for VirtualWfcAp {
    fn default() -> Self {
        Self {
            associated: false,
            client_mac: derive_console_mac(1),
            client_ip: derive_peer_wfc_ip(1),
            assigned_ip: None,
            wfc_dns: VIRTUAL_AP_IP,
            beacon_accum_us: 0,
            tx_seq: 1,
            ip_ident: 1,
            conntest_seq: 0x5A4D_0001,
        }
    }
}

impl VirtualWfcAp {
    fn configure(&mut self, client_mac: [u8; 6], peer_id: u8, wfc_dns: Option<[u8; 4]>) {
        self.client_mac = client_mac;
        self.client_ip = derive_peer_wfc_ip(peer_id);
        self.wfc_dns = wfc_dns.unwrap_or(VIRTUAL_AP_IP);
        self.beacon_accum_us = 0;
    }

    fn next_seq_ctrl(&mut self) -> u16 {
        let seq = self.tx_seq & 0x0FFF;
        self.tx_seq = self.tx_seq.wrapping_add(1) & 0x0FFF;
        seq << 4
    }

    fn next_ip_id(&mut self) -> u16 {
        let id = self.ip_ident;
        self.ip_ident = self.ip_ident.wrapping_add(1);
        id
    }

    /// Construit une trame Beacon IEEE 802.11b complète pour `ZaimuleAP` (Canal 6, Open).
    fn build_beacon_frame(&mut self, timestamp_us: u64, channel: u8) -> Vec<u8> {
        let mut f = Vec::with_capacity(96);
        let seq_ctrl = self.next_seq_ctrl();
        // 802.11 Management Header (24 octets) : Type=0 (Mgmt), Subtype=8 (Beacon) -> 0x0080
        f.extend_from_slice(&0x0080u16.to_le_bytes());
        f.extend_from_slice(&0x0000u16.to_le_bytes()); // Duration
        f.extend_from_slice(&[0xFF; 6]); // Addr1: Broadcast
        f.extend_from_slice(&VIRTUAL_AP_MAC); // Addr2: SA
        f.extend_from_slice(&VIRTUAL_AP_MAC); // Addr3: BSSID
        f.extend_from_slice(&seq_ctrl.to_le_bytes());
        // Beacon Fixed Params (12 octets)
        f.extend_from_slice(&timestamp_us.to_le_bytes());
        f.extend_from_slice(&100u16.to_le_bytes()); // Beacon interval = 100 TU (~102.4 ms)
        f.extend_from_slice(&0x0021u16.to_le_bytes()); // Capability: ESS | Short Preamble (Open)
                                                       // IE 0: SSID
        let ssid = VIRTUAL_AP_SSID.as_bytes();
        f.push(0x00);
        f.push(ssid.len() as u8);
        f.extend_from_slice(ssid);
        // IE 1: Supported Rates (1 Mbps, 2 Mbps, 5.5 Mbps, 11 Mbps)
        f.extend_from_slice(&[0x01, 0x04, 0x82, 0x84, 0x0B, 0x16]);
        // IE 3: DS Parameter Set (Channel)
        f.extend_from_slice(&[0x03, 0x01, channel.clamp(1, 13)]);
        // IE 5: TIM
        f.extend_from_slice(&[0x05, 0x04, 0x00, 0x01, 0x00, 0x00]);
        f
    }

    fn build_probe_response(
        &mut self,
        dst_mac: [u8; 6],
        timestamp_us: u64,
        channel: u8,
    ) -> Vec<u8> {
        let mut f = self.build_beacon_frame(timestamp_us, channel);
        // Subtype=5 (Probe Response) -> 0x0050, Addr1 = dst_mac
        f[0..2].copy_from_slice(&0x0050u16.to_le_bytes());
        f[4..10].copy_from_slice(&dst_mac);
        f
    }

    fn build_auth_response(&mut self, dst_mac: [u8; 6]) -> Vec<u8> {
        let mut f = Vec::with_capacity(32);
        let seq_ctrl = self.next_seq_ctrl();
        // Type=0 (Mgmt), Subtype=11 (Auth) -> 0x00B0
        f.extend_from_slice(&0x00B0u16.to_le_bytes());
        f.extend_from_slice(&0x0000u16.to_le_bytes());
        f.extend_from_slice(&dst_mac);
        f.extend_from_slice(&VIRTUAL_AP_MAC);
        f.extend_from_slice(&VIRTUAL_AP_MAC);
        f.extend_from_slice(&seq_ctrl.to_le_bytes());
        // Open System (0), Sequence 2, Status Code 0 (Success)
        f.extend_from_slice(&0x0000u16.to_le_bytes());
        f.extend_from_slice(&0x0002u16.to_le_bytes());
        f.extend_from_slice(&0x0000u16.to_le_bytes());
        f
    }

    fn build_assoc_response(&mut self, dst_mac: [u8; 6]) -> Vec<u8> {
        self.associated = true;
        let mut f = Vec::with_capacity(40);
        let seq_ctrl = self.next_seq_ctrl();
        // Type=0 (Mgmt), Subtype=1 (Association Response) -> 0x0010
        f.extend_from_slice(&0x0010u16.to_le_bytes());
        f.extend_from_slice(&0x0000u16.to_le_bytes());
        f.extend_from_slice(&dst_mac);
        f.extend_from_slice(&VIRTUAL_AP_MAC);
        f.extend_from_slice(&VIRTUAL_AP_MAC);
        f.extend_from_slice(&seq_ctrl.to_le_bytes());
        f.extend_from_slice(&0x0021u16.to_le_bytes()); // Capability
        f.extend_from_slice(&0x0000u16.to_le_bytes()); // Status = Success
        f.extend_from_slice(&0xC001u16.to_le_bytes()); // AID = 1 (bits 14..15 = 11)
        f.extend_from_slice(&[0x01, 0x04, 0x82, 0x84, 0x0B, 0x16]);
        f
    }

    /// Encapsule un paquet L3 (ARP `0x0806` ou IPv4 `0x0800`) dans une trame Data IEEE 802.11b
    /// From-DS (`FC = 0x0208`) avec en-tête LLC/SNAP (`AA AA 03 00 00 00 <ethertype>`).
    fn wrap_from_ds_llc_frame(
        &mut self,
        dst_mac: [u8; 6],
        src_mac: [u8; 6],
        ethertype: u16,
        payload: &[u8],
    ) -> Vec<u8> {
        let mut f = Vec::with_capacity(24 + 8 + payload.len());
        let seq_ctrl = self.next_seq_ctrl();
        // Frame Control: Data (type=2, subtype=0 -> 0x0008), FromDS=1 (bit 9 -> 0x0200)
        f.extend_from_slice(&0x0208u16.to_le_bytes());
        f.extend_from_slice(&0x0000u16.to_le_bytes());
        f.extend_from_slice(&dst_mac); // Addr1: DA (DS client)
        f.extend_from_slice(&VIRTUAL_AP_MAC); // Addr2: BSSID (AP)
        f.extend_from_slice(&src_mac); // Addr3: SA (Gateway or remote peer)
        f.extend_from_slice(&seq_ctrl.to_le_bytes());
        // LLC/SNAP
        f.extend_from_slice(&[0xAA, 0xAA, 0x03, 0x00, 0x00, 0x00]);
        f.extend_from_slice(&ethertype.to_be_bytes());
        f.extend_from_slice(payload);
        f
    }

    /// Traite une trame IEEE 802.11b émise par la DS vers `ZaimuleAP`.
    /// Retourne `(réponses 802.11b à injecter en RX, paquet IPv4 LAN/WFC éventuel à relayer)`.
    fn handle_station_tx(
        &mut self,
        frame: &[u8],
        timestamp_us: u64,
        channel: u8,
    ) -> (Vec<Vec<u8>>, Option<WfcForwardedIp>) {
        let mut replies = Vec::new();
        let mut forwarded = None;
        if frame.len() < 24 {
            return (replies, forwarded);
        }
        let fc = u16::from_le_bytes([frame[0], frame[1]]);
        let ftype = (fc >> 2) & 0x03;
        let subtype = (fc >> 4) & 0x0F;
        let src_mac: [u8; 6] = frame[10..16].try_into().unwrap_or(self.client_mac);

        match (ftype, subtype) {
            // Management : Probe Request (4)
            (0, 4) => {
                replies.push(self.build_probe_response(src_mac, timestamp_us, channel));
            }
            // Management : Authentication (11)
            (0, 11) => {
                replies.push(self.build_auth_response(src_mac));
            }
            // Management : Association Request (0) / Reassociation Request (2)
            (0, 0) | (0, 2) => {
                self.client_mac = src_mac;
                replies.push(self.build_assoc_response(src_mac));
            }
            // Management : Disassociation (10) / Deauthentication (12)
            (0, 10) | (0, 12) => {
                self.associated = false;
            }
            // Data frame (type = 2) avec LLC/SNAP
            (2, _)
                if frame.len() >= 32 && frame[24..30] == [0xAA, 0xAA, 0x03, 0x00, 0x00, 0x00] =>
            {
                let ethertype = u16::from_be_bytes([frame[30], frame[31]]);
                let payload = &frame[32..];
                if ethertype == 0x0806 {
                    // ARP
                    if let Some(arp_reply) = self.handle_arp(payload) {
                        replies.push(self.wrap_from_ds_llc_frame(
                            src_mac,
                            VIRTUAL_AP_MAC,
                            0x0806,
                            &arp_reply,
                        ));
                    }
                } else if ethertype == 0x0800 {
                    // IPv4
                    let (ip_replies, fwd) = self.handle_ipv4(payload);
                    for ip_pkt in ip_replies {
                        replies.push(self.wrap_from_ds_llc_frame(
                            src_mac,
                            VIRTUAL_AP_MAC,
                            0x0800,
                            &ip_pkt,
                        ));
                    }
                    forwarded = fwd;
                }
            }
            _ => {}
        }

        (replies, forwarded)
    }

    fn handle_arp(&self, arp: &[u8]) -> Option<Vec<u8>> {
        if arp.len() < 28 {
            return None;
        }
        let op = u16::from_be_bytes([arp[6], arp[7]]);
        if op != 1 {
            return None; // On ne répond qu'aux ARP Request (1)
        }
        let sender_mac: [u8; 6] = arp[8..14].try_into().ok()?;
        let sender_ip: [u8; 4] = arp[14..18].try_into().ok()?;
        let target_ip: [u8; 4] = arp[24..28].try_into().ok()?;
        // Répondre pour la passerelle 10.64.0.1 et pour tout pair du LAN virtuel 10.64.0.x
        let reply_mac = if target_ip == VIRTUAL_AP_IP {
            VIRTUAL_AP_MAC
        } else if target_ip[0] == 10
            && target_ip[1] == 64
            && target_ip[2] == 0
            && target_ip[3] >= 10
        {
            derive_console_mac(target_ip[3].saturating_sub(10))
        } else {
            return None;
        };
        let mut out = vec![0u8; 28];
        out[0..2].copy_from_slice(&1u16.to_be_bytes()); // Ethernet
        out[2..4].copy_from_slice(&0x0800u16.to_be_bytes()); // IPv4
        out[4] = 6;
        out[5] = 4;
        out[6..8].copy_from_slice(&2u16.to_be_bytes()); // ARP Reply
        out[8..14].copy_from_slice(&reply_mac);
        out[14..18].copy_from_slice(&target_ip);
        out[18..24].copy_from_slice(&sender_mac);
        out[24..28].copy_from_slice(&sender_ip);
        Some(out)
    }

    fn handle_ipv4(&mut self, ip: &[u8]) -> (Vec<Vec<u8>>, Option<WfcForwardedIp>) {
        let mut replies = Vec::new();
        if ip.len() < 20 || (ip[0] >> 4) != 4 {
            return (replies, None);
        }
        let ihl = usize::from(ip[0] & 0x0F) * 4;
        let total_len = usize::from(u16::from_be_bytes([ip[2], ip[3]])).min(ip.len());
        if ihl < 20 || total_len < ihl {
            return (replies, None);
        }
        let proto = ip[9];
        let src_ip: [u8; 4] = [ip[12], ip[13], ip[14], ip[15]];
        let dst_ip: [u8; 4] = [ip[16], ip[17], ip[18], ip[19]];
        let l4 = &ip[ihl..total_len];

        // 1. ICMP Echo Request vers la passerelle 10.64.0.1
        if proto == 1 && dst_ip == VIRTUAL_AP_IP && l4.len() >= 8 && l4[0] == 8 {
            let mut icmp = l4.to_vec();
            icmp[0] = 0; // Echo Reply
            icmp[2] = 0;
            icmp[3] = 0;
            let chk = ipv4_checksum(&icmp);
            icmp[2..4].copy_from_slice(&chk.to_be_bytes());
            replies.push(self.build_ipv4_packet(VIRTUAL_AP_IP, src_ip, 1, &icmp));
            return (replies, None);
        }

        // 2. UDP : DHCP (68->67) ou DNS (port 53)
        if proto == 17 && l4.len() >= 8 {
            let src_port = u16::from_be_bytes([l4[0], l4[1]]);
            let dst_port = u16::from_be_bytes([l4[2], l4[3]]);
            let udp_payload = &l4[8..];
            if dst_port == 67 {
                if let Some(dhcp_resp) = self.handle_dhcp(udp_payload) {
                    let udp = build_udp_segment(67, src_port, VIRTUAL_AP_IP, [255; 4], &dhcp_resp);
                    replies.push(self.build_ipv4_packet(VIRTUAL_AP_IP, [255; 4], 17, &udp));
                }
                return (replies, None);
            }
            if dst_port == 53 && dst_ip == VIRTUAL_AP_IP {
                if let Some(dns_resp) = self.handle_local_dns(udp_payload) {
                    let udp = build_udp_segment(53, src_port, VIRTUAL_AP_IP, src_ip, &dns_resp);
                    replies.push(self.build_ipv4_packet(VIRTUAL_AP_IP, src_ip, 17, &udp));
                    return (replies, None);
                }
                // Si le domaine n'est pas `conntest` et qu'un DNS WFC externe est configuré, relayer
                if self.wfc_dns != VIRTUAL_AP_IP {
                    return (
                        replies,
                        Some(WfcForwardedIp::External(ip[..total_len].to_vec())),
                    );
                }
                return (replies, None);
            }
        }

        // 3. TCP port 80 sur 10.64.0.1 : répondeur intégré `conntest.nintendowifi.net`
        if proto == 6 && dst_ip == VIRTUAL_AP_IP && l4.len() >= 20 {
            let dst_port = u16::from_be_bytes([l4[2], l4[3]]);
            if dst_port == 80 {
                let tcp_replies = self.handle_conntest_tcp(src_ip, l4);
                replies.extend(tcp_replies);
                return (replies, None);
            }
        }

        // 4. Trafic IPv4 entre pairs du salon virtuel (`10.64.0.10..254`) -> tunnel E2EE ZMR1
        if dst_ip[0] == 10
            && dst_ip[1] == 64
            && dst_ip[2] == 0
            && dst_ip[3] >= 10
            && dst_ip != self.client_ip
        {
            return (
                replies,
                Some(WfcForwardedIp::PeerLan(ip[..total_len].to_vec())),
            );
        }

        // 5. Trafic vers un serveur WFC externe/auto-hébergé -> passerelle NAT sortante `nds_wfc`
        if dst_ip != VIRTUAL_AP_IP && dst_ip != [255; 4] {
            return (
                replies,
                Some(WfcForwardedIp::External(ip[..total_len].to_vec())),
            );
        }

        (replies, None)
    }

    fn handle_dhcp(&mut self, bootp: &[u8]) -> Option<Vec<u8>> {
        if bootp.len() < 244 || bootp[0] != 1 {
            return None;
        }
        // Magic cookie DHCP 99.130.83.99
        if bootp[236..240] != [99, 130, 83, 99] {
            return None;
        }
        let mut msg_type = 0u8;
        let mut i = 240usize;
        while i < bootp.len() {
            let opt = bootp[i];
            if opt == 255 {
                break;
            }
            if opt == 0 {
                i += 1;
                continue;
            }
            if i + 1 >= bootp.len() {
                break;
            }
            let len = usize::from(bootp[i + 1]);
            if opt == 53 && len >= 1 && i + 2 < bootp.len() {
                msg_type = bootp[i + 2];
            }
            i = i.saturating_add(2 + len);
        }
        let reply_type = match msg_type {
            1 => 2u8, // DISCOVER -> OFFER
            3 => {
                self.assigned_ip = Some(self.client_ip);
                5u8 // REQUEST -> ACK
            }
            _ => return None,
        };

        let mut resp = vec![0u8; 240];
        resp[0] = 2; // BOOTREPLY
        resp[1] = 1; // Ethernet
        resp[2] = 6; // MAC len
        resp[4..8].copy_from_slice(&bootp[4..8]); // XID
        resp[16..20].copy_from_slice(&self.client_ip); // YIADDR
        resp[20..24].copy_from_slice(&VIRTUAL_AP_IP); // SIADDR
        resp[28..34].copy_from_slice(&bootp[28..34]); // CHADDR
        resp[236..240].copy_from_slice(&[99, 130, 83, 99]); // Magic cookie
                                                            // Options DHCP
        resp.extend_from_slice(&[53, 1, reply_type]); // Message Type
        resp.extend_from_slice(&[54, 4]);
        resp.extend_from_slice(&VIRTUAL_AP_IP); // Server ID
        resp.extend_from_slice(&[51, 4, 0, 1, 0x51, 0x80]); // Lease 86400s
        resp.extend_from_slice(&[1, 4]);
        resp.extend_from_slice(&VIRTUAL_SUBNET_MASK); // Subnet Mask
        resp.extend_from_slice(&[3, 4]);
        resp.extend_from_slice(&VIRTUAL_AP_IP); // Router
        resp.extend_from_slice(&[6, 4]);
        resp.extend_from_slice(&self.wfc_dns); // Primary DNS
        resp.push(255); // End
        Some(resp)
    }

    fn handle_local_dns(&self, query: &[u8]) -> Option<Vec<u8>> {
        if query.len() < 16 {
            return None;
        }
        let qname_end = find_dns_qname_end(query, 12)?;
        if qname_end + 4 > query.len() {
            return None;
        }
        let domain = decode_dns_name(query, 12).to_ascii_lowercase();
        // Résoudre `conntest.nintendowifi.net` localement vers `10.64.0.1` pour valider le test CWF,
        // et les autres domaines Nintendo WFC vers `self.wfc_dns` (ou `10.64.0.1` en mode privé pur).
        let resolved_ip = if domain.contains("conntest") || self.wfc_dns == VIRTUAL_AP_IP {
            VIRTUAL_AP_IP
        } else {
            return None; // Laisser le DNS WFC configuré répondre via NAT
        };

        let mut resp = Vec::with_capacity(qname_end + 20);
        resp.extend_from_slice(&query[0..2]); // Transaction ID
        resp.extend_from_slice(&0x8180u16.to_be_bytes()); // Standard response, no error
        resp.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT = 1
        resp.extend_from_slice(&1u16.to_be_bytes()); // ANCOUNT = 1
        resp.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT = 0
        resp.extend_from_slice(&0u16.to_be_bytes()); // ARCOUNT = 0
        resp.extend_from_slice(&query[12..qname_end + 4]); // Question section
                                                           // Answer: pointer 0xC00C, Type A (1), Class IN (1), TTL 300s, RDLEN 4, IPv4
        resp.extend_from_slice(&[
            0xC0, 0x0C, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x01, 0x2C, 0x00, 0x04,
        ]);
        resp.extend_from_slice(&resolved_ip);
        Some(resp)
    }

    fn handle_conntest_tcp(&mut self, client_ip: [u8; 4], tcp: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let client_port = u16::from_be_bytes([tcp[0], tcp[1]]);
        let seq = u32::from_be_bytes([tcp[4], tcp[5], tcp[6], tcp[7]]);
        let data_off = usize::from((tcp[12] >> 4) & 0x0F) * 4;
        if data_off < 20 || data_off > tcp.len() {
            return out;
        }
        let flags = tcp[13];
        let payload = &tcp[data_off..];
        let syn = (flags & 0x02) != 0;
        let fin = (flags & 0x01) != 0;

        if syn {
            let my_seq = self.conntest_seq;
            self.conntest_seq = my_seq.wrapping_add(1);
            let seg = build_tcp_segment(
                80,
                client_port,
                VIRTUAL_AP_IP,
                client_ip,
                my_seq,
                seq.wrapping_add(1),
                0x12, // SYN+ACK
                &[],
            );
            out.push(self.build_ipv4_packet(VIRTUAL_AP_IP, client_ip, 6, &seg));
        } else if !payload.is_empty() {
            let http_ok = b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nX-Organization: Nintendo\r\nContent-Length: 32\r\nConnection: close\r\n\r\n<html><body>OK</body></html>\r\n\r\n";
            let my_seq = self.conntest_seq;
            let ack_num = seq.wrapping_add(payload.len() as u32);
            let seg = build_tcp_segment(
                80,
                client_port,
                VIRTUAL_AP_IP,
                client_ip,
                my_seq,
                ack_num,
                0x19, // PSH+ACK+FIN
                http_ok,
            );
            self.conntest_seq = my_seq.wrapping_add(http_ok.len() as u32 + 1);
            out.push(self.build_ipv4_packet(VIRTUAL_AP_IP, client_ip, 6, &seg));
        } else if fin {
            let seg = build_tcp_segment(
                80,
                client_port,
                VIRTUAL_AP_IP,
                client_ip,
                self.conntest_seq,
                seq.wrapping_add(1),
                0x10, // ACK
                &[],
            );
            out.push(self.build_ipv4_packet(VIRTUAL_AP_IP, client_ip, 6, &seg));
        }
        out
    }

    fn build_ipv4_packet(
        &mut self,
        src: [u8; 4],
        dst: [u8; 4],
        proto: u8,
        payload: &[u8],
    ) -> Vec<u8> {
        let total_len = (20 + payload.len()) as u16;
        let id = self.next_ip_id();
        let mut pkt = Vec::with_capacity(usize::from(total_len));
        pkt.push(0x45); // Version 4, IHL 5
        pkt.push(0x00); // DSCP/ECN
        pkt.extend_from_slice(&total_len.to_be_bytes());
        pkt.extend_from_slice(&id.to_be_bytes());
        pkt.extend_from_slice(&0x4000u16.to_be_bytes()); // Don't Fragment
        pkt.push(64); // TTL
        pkt.push(proto);
        pkt.extend_from_slice(&[0x00, 0x00]); // Checksum placeholder
        pkt.extend_from_slice(&src);
        pkt.extend_from_slice(&dst);
        let chk = ipv4_checksum(&pkt[..20]);
        pkt[10..12].copy_from_slice(&chk.to_be_bytes());
        pkt.extend_from_slice(payload);
        pkt
    }
}

enum WfcForwardedIp {
    PeerLan(Vec<u8>),
    External(Vec<u8>),
}

/// Contrôleur Wi-Fi Nintendo DS complet (`0x0480_0000`–`0x0480_FFFF`).
#[derive(Debug, Clone)]
pub struct NdsWifiController {
    pub mode: NdsWifiMode,
    pub peer_id: u8,
    pub wifi_ram: Vec<u8>,
    io_regs: Vec<u16>,
    pub bb: Vec<u8>,
    pub bb_read: u16,
    rf_channel: u8,

    // Interruptions
    pub w_if: u16,
    pub w_ie: u16,
    pub irq_count: u64,

    // Identité & filtres
    pub mac_addr: [u8; 6],
    pub bssid: [u8; 6],
    pub aid_low: u16,
    pub aid_full: u16,
    pub mode_rst: u16,
    pub mode_wep: u16,
    pub powerstate: u16,
    pub power_us: u16,
    pub powerforce: u16,
    pub rf_status: u16,
    pub rf_pins: u16,
    pub preamble: u16,
    pub rxfilter: u16,
    pub rxfilter2: u16,
    pub rxcnt: u16,

    // Tampon circulaire RX
    pub rxbuf_begin: u16,
    pub rxbuf_end: u16,
    pub rxbuf_wrcsr: u16,
    pub rxbuf_wr_addr: u16,
    pub rxbuf_rd_addr: u16,
    pub rxbuf_readcsr: u16,
    pub rxbuf_count: u16,
    pub rxbuf_gap: u16,
    pub rxbuf_gapdisp: u16,

    // FIFO écriture TX & Slots d'émission
    pub txbuf_wr_addr: u16,
    pub txbuf_count: u16,
    pub txbuf_gap: u16,
    pub txbuf_gapdisp: u16,
    pub txbuf_beacon: u16,
    pub txbuf_cmd: u16,
    pub txbuf_reply1: u16,
    pub txbuf_reply2: u16,
    pub txbuf_loc: [u16; 3],
    pub txreq: u16,
    pub txbusy: u16,
    pub txstat: u16,
    pub txstatcnt: u16,
    pub tx_hdr_cnt: u16,
    pub tx_seqno: u16,

    // Horloges microseconde & Multi-Play
    pub us_count: u64,
    pub us_count_en: bool,
    pub us_compare: u64,
    pub us_compare_en: bool,
    pub beacon_int: u16,
    pub beacon_count1: u16,
    pub beacon_count2: u16,
    pub beacon_tu_accum: u64,
    pub cmd_count: u16,
    pub cmd_count_en: bool,
    pub cmd_10us_accum: u64,
    pub cmd_totaltime: u16,
    pub cmd_replytime: u16,
    random_lfsr: u16,

    // Files réseau & statistiques
    seq_counter: u32,
    pub tx_frames: u64,
    pub rx_frames: u64,
    pub mp_cycles: u64,
    outbound_tx: VecDeque<NdsWifiPacket>,
    outbound_external_ip: VecDeque<Vec<u8>>,
    ap: VirtualWfcAp,
}

impl Default for NdsWifiController {
    fn default() -> Self {
        Self::new()
    }
}

impl NdsWifiController {
    pub fn new() -> Self {
        let mut bb = vec![0u8; 0x100];
        bb[0x00] = 0x6D; // Chip ID BB attendu par Nintendo WM
        bb[0x13] = 0x00;
        bb[0x1E] = 0x03;
        bb[0x64] = 0xFF;
        let mac = derive_console_mac(1);
        let mut ctrl = Self {
            mode: NdsWifiMode::Off,
            peer_id: 1,
            wifi_ram: vec![0u8; WIFI_RAM_SIZE],
            io_regs: vec![0u16; 0x200],
            bb,
            bb_read: 0,
            rf_channel: 6,
            w_if: 0,
            w_ie: 0,
            irq_count: 0,
            mac_addr: mac,
            bssid: [0u8; 6],
            aid_low: 0,
            aid_full: 0,
            mode_rst: 0,
            mode_wep: 0,
            powerstate: 0x0200,
            power_us: 0,
            powerforce: 0,
            rf_status: 0,
            rf_pins: 0x0004,
            preamble: 0x0001,
            rxfilter: 0x0401,
            rxfilter2: 0x0008,
            rxcnt: 0,
            rxbuf_begin: 0x4000,
            rxbuf_end: 0x4800,
            rxbuf_wrcsr: 0,
            rxbuf_wr_addr: 0,
            rxbuf_rd_addr: 0,
            rxbuf_readcsr: 0,
            rxbuf_count: 0,
            rxbuf_gap: 0,
            rxbuf_gapdisp: 0,
            txbuf_wr_addr: 0,
            txbuf_count: 0,
            txbuf_gap: 0,
            txbuf_gapdisp: 0,
            txbuf_beacon: 0,
            txbuf_cmd: 0,
            txbuf_reply1: 0,
            txbuf_reply2: 0,
            txbuf_loc: [0; 3],
            txreq: 0x0010,
            txbusy: 0,
            txstat: 0,
            txstatcnt: 0,
            tx_hdr_cnt: 0,
            tx_seqno: 0,
            us_count: 0,
            us_count_en: false,
            us_compare: 0,
            us_compare_en: false,
            beacon_int: 100,
            beacon_count1: 100,
            beacon_count2: 0,
            beacon_tu_accum: 0,
            cmd_count: 0,
            cmd_count_en: false,
            cmd_10us_accum: 0,
            cmd_totaltime: 0,
            cmd_replytime: 0,
            random_lfsr: 0x0001,
            seq_counter: 0,
            tx_frames: 0,
            rx_frames: 0,
            mp_cycles: 0,
            outbound_tx: VecDeque::with_capacity(32),
            outbound_external_ip: VecDeque::with_capacity(32),
            ap: VirtualWfcAp::default(),
        };
        ctrl.ap.configure(mac, 1, None);
        ctrl
    }

    pub fn configure(&mut self, mode: NdsWifiMode, peer_id: u8, wfc_dns: Option<[u8; 4]>) {
        self.mode = mode;
        self.peer_id = peer_id.max(1);
        self.mac_addr = derive_console_mac(self.peer_id);
        self.ap.configure(self.mac_addr, self.peer_id, wfc_dns);
        if mode == NdsWifiMode::Off {
            self.ap.associated = false;
            self.ap.assigned_ip = None;
            self.outbound_tx.clear();
            self.outbound_external_ip.clear();
        }
    }

    pub fn telemetry(&self) -> NdsWifiTelemetry {
        NdsWifiTelemetry {
            mode: self.mode,
            mac_address: format_mac(self.mac_addr),
            channel: self.rf_channel,
            ap_ssid: VIRTUAL_AP_SSID.to_string(),
            ap_associated: self.ap.associated,
            assigned_ip: self.ap.assigned_ip.map(format_ipv4),
            wfc_dns: format_ipv4(self.ap.wfc_dns),
            tx_frames: self.tx_frames,
            rx_frames: self.rx_frames,
            mp_cycles: self.mp_cycles,
            irq_count: self.irq_count,
        }
    }

    #[inline]
    pub fn irq_pending(&self) -> bool {
        (self.w_if & self.w_ie) != 0
    }

    fn raise_irq_bits(&mut self, bits: u16) -> bool {
        let before = self.irq_pending();
        self.w_if |= bits;
        let now = self.irq_pending();
        if now && !before {
            self.irq_count = self.irq_count.saturating_add(1);
        }
        now
    }

    /// Lecture 16 bits sur `0x0480_0000..0x0480_FFFF`.
    pub fn read16(&mut self, addr: u32) -> u16 {
        let off = (addr & 0xFFFF) as usize;
        // 0x4000..0x7FFF : RAM Wi-Fi 8 KiB (miroir sur 0x6000..0x7FFF)
        if (0x4000..0x8000).contains(&off) {
            let idx = (off - 0x4000) & (WIFI_RAM_SIZE - 2);
            return u16::from_le_bytes([self.wifi_ram[idx], self.wifi_ram[idx + 1]]);
        }
        let reg = off & 0x0FFF;
        match reg {
            0x000 => 0x1440, // W_ID
            0x004 => self.mode_rst,
            0x006 => self.mode_wep,
            0x008 => self.txstatcnt,
            0x010 => self.w_if,
            0x012 => self.w_ie,
            0x018 => u16::from_le_bytes([self.mac_addr[0], self.mac_addr[1]]),
            0x01A => u16::from_le_bytes([self.mac_addr[2], self.mac_addr[3]]),
            0x01C => u16::from_le_bytes([self.mac_addr[4], self.mac_addr[5]]),
            0x020 => u16::from_le_bytes([self.bssid[0], self.bssid[1]]),
            0x022 => u16::from_le_bytes([self.bssid[2], self.bssid[3]]),
            0x024 => u16::from_le_bytes([self.bssid[4], self.bssid[5]]),
            0x028 => self.aid_low,
            0x02A => self.aid_full,
            0x030 => self.rxcnt,
            0x036 => self.power_us,
            0x03C => self.powerstate,
            0x040 => self.powerforce,
            0x044 => self.next_random(),
            0x050 => self.rxbuf_begin,
            0x052 => self.rxbuf_end,
            0x054 => self.rxbuf_wrcsr,
            0x056 => self.rxbuf_wr_addr,
            0x058 => self.rxbuf_rd_addr,
            0x05A => self.rxbuf_readcsr,
            0x05C => self.rxbuf_count,
            0x060 => self.pop_rxbuf_rd_data(),
            0x062 => self.rxbuf_gap,
            0x064 => self.rxbuf_gapdisp,
            0x068 => self.txbuf_wr_addr,
            0x06C => self.txbuf_count,
            0x074 => self.txbuf_gap,
            0x076 => self.txbuf_gapdisp,
            0x080 => self.txbuf_beacon,
            0x08C => self.beacon_int,
            0x090 => self.txbuf_cmd,
            0x094 => self.txbuf_reply1,
            0x098 => self.txbuf_reply2,
            0x0A0 => self.txbuf_loc[0],
            0x0A4 => self.txbuf_loc[1],
            0x0A8 => self.txbuf_loc[2],
            0x0B0 => self.txreq,
            0x0B6 => self.txbusy,
            0x0B8 => self.txstat,
            0x0BC => self.preamble,
            0x0C0 => self.cmd_totaltime,
            0x0C4 => self.cmd_replytime,
            0x0D0 => self.rxfilter,
            0x0E0 => self.rxfilter2,
            0x0E8 => u16::from(self.us_count_en),
            0x0EA => u16::from(self.us_compare_en),
            0x0EE => u16::from(self.cmd_count_en),
            0x0F0 => (self.us_compare & 0xFFFF) as u16,
            0x0F2 => ((self.us_compare >> 16) & 0xFFFF) as u16,
            0x0F4 => ((self.us_compare >> 32) & 0xFFFF) as u16,
            0x0F6 => ((self.us_compare >> 48) & 0xFFFF) as u16,
            0x0F8 => (self.us_count & 0xFFFF) as u16,
            0x0FA => ((self.us_count >> 16) & 0xFFFF) as u16,
            0x0FC => ((self.us_count >> 32) & 0xFFFF) as u16,
            0x0FE => ((self.us_count >> 48) & 0xFFFF) as u16,
            0x110 => self.beacon_count1,
            0x114 => self.beacon_count2,
            0x118 => self.cmd_count,
            0x15C => self.bb_read,
            0x15E => 0, // W_BB_BUSY : jamais occupé
            0x180 => 0, // W_RF_BUSY : jamais occupé
            0x194 => self.tx_hdr_cnt,
            0x19C => self.rf_pins,
            0x210 => self.tx_seqno & 0x0FFF,
            0x214 => self.rf_status,
            _ => {
                let slot = (reg >> 1) & 0x1FF;
                self.io_regs[slot]
            }
        }
    }

    /// Écriture 16 bits sur `0x0480_0000..0x0480_FFFF`. Retourne `true` si `IRQ_WIFI` doit être levée.
    pub fn write16(&mut self, addr: u32, v: u16) -> bool {
        let off = (addr & 0xFFFF) as usize;
        if (0x4000..0x8000).contains(&off) {
            let idx = (off - 0x4000) & (WIFI_RAM_SIZE - 2);
            self.wifi_ram[idx] = v as u8;
            self.wifi_ram[idx + 1] = (v >> 8) as u8;
            return false;
        }
        let reg = off & 0x0FFF;
        let slot = (reg >> 1) & 0x1FF;
        self.io_regs[slot] = v;

        match reg {
            0x004 => {
                let prev_b0 = (self.mode_rst & 1) != 0;
                self.mode_rst = v & 0x9FFF; // bits 13/14 write-only
                if !prev_b0 && (v & 1) != 0 {
                    self.rf_pins = 0x0046;
                    self.rf_status = 0x0009;
                }
                if (v & (1 << 13)) != 0 {
                    self.rxbuf_wr_addr = 0;
                    self.cmd_totaltime = 0;
                    self.cmd_replytime = 0;
                }
                if (v & (1 << 14)) != 0 {
                    self.mode_wep = 0;
                    self.txstatcnt = 0;
                    self.aid_low = 0;
                    self.aid_full = 0;
                    self.rxbuf_begin = 0x4000;
                    self.rxbuf_end = 0x4800;
                    self.preamble = 0x0001;
                    self.rxfilter = 0x0401;
                    self.rxfilter2 = 0x0008;
                    self.tx_hdr_cnt = 0;
                }
            }
            0x006 => self.mode_wep = v & 0x007F,
            0x008 => self.txstatcnt = v,
            0x010 => {
                // W_IF : Write-1-to-Clear !
                self.w_if &= !v;
            }
            0x012 => {
                self.w_ie = v;
            }
            0x018 => {
                self.mac_addr[0] = v as u8;
                self.mac_addr[1] = (v >> 8) as u8;
            }
            0x01A => {
                self.mac_addr[2] = v as u8;
                self.mac_addr[3] = (v >> 8) as u8;
            }
            0x01C => {
                self.mac_addr[4] = v as u8;
                self.mac_addr[5] = (v >> 8) as u8;
            }
            0x020 => {
                self.bssid[0] = v as u8;
                self.bssid[1] = (v >> 8) as u8;
            }
            0x022 => {
                self.bssid[2] = v as u8;
                self.bssid[3] = (v >> 8) as u8;
            }
            0x024 => {
                self.bssid[4] = v as u8;
                self.bssid[5] = (v >> 8) as u8;
            }
            0x028 => self.aid_low = v & 0x000F,
            0x02A => self.aid_full = v & 0x07FF,
            0x030 => {
                self.rxcnt = v;
                if (v & 1) != 0 {
                    // Latch W_RXBUF_WR_ADDR dans W_RXBUF_WRCSR
                    self.rxbuf_wrcsr = self.rxbuf_wr_addr & 0x0FFF;
                }
            }
            0x036 => self.power_us = v & 1,
            0x03C => {
                // Toujours rapporter prêt (bit 9 = 0x0200) dès réveil
                self.powerstate = if (v & 0x0002) != 0 {
                    0x0200
                } else {
                    v | 0x0200
                };
            }
            0x040 => {
                self.powerforce = v;
                if (v & 0x8000) != 0 {
                    self.powerstate = if (v & 1) != 0 { 0x0200 } else { 0x0000 };
                }
            }
            0x050 => self.rxbuf_begin = v,
            0x052 => self.rxbuf_end = v,
            0x056 => self.rxbuf_wr_addr = v & 0x0FFF,
            0x058 => self.rxbuf_rd_addr = v & 0x1FFE,
            0x05A => self.rxbuf_readcsr = v & 0x0FFF,
            0x05C => self.rxbuf_count = v & 0x0FFF,
            0x062 => self.rxbuf_gap = v & 0x1FFE,
            0x064 => self.rxbuf_gapdisp = v & 0x0FFF,
            0x068 => self.txbuf_wr_addr = v & 0x1FFE,
            0x06C => self.txbuf_count = v & 0x0FFF,
            0x070 => self.push_txbuf_wr_data(v),
            0x074 => self.txbuf_gap = v & 0x1FFE,
            0x076 => self.txbuf_gapdisp = v & 0x0FFF,
            0x080 => self.txbuf_beacon = v,
            0x08C => {
                self.beacon_int = v & 0x03FF;
                if self.beacon_int == 0 {
                    self.beacon_int = 100;
                }
                self.beacon_count1 = self.beacon_int;
            }
            0x090 => {
                self.txbuf_cmd = v;
                if (v & 0x8000) != 0 && (self.txreq & 0x0002) != 0 {
                    self.execute_tx_cmd();
                }
            }
            0x094 => self.txbuf_reply1 = v,
            0x098 => self.txbuf_reply2 = v,
            0x0A0 => {
                self.txbuf_loc[0] = v;
                if (v & 0x8000) != 0 && (self.txreq & 0x0001) != 0 {
                    self.execute_tx_loc(0);
                }
            }
            0x0A4 => {
                self.txbuf_loc[1] = v;
                if (v & 0x8000) != 0 && (self.txreq & 0x0004) != 0 {
                    self.execute_tx_loc(1);
                }
            }
            0x0A8 => {
                self.txbuf_loc[2] = v;
                if (v & 0x8000) != 0 && (self.txreq & 0x0008) != 0 {
                    self.execute_tx_loc(2);
                }
            }
            0x0AC => {
                // W_TXREQ_RESET
                self.txreq &= !(v & 0x000F);
            }
            0x0AE => {
                // W_TXREQ_SET : priorité LOC3 > LOC2 > LOC1, + CMD
                self.txreq |= v & 0x000F;
                if (self.txreq & 0x0008) != 0 && (self.txbuf_loc[2] & 0x8000) != 0 {
                    self.execute_tx_loc(2);
                }
                if (self.txreq & 0x0004) != 0 && (self.txbuf_loc[1] & 0x8000) != 0 {
                    self.execute_tx_loc(1);
                }
                if (self.txreq & 0x0001) != 0 && (self.txbuf_loc[0] & 0x8000) != 0 {
                    self.execute_tx_loc(0);
                }
                if (self.txreq & 0x0002) != 0 && (self.txbuf_cmd & 0x8000) != 0 {
                    self.execute_tx_cmd();
                }
            }
            0x0BC => self.preamble = v & 0x0007,
            0x0C0 => self.cmd_totaltime = v,
            0x0C4 => self.cmd_replytime = v,
            0x0D0 => self.rxfilter = v,
            0x0E0 => self.rxfilter2 = v,
            0x0E8 => self.us_count_en = (v & 1) != 0,
            0x0EA => self.us_compare_en = (v & 1) != 0,
            0x0EE => self.cmd_count_en = (v & 1) != 0,
            0x0F0 => self.us_compare = (self.us_compare & !0xFFFF) | u64::from(v),
            0x0F2 => {
                self.us_compare = (self.us_compare & !(0xFFFFu64 << 16)) | (u64::from(v) << 16);
            }
            0x0F4 => {
                self.us_compare = (self.us_compare & !(0xFFFFu64 << 32)) | (u64::from(v) << 32);
            }
            0x0F6 => {
                self.us_compare = (self.us_compare & !(0xFFFFu64 << 48)) | (u64::from(v) << 48);
            }
            0x0F8 => self.us_count = (self.us_count & !0xFFFF) | u64::from(v),
            0x0FA => {
                self.us_count = (self.us_count & !(0xFFFFu64 << 16)) | (u64::from(v) << 16);
            }
            0x0FC => {
                self.us_count = (self.us_count & !(0xFFFFu64 << 32)) | (u64::from(v) << 32);
            }
            0x0FE => {
                self.us_count = (self.us_count & !(0xFFFFu64 << 48)) | (u64::from(v) << 48);
            }
            0x110 => self.beacon_count1 = v,
            0x114 => self.beacon_count2 = v,
            0x118 => self.cmd_count = v,
            0x158 => {
                // W_BB_CNT : accès registres Baseband (direction 5 = écriture, 6 = lecture)
                if self.bb.len() < 0x100 {
                    self.bb.resize(0x100, 0);
                    self.bb[0] = 0x6D;
                }
                let idx = (v & 0xFF) as usize;
                match (v >> 12) & 0xF {
                    5 => {
                        let w_val = (self.io_regs[0x15A >> 1] & 0xFF) as u8;
                        if idx != 0 {
                            self.bb[idx] = w_val;
                        }
                    }
                    6 => self.bb_read = u16::from(self.bb[idx]),
                    _ => {}
                }
            }
            0x17C => {
                // W_RF_DATA2 : mise à jour de canal RF2958 (registre 5/6)
                let rf_data1 = self.io_regs[0x17E >> 1];
                let combined = (u32::from(v) << 16) | u32::from(rf_data1);
                if let Some(ch) = decode_rf_channel(combined) {
                    self.rf_channel = ch;
                }
            }
            0x194 => self.tx_hdr_cnt = v & 0x0007,
            0x19C => self.rf_pins = v,
            _ => {}
        }

        self.irq_pending()
    }

    /// Fait avancer les horloges Wi-Fi (1 MHz `W_US_COUNT`, compteurs Beacon/CMD, et balises
    /// périodiques `ZaimuleAP` en mode WFC). Retourne `true` si `IRQ_WIFI` est active.
    pub fn tick_scanline(&mut self, elapsed_us: u64) -> bool {
        if self.us_count_en {
            let prev = self.us_count;
            self.us_count = self.us_count.wrapping_add(elapsed_us);
            if self.us_compare_en && prev < self.us_compare && self.us_count >= self.us_compare {
                self.raise_irq_bits(WIF_PRE_BEACON | WIF_BEACON_TIMESLOT);
                if (self.txbuf_beacon & 0x8000) != 0 {
                    self.execute_tx_beacon();
                }
                self.raise_irq_bits(WIF_POST_BEACON);
            }
        }

        // Compteur Multi-Play W_CMD_COUNT (pas de 10 µs)
        if self.cmd_count_en && self.cmd_count > 0 {
            self.cmd_10us_accum += elapsed_us;
            let dec = (self.cmd_10us_accum / 10) as u16;
            self.cmd_10us_accum %= 10;
            if dec > 0 {
                self.cmd_count = self.cmd_count.saturating_sub(dec);
            }
        }

        // Compteur Beacon Interval (1 TU = 1024 µs)
        self.beacon_tu_accum += elapsed_us;
        if self.beacon_tu_accum >= 1024 {
            let tus = (self.beacon_tu_accum / 1024) as u16;
            self.beacon_tu_accum %= 1024;
            if self.beacon_count1 > tus {
                self.beacon_count1 -= tus;
            } else {
                self.beacon_count1 = self.beacon_int.max(10);
                if (self.txbuf_beacon & 0x8000) != 0 && self.mode == NdsWifiMode::LocalNiFi {
                    self.execute_tx_beacon();
                }
            }
        }

        // Mode WFC Virtual Access Point : diffuser une balise 802.11b `ZaimuleAP` toutes les ~100 ms
        if self.mode == NdsWifiMode::WfcAccessPoint && (self.rxcnt & 0x8000) != 0 {
            self.ap.beacon_accum_us += elapsed_us;
            if self.ap.beacon_accum_us >= 102_400 {
                self.ap.beacon_accum_us = 0;
                let beacon = self.ap.build_beacon_frame(self.us_count, self.rf_channel);
                let _ = self.inject_rx_80211(0x0000, 0x0A, &beacon);
            }
        }

        self.irq_pending()
    }

    /// Extrait toutes les trames prêtes à être chiffrées `ZMR1` et envoyées sur `RelayChannel::NdsWifi`.
    pub fn drain_outbound_tx(&mut self, out: &mut Vec<NdsWifiPacket>) {
        while let Some(pkt) = self.outbound_tx.pop_front() {
            out.push(pkt);
        }
    }

    /// Extrait les paquets IPv4 WFC destinés à un serveur WFC externe/auto-hébergé (passerelle NAT).
    pub fn drain_external_wfc_ip(&mut self, out: &mut Vec<Vec<u8>>) {
        while let Some(ip) = self.outbound_external_ip.pop_front() {
            out.push(ip);
        }
    }

    /// Injecte un paquet IPv4 reçu de la passerelle NAT externe dans le tampon RX 802.11b de la DS.
    pub fn inject_external_wfc_ip(&mut self, ipv4_packet: &[u8]) -> bool {
        if self.mode != NdsWifiMode::WfcAccessPoint || ipv4_packet.len() < 20 {
            return false;
        }
        let frame =
            self.ap
                .wrap_from_ds_llc_frame(self.mac_addr, VIRTUAL_AP_MAC, 0x0800, ipv4_packet);
        self.inject_rx_80211(0x0008, 0x14, &frame)
    }

    /// Injecte une trame reçue d'un pair distant via le tunnel E2EE `ZMR1` (`RelayChannel::NdsWifi`).
    /// Retourne `true` si `IRQ_WIFI` doit être levée sur l'ARM7.
    pub fn receive_remote_packet(&mut self, packet: &NdsWifiPacket) -> bool {
        if self.mode == NdsWifiMode::Off || packet.frame.is_empty() {
            return false;
        }
        match packet.kind {
            NDS_WIFI_KIND_WFC_IPV4 => {
                if self.mode == NdsWifiMode::WfcAccessPoint && packet.frame.len() >= 20 {
                    let src_mac = derive_console_mac(packet.sender_aid);
                    let frame = self.ap.wrap_from_ds_llc_frame(
                        self.mac_addr,
                        src_mac,
                        0x0800,
                        &packet.frame,
                    );
                    return self.inject_rx_80211(0x0008, 0x14, &frame);
                }
                false
            }
            NDS_WIFI_KIND_MP_CMD => {
                let irq = self.inject_rx_80211(0x000C, packet.rate, &packet.frame);
                // Multi-Play Esclave : si `W_TXBUF_REPLY1` est armé (bit 15), envoyer la réponse auto !
                if (self.txbuf_reply1 & 0x8000) != 0 {
                    self.execute_tx_reply();
                }
                irq || self.irq_pending()
            }
            NDS_WIFI_KIND_MP_REPLY => self.inject_rx_80211(0x000F, packet.rate, &packet.frame),
            NDS_WIFI_KIND_MP_ACK => self.inject_rx_80211(0x000D, packet.rate, &packet.frame),
            _ => {
                let flags = classify_rx_flags(&packet.frame, self.bssid);
                self.inject_rx_80211(flags, packet.rate, &packet.frame)
            }
        }
    }

    // -------------------------------------------------------------------------
    // Implémentation interne RX / TX circulaire & FIFO
    // -------------------------------------------------------------------------

    fn next_random(&mut self) -> u16 {
        let out = self.random_lfsr & 0x07FF;
        let bit0 = self.random_lfsr & 1;
        let rot = ((self.random_lfsr << 1) | ((self.random_lfsr >> 10) & 1)) & 0x07FF;
        self.random_lfsr = (bit0 ^ rot).max(1);
        out
    }

    fn rx_ring_bounds(&self) -> (usize, usize) {
        let begin = usize::from(self.rxbuf_begin & 0x1FFE);
        let mut end = usize::from(self.rxbuf_end & 0x1FFE);
        if end <= begin || end > WIFI_RAM_SIZE {
            end = WIFI_RAM_SIZE;
        }
        (begin, end)
    }

    fn pop_rxbuf_rd_data(&mut self) -> u16 {
        let (begin, end) = self.rx_ring_bounds();
        let addr = usize::from(self.rxbuf_rd_addr & 0x1FFE);
        let val = u16::from_le_bytes([self.wifi_ram[addr], self.wifi_ram[(addr + 1) & 0x1FFF]]);
        let mut next = addr + 2;
        if next == usize::from(self.rxbuf_gap & 0x1FFE) && self.rxbuf_gapdisp > 0 {
            next += usize::from(self.rxbuf_gapdisp) * 2;
        }
        if next >= end {
            next = begin + (next - end) % (end - begin).max(2);
        }
        self.rxbuf_rd_addr = (next & 0x1FFE) as u16;
        self.rxbuf_count = self.rxbuf_count.saturating_sub(1);
        val
    }

    fn push_txbuf_wr_data(&mut self, val: u16) {
        let addr = usize::from(self.txbuf_wr_addr & 0x1FFE);
        self.wifi_ram[addr] = val as u8;
        self.wifi_ram[(addr + 1) & 0x1FFF] = (val >> 8) as u8;
        let mut next = (addr + 2) & 0x1FFE;
        if next == usize::from(self.txbuf_gap & 0x1FFE) && self.txbuf_gapdisp > 0 {
            next = (next + usize::from(self.txbuf_gapdisp) * 2) & 0x1FFE;
        }
        self.txbuf_wr_addr = next as u16;
        self.txbuf_count = self.txbuf_count.saturating_sub(1);
    }

    fn read_wifi_ram_u16(&self, byte_off: usize) -> u16 {
        let idx = byte_off & (WIFI_RAM_SIZE - 2);
        u16::from_le_bytes([self.wifi_ram[idx], self.wifi_ram[idx + 1]])
    }

    fn write_wifi_ram_u16(&mut self, byte_off: usize, val: u16) {
        let idx = byte_off & (WIFI_RAM_SIZE - 2);
        self.wifi_ram[idx] = val as u8;
        self.wifi_ram[idx + 1] = (val >> 8) as u8;
    }

    /// Lit une trame 802.11b + son en-tête matériel TX (12 octets) depuis `wifi_ram[slot_byte_off..]`.
    fn read_tx_slot_frame(&mut self, slot_reg: u16) -> Option<(usize, u8, Vec<u8>)> {
        let base = usize::from(slot_reg & 0x0FFF) * 2;
        let rate = self.wifi_ram[(base + 8) & 0x1FFF];
        let rate = if rate == 0x0A { 0x0A } else { 0x14 };
        let len_with_fcs = usize::from(self.read_wifi_ram_u16(base + 10) & 0x0FFF);
        if len_with_fcs < 10 {
            return None;
        }
        // Le champ longueur TX inclut les 4 octets FCS matériels ; on transmet le MPDU utile
        let mpdu_len = len_with_fcs
            .saturating_sub(4)
            .clamp(10, NDS_WIFI_MAX_FRAME_BYTES);

        // Mise à jour auto de W_TX_SEQNO si autorisé (GBATEK W_TXBUF_LOCn.bit13 & W_TX_HDR_CNT.bit2)
        if (self.tx_hdr_cnt & 0x0004) == 0 && mpdu_len >= 24 {
            let seq_off = (base + WIFI_HW_HEADER_LEN + 22) & 0x1FFE;
            let orig_seq = self.read_wifi_ram_u16(seq_off);
            let new_seq = (orig_seq & 0x000F) | ((self.tx_seqno & 0x0FFF) << 4);
            self.write_wifi_ram_u16(seq_off, new_seq);
            self.tx_seqno = self.tx_seqno.wrapping_add(1) & 0x0FFF;
        }

        let mut frame = Vec::with_capacity(mpdu_len);
        for i in 0..mpdu_len {
            frame.push(self.wifi_ram[(base + WIFI_HW_HEADER_LEN + i) & 0x1FFF]);
        }
        Some((base, rate, frame))
    }

    fn enqueue_outbound(&mut self, pkt: NdsWifiPacket) {
        if self.outbound_tx.len() >= MAX_OUTBOUND_QUEUE {
            let _ = self.outbound_tx.pop_front();
        }
        self.outbound_tx.push_back(pkt);
    }

    fn execute_tx_loc(&mut self, idx: usize) {
        let slot_reg = self.txbuf_loc[idx];
        let Some((base, rate, frame)) = self.read_tx_slot_frame(slot_reg) else {
            self.txbuf_loc[idx] &= 0x7FFF;
            return;
        };

        // Marquer le statut TX matériel comme réussi (TXHDR[0] = 0x0001)
        self.write_wifi_ram_u16(base, 0x0001);
        self.txbuf_loc[idx] &= 0x7FFF;
        let loc_stat_id = (idx as u16) << 12;
        self.txstat = 0x0001 | loc_stat_id;
        self.txbusy = 0;
        self.tx_frames = self.tx_frames.saturating_add(1);
        self.raise_irq_bits(WIF_TX_START | WIF_TX_COMPLETE);

        self.dispatch_tx_frame(NDS_WIFI_KIND_RAW_80211, rate, frame);
    }

    fn execute_tx_beacon(&mut self) {
        let Some((base, rate, frame)) = self.read_tx_slot_frame(self.txbuf_beacon) else {
            return;
        };
        self.write_wifi_ram_u16(base, 0x0001);
        if (self.txstatcnt & 0x8000) != 0 {
            self.txstat = 0x0301;
            self.raise_irq_bits(WIF_TX_COMPLETE);
        }
        self.tx_frames = self.tx_frames.saturating_add(1);
        self.dispatch_tx_frame(NDS_WIFI_KIND_RAW_80211, rate, frame);
    }

    fn execute_tx_cmd(&mut self) {
        let Some((base, rate, frame)) = self.read_tx_slot_frame(self.txbuf_cmd) else {
            self.txbuf_cmd &= 0x7FFF;
            return;
        };
        // Succès CMD + ACK Multi-Play (GBATEK DS Wifi Multiplay Master)
        self.write_wifi_ram_u16(base, 0x0001);
        self.write_wifi_ram_u16(base + 2, 0x0000); // Tous les clients répondus
        self.txbuf_cmd &= 0x7FFF;
        self.txbusy = 0;
        self.tx_frames = self.tx_frames.saturating_add(1);
        self.mp_cycles = self.mp_cycles.saturating_add(1);

        let mut irqs = WIF_TX_START | WIF_MP_CMD_DONE;
        if (self.txstatcnt & 0x4000) != 0 {
            self.txstat = 0x0800;
            irqs |= WIF_TX_COMPLETE;
        }
        if (self.txstatcnt & 0x2000) != 0 {
            self.txstat = 0x0B01;
            irqs |= WIF_TX_COMPLETE;
        }
        self.raise_irq_bits(irqs);

        self.dispatch_tx_frame(NDS_WIFI_KIND_MP_CMD, rate, frame);

        // Diffuser l'ACK MP automatique (`03:09:BF:00:00:03`) aux esclaves
        if self.mode == NdsWifiMode::LocalNiFi {
            let mut ack = vec![0u8; 28];
            ack[0..2].copy_from_slice(&0x0218u16.to_le_bytes()); // Data+CF-Ack FromDS
            ack[4..10].copy_from_slice(&NINTENDO_MP_ACK_MAC);
            ack[10..16].copy_from_slice(&self.mac_addr);
            ack[16..22].copy_from_slice(&self.mac_addr);
            self.dispatch_tx_frame(NDS_WIFI_KIND_MP_ACK, 0x14, ack);
        }
    }

    fn execute_tx_reply(&mut self) {
        let Some((base, rate, frame)) = self.read_tx_slot_frame(self.txbuf_reply1) else {
            self.txbuf_reply1 &= 0x7FFF;
            return;
        };
        self.write_wifi_ram_u16(base, 0x0001);
        self.txbuf_reply1 = self.txbuf_reply2;
        self.txbuf_reply2 = 0;
        self.tx_frames = self.tx_frames.saturating_add(1);
        self.mp_cycles = self.mp_cycles.saturating_add(1);
        if (self.txstatcnt & 0x1000) != 0 {
            self.txstat = 0x0401;
            self.raise_irq_bits(WIF_TX_COMPLETE);
        }
        self.dispatch_tx_frame(NDS_WIFI_KIND_MP_REPLY, rate, frame);
    }

    fn dispatch_tx_frame(&mut self, kind: u8, rate: u8, frame: Vec<u8>) {
        match self.mode {
            NdsWifiMode::Off => {}
            NdsWifiMode::LocalNiFi => {
                self.seq_counter = self.seq_counter.wrapping_add(1);
                self.enqueue_outbound(NdsWifiPacket {
                    kind,
                    channel: self.rf_channel,
                    rate,
                    sender_aid: (self.aid_low & 0x0F) as u8,
                    seq: self.seq_counter,
                    frame,
                });
            }
            NdsWifiMode::WfcAccessPoint => {
                let (replies, forwarded) =
                    self.ap
                        .handle_station_tx(&frame, self.us_count, self.rf_channel);
                for rx_frame in replies {
                    let flags = classify_rx_flags(&rx_frame, VIRTUAL_AP_MAC);
                    let _ = self.inject_rx_80211(flags, 0x14, &rx_frame);
                }
                if let Some(fwd) = forwarded {
                    match fwd {
                        WfcForwardedIp::PeerLan(ip_bytes) => {
                            self.seq_counter = self.seq_counter.wrapping_add(1);
                            self.enqueue_outbound(NdsWifiPacket {
                                kind: NDS_WIFI_KIND_WFC_IPV4,
                                channel: self.rf_channel,
                                rate: 0x14,
                                sender_aid: self.peer_id,
                                seq: self.seq_counter,
                                frame: ip_bytes,
                            });
                        }
                        WfcForwardedIp::External(ip_bytes) => {
                            if self.outbound_external_ip.len() >= MAX_OUTBOUND_QUEUE {
                                let _ = self.outbound_external_ip.pop_front();
                            }
                            self.outbound_external_ip.push_back(ip_bytes);
                        }
                    }
                }
            }
        }
    }

    /// Écrit une trame 802.11b + son en-tête RX matériel (12 octets) dans le tampon circulaire
    /// `wifi_ram[W_RXBUF_BEGIN..W_RXBUF_END]`, met à jour `W_RXBUF_WRCSR` / `W_RXBUF_COUNT` et lève
    /// `W_IF |= RX_START | RX_COMPLETE`.
    pub fn inject_rx_80211(&mut self, flags: u16, rate: u8, frame: &[u8]) -> bool {
        if frame.is_empty() || frame.len() > NDS_WIFI_MAX_FRAME_BYTES {
            return false;
        }
        if (self.rxcnt & 0x8000) == 0 {
            return false;
        }
        // Filtrage d'adresse matérielle minimal : accepter broadcast, MP Nintendo, MAC locale ou mode promiscuous
        if frame.len() >= 10 {
            let dst = &frame[4..10];
            let is_broadcast = dst == [0xFF; 6];
            let is_nintendo_mp = dst[0..5] == NINTENDO_MP_CMD_MAC[0..5];
            let is_our_mac = dst == self.mac_addr;
            let is_beacon = (frame[0] & 0xFC) == 0x80;
            let promisc = (self.rxfilter & 0x0080) != 0;
            if !(is_broadcast || is_nintendo_mp || is_our_mac || is_beacon || promisc) {
                return false;
            }
        }

        let (begin, end) = self.rx_ring_bounds();
        let ring_len = end.saturating_sub(begin);
        let padded_frame_len = (frame.len() + 1) & !1;
        let total_bytes = WIFI_HW_HEADER_LEN + padded_frame_len;
        if ring_len < total_bytes + 4 {
            self.raise_irq_bits(WIF_RX_OVERFLOW);
            return self.irq_pending();
        }

        let mut hdr = [0u8; WIFI_HW_HEADER_LEN];
        hdr[0..2].copy_from_slice(&flags.to_le_bytes());
        hdr[4..6].copy_from_slice(&0x0040u16.to_le_bytes());
        hdr[6..8].copy_from_slice(&u16::from(rate).to_le_bytes());
        hdr[8..10].copy_from_slice(&(frame.len() as u16).to_le_bytes());
        hdr[10..12].copy_from_slice(&0x00D0u16.to_le_bytes()); // RSSI fort (3 barres vertes)

        let mut cursor = usize::from(self.rxbuf_wrcsr) * 2;
        if cursor < begin || cursor >= end {
            cursor = begin;
        }
        let write_byte = |ram: &mut [u8], pos: &mut usize, b: u8| {
            ram[*pos & 0x1FFF] = b;
            *pos += 1;
            if *pos >= end {
                *pos = begin;
            }
        };
        for b in hdr {
            write_byte(&mut self.wifi_ram, &mut cursor, b);
        }
        for &b in frame {
            write_byte(&mut self.wifi_ram, &mut cursor, b);
        }
        if frame.len() & 1 != 0 {
            write_byte(&mut self.wifi_ram, &mut cursor, 0x00);
        }
        // Alignement pair garanti pour W_RXBUF_WRCSR (en demi-mots)
        if cursor & 1 != 0 {
            write_byte(&mut self.wifi_ram, &mut cursor, 0x00);
        }

        self.rxbuf_wrcsr = ((cursor >> 1) & 0x0FFF) as u16;
        let added_halfwords = (total_bytes / 2) as u16;
        self.rxbuf_count = self.rxbuf_count.saturating_add(added_halfwords).min(0x0FFF);
        self.rx_frames = self.rx_frames.saturating_add(1);
        self.raise_irq_bits(WIF_RX_START | WIF_RX_COMPLETE | WIF_RX_EVENT)
    }
}

fn classify_rx_flags(frame: &[u8], active_bssid: [u8; 6]) -> u16 {
    if frame.len() < 2 {
        return 0x0000;
    }
    let ftype = (frame[0] >> 2) & 0x03;
    let mut flags: u16 = match ftype {
        0 => 0x0000, // Management
        1 => 0x0001, // Control
        2 => 0x0008, // Data
        _ => 0x0000,
    };
    if frame.len() >= 22 && active_bssid != [0; 6] && frame[16..22] == active_bssid {
        flags |= 0x8000; // Matches BSSID
    }
    flags
}

/// Décode un numéro de canal 802.11b (`1..=13`) depuis un mot série RF2958 (registre 5).
fn decode_rf_channel(rf_word: u32) -> Option<u8> {
    let reg_idx = (rf_word >> 18) & 0x1F;
    if reg_idx != 5 {
        return None;
    }
    let freq_code = (rf_word & 0x3FFFF) as u16;
    // Table RF2958 standard des canaux 1..=13 (2412..2472 MHz)
    let ch = match freq_code {
        0x0000..=0x03FF => ((freq_code as u8) % 13) + 1,
        _ => return None,
    };
    Some(ch.clamp(1, 13))
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

fn transport_checksum(src: [u8; 4], dst: [u8; 4], proto: u8, segment: &[u8]) -> u16 {
    let mut sum = 0u32;
    sum += u32::from(u16::from_be_bytes([src[0], src[1]]));
    sum += u32::from(u16::from_be_bytes([src[2], src[3]]));
    sum += u32::from(u16::from_be_bytes([dst[0], dst[1]]));
    sum += u32::from(u16::from_be_bytes([dst[2], dst[3]]));
    sum += u32::from(proto);
    sum += segment.len() as u32;
    for &pair in segment.as_chunks::<2>().0 {
        sum += u32::from(u16::from_be_bytes(pair));
    }
    if segment.len() & 1 != 0 {
        sum += u32::from(segment[segment.len() - 1]) << 8;
    }
    while (sum >> 16) != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

fn build_udp_segment(
    src_port: u16,
    dst_port: u16,
    src_ip: [u8; 4],
    dst_ip: [u8; 4],
    payload: &[u8],
) -> Vec<u8> {
    let len = (8 + payload.len()) as u16;
    let mut seg = Vec::with_capacity(usize::from(len));
    seg.extend_from_slice(&src_port.to_be_bytes());
    seg.extend_from_slice(&dst_port.to_be_bytes());
    seg.extend_from_slice(&len.to_be_bytes());
    seg.extend_from_slice(&[0x00, 0x00]);
    seg.extend_from_slice(payload);
    let chk = transport_checksum(src_ip, dst_ip, 17, &seg);
    let chk = if chk == 0 { 0xFFFF } else { chk };
    seg[6..8].copy_from_slice(&chk.to_be_bytes());
    seg
}

#[allow(clippy::too_many_arguments)]
fn build_tcp_segment(
    src_port: u16,
    dst_port: u16,
    src_ip: [u8; 4],
    dst_ip: [u8; 4],
    seq: u32,
    ack: u32,
    flags: u8,
    payload: &[u8],
) -> Vec<u8> {
    let mut seg = Vec::with_capacity(20 + payload.len());
    seg.extend_from_slice(&src_port.to_be_bytes());
    seg.extend_from_slice(&dst_port.to_be_bytes());
    seg.extend_from_slice(&seq.to_be_bytes());
    seg.extend_from_slice(&ack.to_be_bytes());
    seg.push(0x50); // Data offset = 5 (20 bytes)
    seg.push(flags);
    seg.extend_from_slice(&4096u16.to_be_bytes()); // Window
    seg.extend_from_slice(&[0x00, 0x00]); // Checksum placeholder
    seg.extend_from_slice(&[0x00, 0x00]); // Urgent pointer
    seg.extend_from_slice(payload);
    let chk = transport_checksum(src_ip, dst_ip, 6, &seg);
    seg[16..18].copy_from_slice(&chk.to_be_bytes());
    seg
}

fn find_dns_qname_end(pkt: &[u8], start: usize) -> Option<usize> {
    let mut i = start;
    while i < pkt.len() {
        let len = usize::from(pkt[i]);
        if len == 0 {
            return Some(i + 1);
        }
        if (len & 0xC0) != 0 {
            return Some(i + 2);
        }
        i = i.checked_add(1 + len)?;
    }
    None
}

fn decode_dns_name(pkt: &[u8], start: usize) -> String {
    let mut out = String::new();
    let mut i = start;
    while i < pkt.len() {
        let len = usize::from(pkt[i]);
        if len == 0 || (len & 0xC0) != 0 {
            break;
        }
        if i + 1 + len > pkt.len() {
            break;
        }
        if !out.is_empty() {
            out.push('.');
        }
        out.push_str(&String::from_utf8_lossy(&pkt[i + 1..i + 1 + len]));
        i += 1 + len;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_tx_frame_at(ctrl: &mut NdsWifiController, slot_hw: u16, frame: &[u8]) {
        let base = usize::from(slot_hw & 0x0FFF) * 2;
        // 12-byte TX hardware header
        for i in 0..12 {
            ctrl.wifi_ram[(base + i) & 0x1FFF] = 0;
        }
        ctrl.wifi_ram[(base + 8) & 0x1FFF] = 0x14; // 2 Mbps
        let len_with_fcs = (frame.len() + 4) as u16;
        ctrl.write_wifi_ram_u16(base + 10, len_with_fcs);
        for (i, &b) in frame.iter().enumerate() {
            ctrl.wifi_ram[(base + 12 + i) & 0x1FFF] = b;
        }
    }

    fn read_rx_frame_from_fifo(ctrl: &mut NdsWifiController) -> (u16, Vec<u8>) {
        // Lire en-tête RX 12 octets via W_RXBUF_RD_DATA (0x0480_8060)
        let flags = ctrl.read16(0x0480_8060);
        let _w1 = ctrl.read16(0x0480_8060);
        let _w2 = ctrl.read16(0x0480_8060);
        let _rate = ctrl.read16(0x0480_8060);
        let frame_len = usize::from(ctrl.read16(0x0480_8060));
        let _rssi = ctrl.read16(0x0480_8060);
        let mut frame = Vec::with_capacity(frame_len);
        let halfwords = frame_len.div_ceil(2);
        for _ in 0..halfwords {
            let w = ctrl.read16(0x0480_8060);
            frame.push(w as u8);
            if frame.len() < frame_len {
                frame.push((w >> 8) as u8);
            }
        }
        (flags, frame)
    }

    #[test]
    fn nifi_multiplay_cmd_and_slave_auto_reply_roundtrip_with_irqs() {
        let mut host = NdsWifiController::new();
        let mut guest = NdsWifiController::new();
        host.configure(NdsWifiMode::LocalNiFi, 1, None);
        guest.configure(NdsWifiMode::LocalNiFi, 2, None);

        // Configurer RX circulaire (0x4000..0x4800 -> offsets 0x0000..0x0800) + IE sur les deux DS
        for c in [&mut host, &mut guest] {
            c.write16(0x0480_8050, 0x4000); // W_RXBUF_BEGIN
            c.write16(0x0480_8052, 0x4800); // W_RXBUF_END
            c.write16(0x0480_8056, 0x0000); // W_RXBUF_WR_ADDR
            c.write16(0x0480_8030, 0x8001); // W_RXCNT: enable RX + latch WRCSR
            c.write16(0x0480_8058, 0x0000); // W_RXBUF_RD_ADDR
            c.write16(
                0x0480_8012,
                WIF_RX_COMPLETE | WIF_TX_COMPLETE | WIF_MP_CMD_DONE,
            );
            c.write16(0x0480_8008, 0xF000); // W_TXSTATCNT all enabled
        }

        // L'invité arme sa réponse Multi-Play dans W_TXBUF_REPLY1 (offset 0x0A00 -> hw 0x0500)
        let mut reply_frame = vec![0u8; 32];
        reply_frame[0..2].copy_from_slice(&0x0118u16.to_le_bytes());
        reply_frame[4..10].copy_from_slice(&[0x03, 0x09, 0xBF, 0x00, 0x00, 0x01]);
        reply_frame[24..28].copy_from_slice(b"P2OK");
        write_tx_frame_at(&mut guest, 0x0500, &reply_frame);
        guest.write16(0x0480_8094, 0x8500); // W_TXBUF_REPLY1 armé !

        // L'hôte prépare un paquet MP CMD (offset 0x0A00 -> hw 0x0500) et déclenche W_TXBUF_CMD
        let mut cmd_frame = vec![0u8; 32];
        cmd_frame[0..2].copy_from_slice(&0x0228u16.to_le_bytes());
        cmd_frame[4..10].copy_from_slice(&NINTENDO_MP_CMD_MAC);
        cmd_frame[24..28].copy_from_slice(b"MKDS");
        write_tx_frame_at(&mut host, 0x0500, &cmd_frame);
        host.write16(0x0480_80AE, 0x0002); // W_TXREQ_SET bit 1 (CMD)
        let host_irq = host.write16(0x0480_8090, 0x8500); // W_TXBUF_CMD start
        assert!(host_irq);
        assert_ne!(host.read16(0x0480_8010) & WIF_MP_CMD_DONE, 0);

        // Drainer CMD + ACK de l'hôte et livrer à l'invité
        let mut host_tx = Vec::new();
        host.drain_outbound_tx(&mut host_tx);
        assert_eq!(host_tx.len(), 2);
        assert_eq!(host_tx[0].kind, NDS_WIFI_KIND_MP_CMD);
        assert_eq!(host_tx[1].kind, NDS_WIFI_KIND_MP_ACK);

        assert!(guest.receive_remote_packet(&host_tx[0]));
        assert_ne!(guest.read16(0x0480_8010) & WIF_RX_COMPLETE, 0);

        // L'invité doit lire la trame CMD via W_RXBUF_RD_DATA et avoir auto-émis son MP_REPLY !
        let (rx_flags, rx_cmd) = read_rx_frame_from_fifo(&mut guest);
        assert_eq!(rx_flags & 0x0F, 0x0C);
        assert_eq!(&rx_cmd[24..28], b"MKDS");

        let mut guest_tx = Vec::new();
        guest.drain_outbound_tx(&mut guest_tx);
        assert_eq!(guest_tx.len(), 1);
        assert_eq!(guest_tx[0].kind, NDS_WIFI_KIND_MP_REPLY);

        // Livrer la réponse de l'invité à l'hôte
        assert!(host.receive_remote_packet(&guest_tx[0]));
        let (host_rx_flags, host_rx_reply) = read_rx_frame_from_fifo(&mut host);
        assert_eq!(host_rx_flags & 0x0F, 0x0F);
        assert_eq!(&host_rx_reply[24..28], b"P2OK");
    }

    #[test]
    fn wfc_virtual_ap_completes_assoc_dhcp_dns_and_nintendo_conntest() {
        let mut ds = NdsWifiController::new();
        ds.configure(NdsWifiMode::WfcAccessPoint, 1, None);
        ds.write16(0x0480_8050, 0x4000);
        ds.write16(0x0480_8052, 0x5000);
        ds.write16(0x0480_8056, 0x0000);
        ds.write16(0x0480_8030, 0x8001);
        ds.write16(0x0480_8058, 0x0000);
        ds.write16(0x0480_8012, WIF_RX_COMPLETE | WIF_TX_COMPLETE);
        ds.write16(0x0480_80AE, 0x0001); // Enable LOC1

        // 1. Association 802.11b auprès de ZaimuleAP
        let mut assoc_req = vec![0u8; 32];
        assoc_req[0..2].copy_from_slice(&0x0000u16.to_le_bytes()); // Assoc Request
        assoc_req[4..10].copy_from_slice(&VIRTUAL_AP_MAC);
        assoc_req[10..16].copy_from_slice(&ds.mac_addr);
        assoc_req[16..22].copy_from_slice(&VIRTUAL_AP_MAC);
        write_tx_frame_at(&mut ds, 0x0A00, &assoc_req);
        ds.write16(0x0480_80A0, 0x8A00); // LOC1
        assert!(ds.telemetry().ap_associated);
        let (_fl, assoc_resp) = read_rx_frame_from_fifo(&mut ds);
        assert_eq!(u16::from_le_bytes([assoc_resp[0], assoc_resp[1]]), 0x0010);

        // 2. DHCP Request -> assigne 10.64.0.11
        let mut bootp = vec![0u8; 244];
        bootp[0] = 1; // BOOTREQUEST
        bootp[4..8].copy_from_slice(&0x1122_3344u32.to_be_bytes());
        bootp[28..34].copy_from_slice(&ds.mac_addr);
        bootp[236..240].copy_from_slice(&[99, 130, 83, 99]);
        bootp.extend_from_slice(&[53, 1, 3, 255]); // DHCPREQUEST
        let udp = build_udp_segment(68, 67, [0; 4], [255; 4], &bootp);
        let ipv4 = ds.ap.build_ipv4_packet([0; 4], [255; 4], 17, &udp);
        let mut data_frame = Vec::new();
        data_frame.extend_from_slice(&0x0108u16.to_le_bytes()); // Data ToDS
        data_frame.extend_from_slice(&0u16.to_le_bytes());
        data_frame.extend_from_slice(&VIRTUAL_AP_MAC);
        data_frame.extend_from_slice(&ds.mac_addr);
        data_frame.extend_from_slice(&[0xFF; 6]);
        data_frame.extend_from_slice(&0u16.to_le_bytes());
        data_frame.extend_from_slice(&[0xAA, 0xAA, 0x03, 0x00, 0x00, 0x00, 0x08, 0x00]);
        data_frame.extend_from_slice(&ipv4);
        write_tx_frame_at(&mut ds, 0x0A00, &data_frame);
        ds.write16(0x0480_80A0, 0x8A00);
        assert_eq!(ds.telemetry().assigned_ip.as_deref(), Some("10.64.0.11"));
        let (_fl, dhcp_rx) = read_rx_frame_from_fifo(&mut ds);
        assert!(dhcp_rx.len() > 260);

        // 3. Requête DNS pour conntest.nintendowifi.net -> répond 10.64.0.1
        let mut dns_q = vec![
            0xAB, 0xCD, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        for label in ["conntest", "nintendowifi", "net"] {
            dns_q.push(label.len() as u8);
            dns_q.extend_from_slice(label.as_bytes());
        }
        dns_q.extend_from_slice(&[0x00, 0x00, 0x01, 0x00, 0x01]);
        let dns_udp = build_udp_segment(12345, 53, [10, 64, 0, 11], VIRTUAL_AP_IP, &dns_q);
        let dns_ip = ds
            .ap
            .build_ipv4_packet([10, 64, 0, 11], VIRTUAL_AP_IP, 17, &dns_udp);
        let mut dns_80211 = data_frame[..32].to_vec();
        dns_80211.extend_from_slice(&dns_ip);
        write_tx_frame_at(&mut ds, 0x0A00, &dns_80211);
        ds.write16(0x0480_80A0, 0x8A00);
        let (_fl, dns_rx) = read_rx_frame_from_fifo(&mut ds);
        assert_eq!(&dns_rx[dns_rx.len() - 4..], &VIRTUAL_AP_IP);

        // 4. Test HTTP conntest.nintendowifi.net:80 -> HTTP/1.1 200 OK X-Organization: Nintendo
        let http_req = b"GET / HTTP/1.1\r\nHost: conntest.nintendowifi.net\r\n\r\n";
        let tcp_get = build_tcp_segment(
            40000,
            80,
            [10, 64, 0, 11],
            VIRTUAL_AP_IP,
            100,
            1,
            0x18,
            http_req,
        );
        let tcp_ip = ds
            .ap
            .build_ipv4_packet([10, 64, 0, 11], VIRTUAL_AP_IP, 6, &tcp_get);
        let mut http_80211 = data_frame[..32].to_vec();
        http_80211.extend_from_slice(&tcp_ip);
        write_tx_frame_at(&mut ds, 0x0A00, &http_80211);
        ds.write16(0x0480_80A0, 0x8A00);
        let (_fl, http_rx) = read_rx_frame_from_fifo(&mut ds);
        let http_str = String::from_utf8_lossy(&http_rx);
        assert!(http_str.contains("200 OK"));
        assert!(http_str.contains("X-Organization: Nintendo"));
    }

    #[test]
    fn firmware_patcher_updates_unique_mac_and_wfc_slot_with_valid_crcs() {
        let mut fw = vec![0u8; 0x4_0000];
        fw[0x2C..0x2E].copy_from_slice(&0x0138u16.to_le_bytes());
        let mac = derive_console_mac(3);
        patch_firmware_for_wifi(
            &mut fw,
            mac,
            NdsWifiMode::WfcAccessPoint,
            Some([167, 235, 229, 36]),
            false,
        );
        assert_eq!(&fw[0x36..0x3C], &mac);
        let cal_crc = u16::from_le_bytes([fw[0x2A], fw[0x2B]]);
        assert_eq!(cal_crc, crc16_nds_wifi(&fw[0x2C..0x2C + 0x138]));

        let slot = &fw[0x3FA00..0x3FB00];
        assert_eq!(
            &slot[0x40..0x40 + VIRTUAL_AP_SSID.len()],
            VIRTUAL_AP_SSID.as_bytes()
        );
        assert_eq!(slot[0xE7], 0x00);
        assert_eq!(&slot[0xC8..0xCC], &[167, 235, 229, 36]);
        let ap_crc = u16::from_le_bytes([slot[0xFE], slot[0xFF]]);
        assert_eq!(ap_crc, crc16_nds_wifi(&slot[..0xFE]));
    }
}
