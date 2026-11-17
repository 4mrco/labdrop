use crate::network::build_mdns_a_response;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Metadata describing the active LabDrop host for SSDP/UPnP, WS-Discovery, and LLMNR.
#[derive(Debug, Clone)]
pub struct DiscoveryConfig {
    pub session_id: String,
    pub device_uuid: String,
    pub hostname: String,
    pub interface_ip: Ipv4Addr,
    pub http_port: Option<u16>,
    pub smb_port: Option<u16>,
    pub webdav_enabled: bool,
}

impl DiscoveryConfig {
    pub fn deterministic_uuid(session_id: &str) -> String {
        // Produce a stable RFC 4122 v4-formatted UUID derived from session_id + timestamp
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let mut h1: u64 = 0xcbf29ce484222325;
        for &b in session_id.as_bytes() {
            h1 ^= b as u64;
            h1 = h1.wrapping_mul(0x100000001b3);
        }
        let h2: u64 = (now as u64) ^ h1.rotate_left(17) ^ 0x9e3779b97f4a7c15;
        let p1 = (h1 >> 32) as u32;
        let p2 = (h1 >> 16) as u16;
        let p3 = ((h1 as u16) & 0x0FFF) | 0x4000;
        let p4 = (((h2 >> 48) as u16) & 0x3FFF) | 0x8000;
        let p5 = h2 & 0x0000_FFFF_FFFF_FFFF;
        format!("{p1:08x}-{p2:04x}-{p3:04x}-{p4:04x}-{p5:012x}")
    }
}

/// Manages Windows & LAN Network Discovery responders:
/// - SSDP / UPnP (`239.255.255.250:1900`) -> appears in Windows File Explorer -> Network -> Other Devices (1-click opens browser)
/// - WS-Discovery (`239.255.255.250:3702`) -> appears in Windows File Explorer -> Network -> Computers when SMB port 445 is active
/// - LLMNR (`224.0.0.252:5355`) -> resolves `\\<hostname>` and `http://<hostname>:8080` on Windows LANs
pub struct NetworkDiscovery {
    stop_flag: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    pub ssdp_active: bool,
    pub wsd_active: bool,
    pub llmnr_active: bool,
}

impl NetworkDiscovery {
    pub fn start(cfg: DiscoveryConfig) -> Self {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let mut threads = Vec::new();

        let mut ssdp_active = false;
        let mut wsd_active = false;
        let mut llmnr_active = false;

        if cfg.http_port.is_some() {
            if let Some(h) = spawn_ssdp_responder(cfg.clone(), Arc::clone(&stop_flag)) {
                ssdp_active = true;
                threads.push(h);
            }
        }

        // Only advertise as a Windows WSD Computer (`\\HOSTNAME`) when SMB port 445 is actually active,
        // so Windows Explorer does not show a dead SMB computer entry in rootless port-4445 mode.
        if cfg.smb_port == Some(445) && cfg.http_port.is_some() {
            if let Some(h) = spawn_wsd_responder(cfg.clone(), Arc::clone(&stop_flag)) {
                wsd_active = true;
                threads.push(h);
            }
        }

        if let Some(h) = spawn_llmnr_responder(cfg, Arc::clone(&stop_flag)) {
            llmnr_active = true;
            threads.push(h);
        }

        Self {
            stop_flag,
            threads,
            ssdp_active,
            wsd_active,
            llmnr_active,
        }
    }

    pub fn stop(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        for h in self.threads.drain(..) {
            let _ = h.join();
        }
    }
}

