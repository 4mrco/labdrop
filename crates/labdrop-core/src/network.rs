use serde::{Deserialize, Serialize};
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Represents a discovered IPv4 network interface on the transmitting computer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkInterface {
    pub name: String,
    pub ipv4: Ipv4Addr,
    pub prefix_len: u8,
    pub subnet_cidr: String,
    pub is_loopback: bool,
    pub is_wireless: bool,
    pub is_virtual: bool,
    pub is_default_route: bool,
}

impl NetworkInterface {
    pub fn display_label(&self) -> String {
        let kind = if self.is_loopback {
            "Loopback"
        } else if self.is_wireless {
            "Wi-Fi / WLAN"
        } else if self.is_virtual {
            "Virtual / Bridge"
        } else {
            "Ethernet / LAN"
        };
        let def = if self.is_default_route {
            " [Default LAN]"
        } else {
            ""
        };
        format!(
            "{} — {}/{} ({}){}",
            self.name, self.ipv4, self.prefix_len, kind, def
        )
    }
}

/// Computes the network CIDR string (e.g. `192.168.18.0/24`) from an IPv4 address and prefix length.
pub fn compute_subnet_cidr(ip: Ipv4Addr, prefix_len: u8) -> String {
    let clamped = prefix_len.min(32);
    let mask: u32 = if clamped == 0 {
        0
    } else {
        !0u32 << (32 - clamped)
    };
    let net_u32 = u32::from(ip) & mask;
    let net_ip = Ipv4Addr::from(net_u32);
    format!("{net_ip}/{clamped}")
}

/// Probes the default outbound IPv4 address via a non-transmitting UDP connect.
pub fn probe_default_ipv4() -> Option<Ipv4Addr> {
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("192.0.2.1:80").ok()?;
    match sock.local_addr().ok()? {
        std::net::SocketAddr::V4(v4) => Some(*v4.ip()),
        std::net::SocketAddr::V6(_) => None,
    }
}

/// Discovers all IPv4 network interfaces on the system, ordered with preferred laboratory LAN interfaces first.
pub fn discover_network_interfaces() -> Vec<NetworkInterface> {
    let default_ip = probe_default_ipv4();
    let mut interfaces = Vec::new();

    #[cfg(unix)]
    {
        if let Some(mut parsed) = discover_linux_ip_json(default_ip) {
            interfaces.append(&mut parsed);
        } else if let Some(mut parsed) = discover_linux_ip_text(default_ip) {
            interfaces.append(&mut parsed);
        }
    }

    #[cfg(windows)]
    {
        if let Some(mut parsed) = discover_windows_interfaces(default_ip) {
            interfaces.append(&mut parsed);
        }
    }

    // Ensure default_ip is present even if `ip` or PowerShell wasn't available (e.g. minimal container or Wine)
    if let Some(def_ip) = default_ip {
        if !interfaces.iter().any(|i| i.ipv4 == def_ip) {
            interfaces.push(NetworkInterface {
                name: "eth0".to_string(),
                ipv4: def_ip,
                prefix_len: 24,
                subnet_cidr: compute_subnet_cidr(def_ip, 24),
                is_loopback: def_ip.is_loopback(),
                is_wireless: false,
                is_virtual: false,
                is_default_route: true,
            });
        }
    }

    // Always ensure loopback is available as last resort / local testing interface
    if !interfaces.iter().any(|i| i.is_loopback) {
        let lo = Ipv4Addr::new(127, 0, 0, 1);
        interfaces.push(NetworkInterface {
            name: "lo".to_string(),
            ipv4: lo,
            prefix_len: 8,
            subnet_cidr: "127.0.0.0/8".to_string(),
            is_loopback: true,
            is_wireless: false,
            is_virtual: false,
            is_default_route: false,
        });
    }

    // Sort: Default route first, then physical Ethernet, then Wi-Fi, then virtual, then loopback
    interfaces.sort_by_key(|iface| {
        let priority = if iface.is_loopback {
            4
        } else if iface.is_default_route {
            0
        } else if !iface.is_wireless && !iface.is_virtual {
            1
        } else if iface.is_wireless {
            2
        } else {
            3
        };
        (priority, iface.name.clone())
    });

    interfaces
}

fn classify_interface_name(name: &str) -> (bool, bool) {
    let lower = name.to_lowercase();
    let is_wireless = lower.starts_with("wl")
        || lower.contains("wifi")
        || lower.contains("wi-fi")
        || lower.contains("wlan");
    let is_virtual = lower.starts_with("docker")
        || lower.starts_with("podman")
        || lower.starts_with("veth")
        || lower.starts_with("virbr")
        || lower.starts_with("cni")
        || lower.starts_with("tun")
        || lower.starts_with("tap")
        || lower.contains("vethernet")
        || lower.contains("WSL");
    (is_wireless, is_virtual)
}