impl Drop for NetworkDiscovery {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Binds an IPv4 UDP socket with `SO_REUSEADDR` (and `SO_REUSEPORT` on Unix) and joins `multicast_ip`
/// on `interface_ip`, allowing coexistence with OS discovery daemons.
pub fn bind_reusable_multicast_udp(
    port: u16,
    multicast_ip: Ipv4Addr,
    interface_ip: Ipv4Addr,
) -> Option<UdpSocket> {
    #[cfg(unix)]
    let sock = {
        use std::os::unix::io::FromRawFd;
        unsafe {
            let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, libc::IPPROTO_UDP);
            if fd < 0 {
                return None;
            }
            let yes: libc::c_int = 1;
            let _ = libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_REUSEADDR,
                &yes as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
            let _ = libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_REUSEPORT,
                &yes as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
            let addr = libc::sockaddr_in {
                sin_family: libc::AF_INET as libc::sa_family_t,
                sin_port: port.to_be(),
                sin_addr: libc::in_addr { s_addr: 0 }, // INADDR_ANY
                sin_zero: [0; 8],
            };
            if libc::bind(
                fd,
                &addr as *const libc::sockaddr_in as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            ) != 0
            {
                libc::close(fd);
                return None;
            }
            let if_in = libc::in_addr {
                s_addr: u32::from_ne_bytes(interface_ip.octets()),
            };
            let _ = libc::setsockopt(
                fd,
                libc::IPPROTO_IP,
                libc::IP_MULTICAST_IF,
                &if_in as *const libc::in_addr as *const libc::c_void,
                std::mem::size_of::<libc::in_addr>() as libc::socklen_t,
            );
            UdpSocket::from_raw_fd(fd)
        }
    };

    #[cfg(windows)]
    let sock = {
        use std::os::windows::io::{FromRawSocket, RawSocket};
        #[repr(C)]
        struct SockAddrIn {
            sin_family: u16,
            sin_port: u16,
            sin_addr: u32,
            sin_zero: [u8; 8],
        }
        #[link(name = "ws2_32")]
        extern "system" {
            fn socket(af: i32, kind: i32, protocol: i32) -> usize;
            fn setsockopt(s: usize, level: i32, optname: i32, optval: *const u8, optlen: i32) -> i32;
            fn bind(s: usize, name: *const SockAddrIn, namelen: i32) -> i32;
            fn closesocket(s: usize) -> i32;
        }
        const AF_INET: i32 = 2;
        const SOCK_DGRAM: i32 = 2;
        const IPPROTO_UDP: i32 = 17;
        const IPPROTO_IP: i32 = 0;
        const IP_MULTICAST_IF: i32 = 9;
        const SOL_SOCKET: i32 = 0xffff;
        const SO_REUSEADDR: i32 = 0x0004;
        const INVALID_SOCKET: usize = !0;

        unsafe {
            let s = socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP);
            if s == INVALID_SOCKET {
                return None;
            }
            let yes: i32 = 1;
            let _ = setsockopt(
                s,
                SOL_SOCKET,
                SO_REUSEADDR,
                &yes as *const i32 as *const u8,
                std::mem::size_of::<i32>() as i32,
            );
            let addr = SockAddrIn {
                sin_family: AF_INET as u16,
                sin_port: port.to_be(),
                sin_addr: 0,
                sin_zero: [0; 8],
            };
            if bind(s, &addr, std::mem::size_of::<SockAddrIn>() as i32) != 0 {
                let _ = closesocket(s);
                return None;
            }
            let if_addr = u32::from_ne_bytes(interface_ip.octets());
            let _ = setsockopt(
                s,
                IPPROTO_IP,
                IP_MULTICAST_IF,
                &if_addr as *const u32 as *const u8,
                std::mem::size_of::<u32>() as i32,
            );
            UdpSocket::from_raw_socket(s as RawSocket)
        }
    };

    let _ = sock.set_read_timeout(Some(Duration::from_millis(250)));
    let _ = sock.join_multicast_v4(&multicast_ip, &interface_ip);
    let _ = sock.set_multicast_ttl_v4(4);
    Some(sock)
}

/// Renders the UPnP Device Description XML served at `GET /upnp/description.xml`.
pub fn render_upnp_description_xml(cfg: &DiscoveryConfig) -> String {
    let port = cfg.http_port.unwrap_or(8080);
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
         <root xmlns=\"urn:schemas-upnp-org:device-1-0\">\n\
           <specVersion><major>1</major><minor>0</minor></specVersion>\n\
           <device>\n\
             <deviceType>urn:schemas-upnp-org:device:Basic:1</deviceType>\n\
             <friendlyName>LabDrop ({host})</friendlyName>\n\
             <manufacturer>LabDrop</manufacturer>\n\
             <manufacturerURL>https://github.com/4mrco/labdrop</manufacturerURL>\n\
             <modelDescription>LabDrop Portable Laboratory File Share</modelDescription>\n\
             <modelName>LabDrop</modelName>\n\
             <modelNumber>{ver}</modelNumber>\n\
             <serialNumber>{sess}</serialNumber>\n\
             <UDN>uuid:{uuid}</UDN>\n\
             <presentationURL>http://{ip}:{port}/</presentationURL>\n\
           </device>\n\
         </root>\n",
        host = xml_escape(&cfg.hostname),
        ver = env!("CARGO_PKG_VERSION"),
        sess = xml_escape(&cfg.session_id),
        uuid = xml_escape(&cfg.device_uuid),
        ip = cfg.interface_ip,
    )
}

/// Builds an SSDP `M-SEARCH` response (`200 OK`) if the packet asks for a matching UPnP target.
pub fn build_ssdp_search_responses(
    packet: &str,
    cfg: &DiscoveryConfig,
) -> Vec<String> {
    if !packet.starts_with("M-SEARCH ") {
        return Vec::new();
    }
    let st_line = packet
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("st:"))
        .map(|l| l[3..].trim().to_ascii_lowercase())
        .unwrap_or_default();

    let all_targets = [
        "upnp:rootdevice".to_string(),
        format!("uuid:{}", cfg.device_uuid),
        "urn:schemas-upnp-org:device:Basic:1".to_string(),
    ];

    let matched: Vec<String> = if st_line == "ssdp:all" {
        all_targets.to_vec()
    } else if all_targets.iter().any(|t| t.eq_ignore_ascii_case(&st_line)) {
        vec![st_line]
    } else {
        return Vec::new();
    };

    let port = cfg.http_port.unwrap_or(8080);
    let location = format!("http://{}:{port}/upnp/description.xml", cfg.interface_ip);
    matched
        .into_iter()
        .map(|st| {
            let usn = if st.starts_with("uuid:") {
                st.clone()
            } else {
                format!("uuid:{}::{st}", cfg.device_uuid)
            };
            format!(
                "HTTP/1.1 200 OK\r\n\
                 CACHE-CONTROL: max-age=120\r\n\
                 EXT:\r\n\
                 LOCATION: {location}\r\n\
                 SERVER: LabDrop/0.1 UPnP/1.0\r\n\
                 ST: {st}\r\n\
                 USN: {usn}\r\n\
                 \r\n"
            )
        })
        .collect()
}

fn build_ssdp_notify(cfg: &DiscoveryConfig, nts: &str) -> Vec<String> {
    let port = cfg.http_port.unwrap_or(8080);
    let location = format!("http://{}:{port}/upnp/description.xml", cfg.interface_ip);
    let targets = [
        "upnp:rootdevice".to_string(),
        format!("uuid:{}", cfg.device_uuid),
        "urn:schemas-upnp-org:device:Basic:1".to_string(),
    ];
    targets
        .into_iter()
        .map(|nt| {
            let usn = if nt.starts_with("uuid:") {
                nt.clone()
            } else {
                format!("uuid:{}::{nt}", cfg.device_uuid)
            };
            if nts == "ssdp:byebye" {
                format!(
                    "NOTIFY * HTTP/1.1\r\n\
                     HOST: 239.255.255.250:1900\r\n\
                     NT: {nt}\r\n\
                     NTS: ssdp:byebye\r\n\
                     USN: {usn}\r\n\
                     \r\n"
                )
            } else {
                format!(
                    "NOTIFY * HTTP/1.1\r\n\
                     HOST: 239.255.255.250:1900\r\n\
                     CACHE-CONTROL: max-age=120\r\n\
                     LOCATION: {location}\r\n\
                     NT: {nt}\r\n\
                     NTS: ssdp:alive\r\n\
                     SERVER: LabDrop/0.1 UPnP/1.0\r\n\
                     USN: {usn}\r\n\
                     \r\n"
                )
            }
        })
        .collect()
}