#[cfg(unix)]
fn discover_linux_ip_json(default_ip: Option<Ipv4Addr>) -> Option<Vec<NetworkInterface>> {
    #[derive(Deserialize)]
    struct IpAddrEntry {
        ifname: String,
        #[serde(default)]
        addr_info: Vec<AddrInfo>,
    }

    #[derive(Deserialize)]
    struct AddrInfo {
        family: Option<String>,
        local: Option<String>,
        prefixlen: Option<u8>,
    }

    let output = Command::new("ip")
        .args(["-j", "-4", "addr", "show"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let entries: Vec<IpAddrEntry> = serde_json::from_slice(&output.stdout).ok()?;
    let mut list = Vec::new();
    for entry in entries {
        for info in entry.addr_info {
            if info.family.as_deref() != Some("inet") {
                continue;
            }
            let Some(local_str) = info.local else {
                continue;
            };
            let Ok(ipv4) = local_str.parse::<Ipv4Addr>() else {
                continue;
            };
            let prefix_len = info.prefixlen.unwrap_or(24);
            let is_loopback = ipv4.is_loopback() || entry.ifname == "lo";
            let (is_wireless, is_virtual) = classify_interface_name(&entry.ifname);
            list.push(NetworkInterface {
                name: entry.ifname.clone(),
                ipv4,
                prefix_len,
                subnet_cidr: compute_subnet_cidr(ipv4, prefix_len),
                is_loopback,
                is_wireless,
                is_virtual,
                is_default_route: default_ip == Some(ipv4),
            });
        }
    }
    if list.is_empty() {
        None
    } else {
        Some(list)
    }
}

#[cfg(unix)]
fn discover_linux_ip_text(default_ip: Option<Ipv4Addr>) -> Option<Vec<NetworkInterface>> {
    let output = Command::new("ip")
        .args(["-o", "-4", "addr", "show"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut list = Vec::new();
    for line in text.lines() {
        // Format: 2: wlp2s0    inet 192.168.18.171/24 brd ...
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 4 {
            continue;
        }
        let ifname = parts[1].trim_end_matches(':').to_string();
        if let Some(inet_pos) = parts.iter().position(|&p| p == "inet") {
            if let Some(&cidr) = parts.get(inet_pos + 1) {
                let mut sp = cidr.split('/');
                if let Some(ip_str) = sp.next() {
                    if let Ok(ipv4) = ip_str.parse::<Ipv4Addr>() {
                        let prefix_len = sp
                            .next()
                            .and_then(|p| p.parse::<u8>().ok())
                            .unwrap_or(24);
                        let is_loopback = ipv4.is_loopback() || ifname == "lo";
                        let (is_wireless, is_virtual) = classify_interface_name(&ifname);
                        list.push(NetworkInterface {
                            name: ifname,
                            ipv4,
                            prefix_len,
                            subnet_cidr: compute_subnet_cidr(ipv4, prefix_len),
                            is_loopback,
                            is_wireless,
                            is_virtual,
                            is_default_route: default_ip == Some(ipv4),
                        });
                    }
                }
            }
        }
    }
    if list.is_empty() {
        None
    } else {
        Some(list)
    }
}

#[cfg(windows)]
fn discover_windows_interfaces(default_ip: Option<Ipv4Addr>) -> Option<Vec<NetworkInterface>> {
    let ps_cmd = "Get-NetIPAddress -AddressFamily IPv4 | Where-Object { $_.IPAddress -notlike '169.254.*' } | Select-Object InterfaceAlias,IPAddress,PrefixLength | ConvertTo-Json -Compress";
    let output = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", ps_cmd])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct WinIpEntry {
        interface_alias: String,
        #[serde(rename = "IPAddress")]
        ip_address: String,
        prefix_length: Option<u8>,
    }

    let val: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let entries: Vec<WinIpEntry> = if val.is_array() {
        serde_json::from_value(val).ok()?
    } else {
        vec![serde_json::from_value(val).ok()?]
    };

    let mut list = Vec::new();
    for entry in entries {
        if let Ok(ipv4) = entry.ip_address.parse::<Ipv4Addr>() {
            let prefix_len = entry.prefix_length.unwrap_or(24);
            let is_loopback = ipv4.is_loopback();
            let (is_wireless, is_virtual) = classify_interface_name(&entry.interface_alias);
            list.push(NetworkInterface {
                name: entry.interface_alias,
                ipv4,
                prefix_len,
                subnet_cidr: compute_subnet_cidr(ipv4, prefix_len),
                is_loopback,
                is_wireless,
                is_virtual,
                is_default_route: default_ip == Some(ipv4),
            });
        }
    }
    if list.is_empty() {
        None
    } else {
        Some(list)
    }
}

/// Discovers the local computer's hostname and normalized `.local` mDNS name.
pub fn get_local_hostname() -> (String, String) {
    let raw = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .or_else(|| {
            Command::new("hostname")
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "labdrop-host".to_string());

    let short = raw
        .split('.')
        .next()
        .unwrap_or("labdrop-host")
        .trim()
        .to_string();
    let mdns = format!("{}.local", short.to_lowercase());
    (short, mdns)
}

/// Manages optional mDNS / Avahi advertisement during an active sharing session.
pub struct MdnsAdvertiser {
    stop_flag: Arc<AtomicBool>,
    responder_thread: Option<JoinHandle<()>>,
    avahi_child: Option<Child>,
    pub advertised_hostname: String,
    pub active_method: String,
}

impl MdnsAdvertiser {
    /// Starts best-effort mDNS advertisement for `interface_ip` and `service_port`.
    pub fn start(interface_ip: Ipv4Addr, service_port: u16, service_type: &str) -> Self {
        let (_short_host, mdns_host) = get_local_hostname();
        let stop_flag = Arc::new(AtomicBool::new(false));

        // 1. Try spawning avahi-publish on Linux if avahi-daemon is active
        #[cfg(unix)]
        let avahi_child = {
            let mut child_opt = None;
            if Command::new("avahi-publish")
                .arg("--version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
            {
                let svc_name = format!("LabDrop ({_short_host})");
                if let Ok(child) = Command::new("avahi-publish")
                    .args([
                        "-s",
                        &svc_name,
                        service_type,
                        &service_port.to_string(),
                        "path=/",
                    ])
                    .current_dir(std::env::temp_dir())
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                {
                    child_opt = Some(child);
                }
            }
            child_opt
        };
        #[cfg(not(unix))]
        let avahi_child: Option<Child> = {
            let _ = (service_port, service_type);
            None
        };

        // 2. Also start lightweight pure-Rust UDP mDNS responder for `<hostname>.local` and `labdrop.local`
        let flag_clone = Arc::clone(&stop_flag);
        let mdns_clone = mdns_host.clone();
        let responder_thread = spawn_udp_mdns_responder(interface_ip, mdns_clone, flag_clone);

        let active_method = match (&avahi_child, &responder_thread) {
            (Some(_), Some(_)) => "Avahi + UDP mDNS (.local best-effort; use IP directly if blocked)".to_string(),
            (Some(_), None) => "Avahi mDNS (.local best-effort; use IP directly if blocked)".to_string(),
            (None, Some(_)) => "Embedded UDP mDNS (.local best-effort; use IP directly if blocked)".to_string(),
            (None, None) => "Direct IP only (mDNS port 5353 unavailable)".to_string(),
        };

        Self {
            stop_flag,
            responder_thread,
            avahi_child,
            advertised_hostname: mdns_host,
            active_method,
        }
    }

    pub fn avahi_pid(&self) -> Option<u32> {
        self.avahi_child.as_ref().map(|c| c.id())
    }

    pub fn stop(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(mut child) = self.avahi_child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(handle) = self.responder_thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for MdnsAdvertiser {
    fn drop(&mut self) {
        self.stop();
    }
}

fn spawn_udp_mdns_responder(
    interface_ip: Ipv4Addr,
    mdns_hostname: String,
    stop_flag: Arc<AtomicBool>,
) -> Option<JoinHandle<()>> {
    let multicast_ip = Ipv4Addr::new(224, 0, 0, 251);
    let sock = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 5353)).ok()?;
    let _ = sock.set_read_timeout(Some(Duration::from_millis(200)));
    let _ = sock.join_multicast_v4(&multicast_ip, &interface_ip);

    let target_names = [
        mdns_hostname.trim_end_matches('.').to_lowercase(),
        "labdrop.local".to_string(),
    ];

    Some(thread::spawn(move || {
        let mut buf = [0u8; 1500];
        while !stop_flag.load(Ordering::Relaxed) {
            let Ok((len, src)) = sock.recv_from(&mut buf) else {
                continue;
            };
            if len < 12 {
                continue;
            }
            if let Some(resp) =
                build_mdns_a_response(&buf[..len], &target_names, interface_ip)
            {
                let _ = sock.send_to(&resp, src);
                let _ = sock.send_to(&resp, SocketAddrV4::new(multicast_ip, 5353));
            }
        }
    }))
}

/// Minimal DNS packet parser/responder for mDNS `A` (IPv4) queries matching our hostname.
pub fn build_mdns_a_response(
    packet: &[u8],
    target_names: &[String],
    ip: Ipv4Addr,
) -> Option<Vec<u8>> {
    if packet.len() < 12 {
        return None;
    }
    let flags = u16::from_be_bytes([packet[2], packet[3]]);
    // Must be a standard query (QR bit == 0)
    if (flags & 0x8000) != 0 {
        return None;
    }
    let qdcount = u16::from_be_bytes([packet[4], packet[5]]);
    if qdcount == 0 {
        return None;
    }

    // Parse the first QNAME starting at byte 12
    let mut pos = 12usize;
    let mut labels = Vec::new();
    while pos < packet.len() {
        let l = packet[pos] as usize;
        pos += 1;
        if l == 0 {
            break;
        }
        if (l & 0xC0) != 0 || pos + l > packet.len() {
            return None;
        }
        let label = std::str::from_utf8(&packet[pos..pos + l]).ok()?;
        labels.push(label.to_lowercase());
        pos += l;
    }
    if pos + 4 > packet.len() {
        return None;
    }
    let qtype = u16::from_be_bytes([packet[pos], packet[pos + 1]]);
    // QTYPE 1 (A) or 255 (ANY)
    if qtype != 1 && qtype != 255 {
        return None;
    }

    let queried_name = labels.join(".");
    if !target_names.iter().any(|t| t == &queried_name) {
        return None;
    }

    let qname_bytes = &packet[12..pos];
    let mut resp = Vec::with_capacity(64);
    // Transaction ID
    resp.extend_from_slice(&packet[0..2]);
    // Flags: 0x8400 (Response + Authoritative)
    resp.extend_from_slice(&0x8400u16.to_be_bytes());
    // QDCOUNT = 0, ANCOUNT = 1, NSCOUNT = 0, ARCOUNT = 0
    resp.extend_from_slice(&0u16.to_be_bytes());
    resp.extend_from_slice(&1u16.to_be_bytes());
    resp.extend_from_slice(&0u16.to_be_bytes());
    resp.extend_from_slice(&0u16.to_be_bytes());
    // Answer NAME
    resp.extend_from_slice(qname_bytes);
    // TYPE = A (1)
    resp.extend_from_slice(&1u16.to_be_bytes());
    // CLASS = IN (1) | cache-flush (0x8001)
    resp.extend_from_slice(&0x8001u16.to_be_bytes());
    // TTL = 120 seconds
    resp.extend_from_slice(&120u32.to_be_bytes());
    // RDLENGTH = 4
    resp.extend_from_slice(&4u16.to_be_bytes());
    // RDATA = IPv4 octets
    resp.extend_from_slice(&ip.octets());

    Some(resp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_subnet_cidr_should_mask_host_bits() {
        let ip = Ipv4Addr::new(192, 168, 18, 171);
        assert_eq!(compute_subnet_cidr(ip, 24), "192.168.18.0/24");
        assert_eq!(compute_subnet_cidr(ip, 16), "192.168.0.0/16");
    }

    #[test]
    fn discover_network_interfaces_should_return_at_least_loopback() {
        let ifaces = discover_network_interfaces();
        assert!(!ifaces.is_empty());
        assert!(ifaces.iter().any(|i| i.is_loopback));
    }

    #[test]
    fn mdns_a_query_builder_should_respond_to_matching_hostname() {
        // Construct a minimal DNS query for "labdrop.local" type A
        let mut query = vec![
            0x12, 0x34, // ID
            0x00, 0x00, // Flags (query)
            0x00, 0x01, // QDCOUNT = 1
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        query.push(7);
        query.extend_from_slice(b"labdrop");
        query.push(5);
        query.extend_from_slice(b"local");
        query.push(0);
        query.extend_from_slice(&1u16.to_be_bytes()); // QTYPE A
        query.extend_from_slice(&1u16.to_be_bytes()); // QCLASS IN

        let targets = vec!["labdrop.local".to_string()];
        let ip = Ipv4Addr::new(192, 168, 18, 171);
        let resp = build_mdns_a_response(&query, &targets, ip).expect("should build A response");
        assert_eq!(&resp[resp.len() - 4..], &[192, 168, 18, 171]);
    }
}