fn spawn_ssdp_responder(
    cfg: DiscoveryConfig,
    stop_flag: Arc<AtomicBool>,
) -> Option<JoinHandle<()>> {
    let mcast_ip = Ipv4Addr::new(239, 255, 255, 250);
    let mcast_addr = SocketAddr::V4(SocketAddrV4::new(mcast_ip, 1900));
    let sock = bind_reusable_multicast_udp(1900, mcast_ip, cfg.interface_ip)?;

    Some(thread::spawn(move || {
        // Announce presence immediately
        for msg in build_ssdp_notify(&cfg, "ssdp:alive") {
            let _ = sock.send_to(msg.as_bytes(), mcast_addr);
        }

        let mut buf = [0u8; 2048];
        let mut ticks = 0u32;
        while !stop_flag.load(Ordering::Relaxed) {
            match sock.recv_from(&mut buf) {
                Ok((len, src)) => {
                    let text = String::from_utf8_lossy(&buf[..len]);
                    for resp in build_ssdp_search_responses(&text, &cfg) {
                        let _ = sock.send_to(resp.as_bytes(), src);
                    }
                }
                Err(_) => {
                    ticks += 1;
                    // Re-announce every ~30 seconds (120 * 250ms)
                    if ticks.is_multiple_of(120) {
                        for msg in build_ssdp_notify(&cfg, "ssdp:alive") {
                            let _ = sock.send_to(msg.as_bytes(), mcast_addr);
                        }
                    }
                }
            }
        }

        // Send ssdp:byebye on shutdown so Windows removes the device from Network
        for msg in build_ssdp_notify(&cfg, "ssdp:byebye") {
            let _ = sock.send_to(msg.as_bytes(), mcast_addr);
        }
    }))
}

/// Parses an incoming WS-Discovery SOAP UDP datagram and returns a `ProbeMatches` or `ResolveMatches` reply.
pub fn build_wsd_udp_reply(packet: &str, cfg: &DiscoveryConfig, msg_num: u64) -> Option<String> {
    let is_probe = packet.contains(":Probe") || packet.contains("<Probe");
    let is_resolve = packet.contains(":Resolve") || packet.contains("<Resolve");
    if !is_probe && !is_resolve {
        return None;
    }

    let relates_to = extract_xml_tag_text(packet, "MessageID")?;
    let port = cfg.http_port.unwrap_or(8080);
    let xaddr = format!("http://{}:{port}/wsd/{}", cfg.interface_ip, cfg.device_uuid);
    let msg_uuid = format!("urn:uuid:{}-{msg_num:04x}", &cfg.device_uuid[..28]);
    let seq_uuid = format!("urn:uuid:{}", cfg.device_uuid);

    if is_probe {
        // Match generic device probes or computer probes
        if !packet.contains("Device") && !packet.contains("Computer") && packet.contains("Types>") {
            return None;
        }
        Some(format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
             <soap:Envelope xmlns:soap=\"http://www.w3.org/2003/05/soap-envelope\" \
             xmlns:wsa=\"http://schemas.xmlsoap.org/ws/2004/08/addressing\" \
             xmlns:wsd=\"http://schemas.xmlsoap.org/ws/2005/04/discovery\" \
             xmlns:wsdp=\"http://schemas.xmlsoap.org/ws/2006/02/devprof\" \
             xmlns:pub=\"http://schemas.microsoft.com/windows/pub/2005/07\">\
             <soap:Header>\
               <wsa:To>http://schemas.xmlsoap.org/ws/2004/08/addressing/role/anonymous</wsa:To>\
               <wsa:Action>http://schemas.xmlsoap.org/ws/2005/04/discovery/ProbeMatches</wsa:Action>\
               <wsa:MessageID>{msg_uuid}</wsa:MessageID>\
               <wsa:RelatesTo>{relates_to}</wsa:RelatesTo>\
               <wsd:AppSequence InstanceId=\"1\" SequenceId=\"{seq_uuid}\" MessageNumber=\"{msg_num}\"/>\
             </soap:Header>\
             <soap:Body>\
               <wsd:ProbeMatches>\
                 <wsd:ProbeMatch>\
                   <wsa:EndpointReference><wsa:Address>urn:uuid:{uuid}</wsa:Address></wsa:EndpointReference>\
                   <wsd:Types>wsdp:Device pub:Computer</wsd:Types>\
                   <wsd:XAddrs>{xaddr}</wsd:XAddrs>\
                   <wsd:MetadataVersion>1</wsd:MetadataVersion>\
                 </wsd:ProbeMatch>\
               </wsd:ProbeMatches>\
             </soap:Body>\
             </soap:Envelope>",
            uuid = cfg.device_uuid,
        ))
    } else {
        if !packet.contains(&cfg.device_uuid) {
            return None;
        }
        Some(format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
             <soap:Envelope xmlns:soap=\"http://www.w3.org/2003/05/soap-envelope\" \
             xmlns:wsa=\"http://schemas.xmlsoap.org/ws/2004/08/addressing\" \
             xmlns:wsd=\"http://schemas.xmlsoap.org/ws/2005/04/discovery\" \
             xmlns:wsdp=\"http://schemas.xmlsoap.org/ws/2006/02/devprof\" \
             xmlns:pub=\"http://schemas.microsoft.com/windows/pub/2005/07\">\
             <soap:Header>\
               <wsa:To>http://schemas.xmlsoap.org/ws/2004/08/addressing/role/anonymous</wsa:To>\
               <wsa:Action>http://schemas.xmlsoap.org/ws/2005/04/discovery/ResolveMatches</wsa:Action>\
               <wsa:MessageID>{msg_uuid}</wsa:MessageID>\
               <wsa:RelatesTo>{relates_to}</wsa:RelatesTo>\
               <wsd:AppSequence InstanceId=\"1\" SequenceId=\"{seq_uuid}\" MessageNumber=\"{msg_num}\"/>\
             </soap:Header>\
             <soap:Body>\
               <wsd:ResolveMatches>\
                 <wsd:ResolveMatch>\
                   <wsa:EndpointReference><wsa:Address>urn:uuid:{uuid}</wsa:Address></wsa:EndpointReference>\
                   <wsd:Types>wsdp:Device pub:Computer</wsd:Types>\
                   <wsd:XAddrs>{xaddr}</wsd:XAddrs>\
                   <wsd:MetadataVersion>1</wsd:MetadataVersion>\
                 </wsd:ResolveMatch>\
               </wsd:ResolveMatches>\
             </soap:Body>\
             </soap:Envelope>",
            uuid = cfg.device_uuid,
        ))
    }
}

/// Builds the WS-Transfer `GetResponse` SOAP XML served over HTTP `POST /wsd/<uuid>`.
pub fn build_wsd_http_get_response(request_body: &str, cfg: &DiscoveryConfig) -> String {
    let relates_to = extract_xml_tag_text(request_body, "MessageID")
        .unwrap_or_else(|| format!("urn:uuid:{}", cfg.device_uuid));
    let comp_upper = cfg.hostname.to_uppercase();
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
         <soap:Envelope xmlns:soap=\"http://www.w3.org/2003/05/soap-envelope\" \
         xmlns:wsa=\"http://schemas.xmlsoap.org/ws/2004/08/addressing\" \
         xmlns:wsx=\"http://schemas.xmlsoap.org/ws/2004/09/mex\" \
         xmlns:wsdp=\"http://schemas.xmlsoap.org/ws/2006/02/devprof\" \
         xmlns:pnpx=\"http://schemas.microsoft.com/windows/pnpx/2005/10\" \
         xmlns:pub=\"http://schemas.microsoft.com/windows/pub/2005/07\">\
         <soap:Header>\
           <wsa:To>http://schemas.xmlsoap.org/ws/2004/08/addressing/role/anonymous</wsa:To>\
           <wsa:Action>http://schemas.xmlsoap.org/ws/2004/09/transfer/GetResponse</wsa:Action>\
           <wsa:RelatesTo>{relates_to}</wsa:RelatesTo>\
         </soap:Header>\
         <soap:Body>\
           <wsx:Metadata>\
             <wsx:MetadataSection Dialect=\"http://schemas.xmlsoap.org/ws/2006/02/devprof/ThisDevice\">\
               <wsdp:ThisDevice>\
                 <wsdp:FriendlyName>LabDrop ({host})</wsdp:FriendlyName>\
                 <wsdp:FirmwareVersion>1.0</wsdp:FirmwareVersion>\
                 <wsdp:SerialNumber>1</wsdp:SerialNumber>\
               </wsdp:ThisDevice>\
             </wsx:MetadataSection>\
             <wsx:MetadataSection Dialect=\"http://schemas.xmlsoap.org/ws/2006/02/devprof/ThisModel\">\
               <wsdp:ThisModel>\
                 <wsdp:Manufacturer>LabDrop</wsdp:Manufacturer>\
                 <wsdp:ModelName>LabDrop File Share</wsdp:ModelName>\
                 <pnpx:DeviceCategory>Computers</pnpx:DeviceCategory>\
               </wsdp:ThisModel>\
             </wsx:MetadataSection>\
             <wsx:MetadataSection Dialect=\"http://schemas.xmlsoap.org/ws/2006/02/devprof/Relationship\">\
               <wsdp:Relationship Type=\"http://schemas.xmlsoap.org/ws/2006/02/devprof/host\">\
                 <wsdp:Host>\
                   <wsa:EndpointReference><wsa:Address>urn:uuid:{uuid}</wsa:Address></wsa:EndpointReference>\
                   <wsdp:Types>pub:Computer</wsdp:Types>\
                   <wsdp:ServiceId>urn:uuid:{uuid}</wsdp:ServiceId>\
                   <pub:Computer>{comp_upper}/Workgroup:WORKGROUP</pub:Computer>\
                 </wsdp:Host>\
               </wsdp:Relationship>\
             </wsx:MetadataSection>\
           </wsx:Metadata>\
         </soap:Body>\
         </soap:Envelope>",
        host = xml_escape(&cfg.hostname),
        uuid = cfg.device_uuid,
    )
}

fn build_wsd_hello_or_bye(cfg: &DiscoveryConfig, is_hello: bool, msg_num: u64) -> String {
    let port = cfg.http_port.unwrap_or(8080);
    let xaddr = format!("http://{}:{port}/wsd/{}", cfg.interface_ip, cfg.device_uuid);
    let action = if is_hello {
        "http://schemas.xmlsoap.org/ws/2005/04/discovery/Hello"
    } else {
        "http://schemas.xmlsoap.org/ws/2005/04/discovery/Bye"
    };
    let tag = if is_hello { "Hello" } else { "Bye" };
    let extra = if is_hello {
        format!(
            "<wsd:Types>wsdp:Device pub:Computer</wsd:Types>\
             <wsd:XAddrs>{xaddr}</wsd:XAddrs>\
             <wsd:MetadataVersion>1</wsd:MetadataVersion>"
        )
    } else {
        String::new()
    };

    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
         <soap:Envelope xmlns:soap=\"http://www.w3.org/2003/05/soap-envelope\" \
         xmlns:wsa=\"http://schemas.xmlsoap.org/ws/2004/08/addressing\" \
         xmlns:wsd=\"http://schemas.xmlsoap.org/ws/2005/04/discovery\" \
         xmlns:wsdp=\"http://schemas.xmlsoap.org/ws/2006/02/devprof\" \
         xmlns:pub=\"http://schemas.microsoft.com/windows/pub/2005/07\">\
         <soap:Header>\
           <wsa:To>urn:schemas-xmlsoap-org:ws:2005:04:discovery</wsa:To>\
           <wsa:Action>{action}</wsa:Action>\
           <wsa:MessageID>urn:uuid:{uuid}-{msg_num:04x}</wsa:MessageID>\
           <wsd:AppSequence InstanceId=\"1\" SequenceId=\"urn:uuid:{uuid}\" MessageNumber=\"{msg_num}\"/>\
         </soap:Header>\
         <soap:Body>\
           <wsd:{tag}>\
             <wsa:EndpointReference><wsa:Address>urn:uuid:{uuid}</wsa:Address></wsa:EndpointReference>\
             {extra}\
           </wsd:{tag}>\
         </soap:Body>\
         </soap:Envelope>",
        uuid = cfg.device_uuid,
    )
}

fn spawn_wsd_responder(
    cfg: DiscoveryConfig,
    stop_flag: Arc<AtomicBool>,
) -> Option<JoinHandle<()>> {
    let mcast_ip = Ipv4Addr::new(239, 255, 255, 250);
    let mcast_addr = SocketAddr::V4(SocketAddrV4::new(mcast_ip, 3702));
    let sock = bind_reusable_multicast_udp(3702, mcast_ip, cfg.interface_ip)?;

    Some(thread::spawn(move || {
        let mut msg_num = 1u64;
        let hello = build_wsd_hello_or_bye(&cfg, true, msg_num);
        let _ = sock.send_to(hello.as_bytes(), mcast_addr);

        let mut buf = [0u8; 8192];
        while !stop_flag.load(Ordering::Relaxed) {
            let Ok((len, src)) = sock.recv_from(&mut buf) else {
                continue;
            };
            let text = String::from_utf8_lossy(&buf[..len]);
            msg_num += 1;
            if let Some(reply) = build_wsd_udp_reply(&text, &cfg, msg_num) {
                let _ = sock.send_to(reply.as_bytes(), src);
            }
        }

        msg_num += 1;
        let bye = build_wsd_hello_or_bye(&cfg, false, msg_num);
        let _ = sock.send_to(bye.as_bytes(), mcast_addr);
    }))
}

fn spawn_llmnr_responder(
    cfg: DiscoveryConfig,
    stop_flag: Arc<AtomicBool>,
) -> Option<JoinHandle<()>> {
    let mcast_ip = Ipv4Addr::new(224, 0, 0, 252);
    let sock = bind_reusable_multicast_udp(5355, mcast_ip, cfg.interface_ip)?;
    let targets = vec![cfg.hostname.to_lowercase(), "labdrop".to_string()];

    Some(thread::spawn(move || {
        let mut buf = [0u8; 1500];
        while !stop_flag.load(Ordering::Relaxed) {
            let Ok((len, src)) = sock.recv_from(&mut buf) else {
                continue;
            };
            if let Some(mut resp) = build_mdns_a_response(&buf[..len], &targets, cfg.interface_ip)
            {
                // LLMNR response flags = 0x8000 (standard response, non-authoritative bit not required)
                resp[2] = 0x80;
                resp[3] = 0x00;
                // LLMNR unicast reply preserves standard CLASS IN (0x0001) and 30s TTL
                if resp.len() >= 12 {
                    let ans_off = resp.len() - 10;
                    resp[ans_off] = 0x00;
                    resp[ans_off + 1] = 0x01;
                }
                let _ = sock.send_to(&resp, src);
            }
        }
    }))
}

fn extract_xml_tag_text(xml: &str, local_name: &str) -> Option<String> {
    // Matches `<MessageID>...</MessageID>` or `<wsa:MessageID ...>...</wsa:MessageID>`
    let open_needle = format!(":{local_name}");
    let plain_needle = format!("<{local_name}");
    let start_pos = xml.find(&open_needle).or_else(|| xml.find(&plain_needle))?;
    let gt = xml[start_pos..].find('>')? + start_pos + 1;
    let end = xml[gt..].find('<')? + gt;
    Some(xml[gt..end].trim().to_string())
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_cfg() -> DiscoveryConfig {
        DiscoveryConfig {
            session_id: "ld-test123".to_string(),
            device_uuid: DiscoveryConfig::deterministic_uuid("ld-test123"),
            hostname: "lab-host".to_string(),
            interface_ip: Ipv4Addr::new(192, 168, 0, 105),
            http_port: Some(8080),
            smb_port: Some(445),
            webdav_enabled: true,
        }
    }

    #[test]
    fn ssdp_msearch_should_return_upnp_location_pointing_to_http_server() {
        let cfg = sample_cfg();
        let req = "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\nST: ssdp:all\r\n\r\n";
        let resps = build_ssdp_search_responses(req, &cfg);
        assert_eq!(resps.len(), 3);
        assert!(resps[0].contains("LOCATION: http://192.168.0.105:8080/upnp/description.xml"));

        let xml = render_upnp_description_xml(&cfg);
        assert!(xml.contains("<friendlyName>LabDrop (lab-host)</friendlyName>"));
        assert!(xml.contains("<presentationURL>http://192.168.0.105:8080/</presentationURL>"));
    }

    #[test]
    fn wsd_probe_and_get_metadata_should_produce_valid_windows_computer_soap() {
        let cfg = sample_cfg();
        let probe = "<soap:Envelope><soap:Header><wsa:MessageID>urn:uuid:11111111-2222-3333-4444-555555555555</wsa:MessageID></soap:Header><soap:Body><wsd:Probe><wsd:Types>wsdp:Device</wsd:Types></wsd:Probe></soap:Body></soap:Envelope>";
        let reply = build_wsd_udp_reply(probe, &cfg, 2).expect("should match WSD probe");
        assert!(reply.contains("ProbeMatches"));
        assert!(reply.contains("urn:uuid:11111111-2222-3333-4444-555555555555"));
        assert!(reply.contains("http://192.168.0.105:8080/wsd/"));

        let get_req = "<soap:Envelope><soap:Header><wsa:MessageID>urn:uuid:aaaa-bbbb</wsa:MessageID></soap:Header><soap:Body/></soap:Envelope>";
        let get_resp = build_wsd_http_get_response(get_req, &cfg);
        assert!(get_resp.contains("<pub:Computer>LAB-HOST/Workgroup:WORKGROUP</pub:Computer>"));
        assert!(get_resp.contains("<wsdp:FriendlyName>LabDrop (lab-host)</wsdp:FriendlyName>"));
    }
}
