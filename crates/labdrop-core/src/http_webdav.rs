use crate::discovery::{
    build_wsd_http_get_response, render_upnp_description_xml, DiscoveryConfig,
};
use crate::error::{LabDropError, Result};
use crate::fs_space::format_bytes;
use crate::staging::StagingManifest;
use crate::transfer::{ProtocolKind, TransferTracker};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;

/// Operating mode of the embedded HTTP / WebDAV server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddedServerMode {
    /// Serves both WebDAV (RFC 4918) for native file managers and HTML/ZIP for browsers.
    WebDavAndHttp,
    /// Pure HTTP fallback mode (disables WebDAV PROPFIND/OPTIONS advertisement).
    HttpOnly,
}

/// Running handle for the embedded HTTP / WebDAV file server.
pub struct EmbeddedFileServer {
    pub bind_ip: Ipv4Addr,
    pub port: u16,
    pub mode: EmbeddedServerMode,
    stop_flag: Arc<AtomicBool>,
    accept_threads: Vec<JoinHandle<()>>,
}

impl EmbeddedFileServer {
    /// Starts the embedded server bound strictly to `interface_ip:port` (plus `127.0.0.1:port` if non-loopback).
    pub fn start(
        interface_ip: Ipv4Addr,
        preferred_port: u16,
        mode: EmbeddedServerMode,
        staged_dir: PathBuf,
        manifest: Option<StagingManifest>,
        tracker: Arc<TransferTracker>,
    ) -> Result<Self> {
        Self::start_with_discovery(
            interface_ip,
            preferred_port,
            mode,
            staged_dir,
            manifest,
            tracker,
            None,
        )
    }

    pub fn start_with_discovery(
        interface_ip: Ipv4Addr,
        preferred_port: u16,
        mode: EmbeddedServerMode,
        staged_dir: PathBuf,
        manifest: Option<StagingManifest>,
        tracker: Arc<TransferTracker>,
        mut discovery_cfg: Option<DiscoveryConfig>,
    ) -> Result<Self> {
        let canonical_root = fs::canonicalize(&staged_dir).map_err(|e| LabDropError::IoPath {
            path: staged_dir.clone(),
            source: e,
        })?;

        let bind_addr = SocketAddrV4::new(interface_ip, preferred_port);
        let primary_listener = TcpListener::bind(bind_addr).or_else(|_| {
            // If preferred_port is busy, fall back to ephemeral port
            TcpListener::bind(SocketAddrV4::new(interface_ip, 0))
        }).map_err(|e| {
            LabDropError::Network(format!("Failed to bind server on {interface_ip}:{preferred_port}: {e}"))
        })?;

        let actual_port = primary_listener
            .local_addr()
            .map_err(LabDropError::Io)?
            .port();
        primary_listener
            .set_nonblocking(true)
            .map_err(LabDropError::Io)?;

        if let Some(ref mut d) = discovery_cfg {
            d.http_port = Some(actual_port);
        }
        let discovery_arc = Arc::new(discovery_cfg);

        let mut listeners = vec![primary_listener];
        if !interface_ip.is_loopback() && !interface_ip.is_unspecified() {
            if let Ok(lo_listener) =
                TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, actual_port))
            {
                let _ = lo_listener.set_nonblocking(true);
                listeners.push(lo_listener);
            }
        }

        let mut sha_map = HashMap::new();
        if let Some(m) = manifest {
            for f in m.files {
                sha_map.insert(f.relative_path, f.sha256);
            }
        }
        let sha_map = Arc::new(sha_map);
        let stop_flag = Arc::new(AtomicBool::new(false));
        let mut accept_threads = Vec::new();

        for listener in listeners {
            let stop = Arc::clone(&stop_flag);
            let root = canonical_root.clone();
            let shas = Arc::clone(&sha_map);
            let trk = Arc::clone(&tracker);
            let disc = Arc::clone(&discovery_arc);

            let handle = thread::spawn(move || {
                accept_loop(listener, stop, root, shas, trk, mode, disc);
            });
            accept_threads.push(handle);
        }

        Ok(Self {
            bind_ip: interface_ip,
            port: actual_port,
            mode,
            stop_flag,
            accept_threads,
        })
    }

    pub fn stop(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        // Poke listeners to wake any accept poll immediately
        let _ = TcpStream::connect_timeout(
            &SocketAddr::V4(SocketAddrV4::new(self.bind_ip, self.port)),
            Duration::from_millis(50),
        );
        for handle in self.accept_threads.drain(..) {
            let _ = handle.join();
        }
    }
}

impl Drop for EmbeddedFileServer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn accept_loop(
    listener: TcpListener,
    stop_flag: Arc<AtomicBool>,
    canonical_root: PathBuf,
    sha_map: Arc<HashMap<String, String>>,
    tracker: Arc<TransferTracker>,
    mode: EmbeddedServerMode,
    discovery: Arc<Option<DiscoveryConfig>>,
) {
    while !stop_flag.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, peer_addr)) => {
                // On Windows (Winsock2), accepted sockets inherit the listener's non-blocking mode.
                // Explicitly switch back to blocking mode so large responses do not fail with WouldBlock.
                let _ = stream.set_nonblocking(false);
                let stop = Arc::clone(&stop_flag);
                let root = canonical_root.clone();
                let shas = Arc::clone(&sha_map);
                let trk = Arc::clone(&tracker);
                let disc = Arc::clone(&discovery);
                thread::spawn(move || {
                    let _ = handle_client(stream, peer_addr, stop, &root, &shas, &trk, mode, &disc);
                });
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(25));
            }
            Err(_) => {
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

#[derive(Debug)]
struct HttpRequest {
    method: String,
    raw_target: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

fn read_http_request(stream: &mut TcpStream) -> Option<HttpRequest> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(15)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));

    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).ok()? == 0 {
        return None;
    }
    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() < 2 {
        return None;
    }
    let method = parts[0].to_uppercase();
    let raw_target = parts[1].to_string();

    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            break;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            headers.insert(k.trim().to_lowercase(), v.trim().to_string());
        }
    }

    let content_len = headers
        .get("content-length")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0)
        .min(256 * 1024);

    let mut body = vec![0u8; content_len];
    if content_len > 0 {
        reader.read_exact(&mut body).ok()?;
    }

    Some(HttpRequest {
        method,
        raw_target,
        headers,
        body,
    })
}

#[allow(clippy::too_many_arguments)]
fn handle_client(
    mut stream: TcpStream,
    peer_addr: SocketAddr,
    stop_flag: Arc<AtomicBool>,
    canonical_root: &Path,
    sha_map: &HashMap<String, String>,
    tracker: &TransferTracker,
    mode: EmbeddedServerMode,
    discovery: &Option<DiscoveryConfig>,
) -> std::io::Result<()> {
    let client_ip = peer_addr.ip().to_string();
    let Some(req) = read_http_request(&mut stream) else {
        return Ok(());
    };

    // Virtual UPnP device description endpoint for Windows Network Discovery (`Other Devices`)
    if let Some(ref disc) = discovery {
        if req.method == "GET" && req.raw_target == "/upnp/description.xml" {
            let xml = render_upnp_description_xml(disc);
            return send_simple_response(
                &mut stream,
                "200 OK",
                "text/xml; charset=utf-8",
                xml.as_bytes(),
                &[],
            );
        }
        // Virtual WS-Discovery metadata endpoint (`POST /wsd/<uuid>`) for Windows Network Computers
        let wsd_path = format!("/wsd/{}", disc.device_uuid);
        if req.method == "POST" && req.raw_target == wsd_path {
            let req_body = String::from_utf8_lossy(&req.body);
            let soap = build_wsd_http_get_response(&req_body, disc);
            return send_simple_response(
                &mut stream,
                "200 OK",
                "application/soap+xml; charset=utf-8",
                soap.as_bytes(),
                &[],
            );
        }
    }

    // Determine whether this request is WebDAV or standard HTTP
    let is_webdav_req = matches!(req.method.as_str(), "PROPFIND" | "OPTIONS")
        || req
            .headers
            .get("user-agent")
            .map(|ua| {
                let l = ua.to_lowercase();
                l.contains("webdav")
                    || l.contains("gvfs")
                    || l.contains("davfs")
                    || l.contains("miniredir")
                    || l.contains("cyberduck")
                    || l.contains("cadaver")
            })
            .unwrap_or(false);

    let protocol = if mode == EmbeddedServerMode::WebDavAndHttp && is_webdav_req {
        ProtocolKind::WebDav
    } else {
        ProtocolKind::Http
    };
    tracker.record_client_activity(&client_ip, protocol);

    // Reject write/mutation methods strictly (Read-Only enforcement)
    if matches!(
        req.method.as_str(),
        "PUT" | "POST" | "DELETE" | "MKCOL" | "MOVE" | "COPY" | "PROPPATCH" | "LOCK" | "UNLOCK" | "PATCH"
    ) {
        return send_simple_response(
            &mut stream,
            "403 Forbidden",
            "text/plain; charset=utf-8",
            b"LabDrop share is strictly read-only.\n",
            &[("Allow", "OPTIONS, GET, HEAD, PROPFIND")],
        );
    }

    if req.method == "OPTIONS" {
        if mode == EmbeddedServerMode::WebDavAndHttp {
            return send_simple_response(
                &mut stream,
                "200 OK",
                "text/plain; charset=utf-8",
                b"",
                &[
                    ("DAV", "1, 2"),
                    ("MS-Author-Via", "DAV"),
                    ("Allow", "OPTIONS, GET, HEAD, PROPFIND"),
                    ("Accept-Ranges", "bytes"),
                ],
            );
        } else {
            return send_simple_response(
                &mut stream,
                "200 OK",
                "text/plain; charset=utf-8",
                b"",
                &[("Allow", "OPTIONS, GET, HEAD"), ("Accept-Ranges", "bytes")],
            );
        }
    }

    // Separate path and query string
    let (raw_path, query_str) = match req.raw_target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (req.raw_target.as_str(), ""),
    };

    let decoded_path = match percent_decode(raw_path) {
        Some(p) => p,
        None => {
            return send_simple_response(
                &mut stream,
                "400 Bad Request",
                "text/plain; charset=utf-8",
                b"Invalid URL encoding.\n",
                &[],
            );
        }
    };

    // Virtual /SHA256SUMS.txt endpoint (when generated from StagingManifest and no physical SHA256SUMS.txt overrides it)
    if decoded_path == "/SHA256SUMS.txt"
        && !sha_map.is_empty()
        && !canonical_root.join("SHA256SUMS.txt").is_file()
        && matches!(req.method.as_str(), "GET" | "HEAD")
    {
        let mut lines: Vec<String> = sha_map
            .iter()
            .map(|(rel, hash)| format!("{hash}  {rel}"))
            .collect();
        lines.sort();
        let body = format!("{}\n", lines.join("\n"));
        return send_simple_response(
            &mut stream,
            "200 OK",
            "text/plain; charset=utf-8",
            if req.method == "HEAD" {
                b""
            } else {
                body.as_bytes()
            },
            &[],
        );
    }

    let Some((target_path, rel_display, virtual_prefix)) =
        resolve_safe_path(canonical_root, &decoded_path)
    else {
        return send_simple_response(
            &mut stream,
            "404 Not Found",
            "text/plain; charset=utf-8",
            b"404 Not Found\n",
            &[],
        );
    };

    if req.method == "PROPFIND" {
        if mode == EmbeddedServerMode::HttpOnly {
            return send_simple_response(
                &mut stream,
                "405 Method Not Allowed",
                "text/plain; charset=utf-8",
                b"WebDAV is disabled in HTTP-only mode.\n",
                &[("Allow", "OPTIONS, GET, HEAD")],
            );
        }
        return handle_propfind(
            &mut stream,
            &req,
            canonical_root,
            &target_path,
            &virtual_prefix,
        );
    }

    if !matches!(req.method.as_str(), "GET" | "HEAD") {
        return send_simple_response(
            &mut stream,
            "405 Method Not Allowed",
            "text/plain; charset=utf-8",
            b"Method Not Allowed\n",
            &[("Allow", "OPTIONS, GET, HEAD, PROPFIND")],
        );
    }

    let metadata = match fs::metadata(&target_path) {
        Ok(m) => m,
        Err(_) => {
            return send_simple_response(
                &mut stream,
                "404 Not Found",
                "text/plain; charset=utf-8",
                b"404 Not Found\n",
                &[],
            );
        }
    };

    if metadata.is_dir() {
        if query_str.split('&').any(|kv| kv == "zip=1") {
            return stream_directory_as_zip(
                &mut stream,
                &client_ip,
                &stop_flag,
                &target_path,
                &rel_display,
                tracker,
            );
        }
        let html = render_directory_html(canonical_root, &target_path, &rel_display, sha_map)?;
        return send_simple_response(
            &mut stream,
            "200 OK",
            "text/html; charset=utf-8",
            if req.method == "HEAD" {
                b""
            } else {
                html.as_bytes()
            },
            &[],
        );
    }

    // Serve regular file with Range support and real-time transfer progress tracking
    serve_file(
        &mut stream,
        &req,
        &client_ip,
        protocol,
        &stop_flag,
        &target_path,
        &rel_display,
        metadata.len(),
        tracker,
    )
}

/// Resolves a decoded URL path safely within `canonical_root`, rejecting any traversal outside `canonical_root`.
/// Supports optional `/LabDrop` virtual root prefix for WebDAV/SMB URL consistency.
fn resolve_safe_path(
    canonical_root: &Path,
    decoded_url_path: &str,
) -> Option<(PathBuf, String, String)> {
    if decoded_url_path.contains('\0') {
        return None;
    }

    let trimmed = decoded_url_path.trim_start_matches('/');
    // Check if the URL starts with virtual "LabDrop" prefix and no real top-level "LabDrop" exists
    let (effective_rel, virtual_prefix) = if (trimmed == "LabDrop"
        || trimmed.starts_with("LabDrop/"))
        && !canonical_root.join("LabDrop").exists()
    {
        let rest = trimmed
            .strip_prefix("LabDrop")
            .unwrap_or("")
            .trim_start_matches('/');
        (rest, "/LabDrop".to_string())
    } else {
        (trimmed, String::new())
    };

    let mut rel_buf = PathBuf::new();
    for comp in Path::new(effective_rel).components() {
        match comp {
            Component::Normal(seg) => rel_buf.push(seg),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }

    let candidate = canonical_root.join(&rel_buf);
    let canonical_target = fs::canonicalize(&candidate).ok()?;
    if !canonical_target.starts_with(canonical_root) {
        return None;
    }

    let rel_display = canonical_target
        .strip_prefix(canonical_root)
        .ok()
        .map(crate::staging::path_to_forward_slashes)
        .unwrap_or_default();

    Some((canonical_target, rel_display, virtual_prefix))
}

#[allow(clippy::too_many_arguments)]
fn serve_file(
    stream: &mut TcpStream,
    req: &HttpRequest,
    client_ip: &str,
    protocol: ProtocolKind,
    stop_flag: &AtomicBool,
    file_path: &Path,
    rel_display: &str,
    file_size: u64,
    tracker: &TransferTracker,
) -> std::io::Result<()> {
    let mut file = File::open(file_path)?;
    let filename = file_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download.bin".to_string());
    let encoded_filename = percent_encode_path_segment(&filename);
    let content_type = guess_mime_type(&filename);

    let (status_line, start_offset, end_offset, is_partial) =
        if let Some(range_hdr) = req.headers.get("range") {
            if let Some((s, e)) = parse_byte_range(range_hdr, file_size) {
                ("206 Partial Content", s, e, true)
            } else {
                return send_simple_response(
                    stream,
                    "416 Range Not Satisfiable",
                    "text/plain; charset=utf-8",
                    b"Requested range not satisfiable.\n",
                    &[("Content-Range", &format!("bytes */{file_size}"))],
                );
            }
        } else if file_size == 0 {
            ("200 OK", 0, 0, false)
        } else {
            ("200 OK", 0, file_size - 1, false)
        };

    let content_length = if file_size == 0 {
        0
    } else {
        end_offset - start_offset + 1
    };

    let mut header = format!(
        "HTTP/1.1 {status_line}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {content_length}\r\n\
         Accept-Ranges: bytes\r\n\
         Content-Disposition: inline; filename=\"{}\"; filename*=UTF-8''{encoded_filename}\r\n\
         Connection: close\r\n",
        filename.replace('"', "_")
    );
    if is_partial {
        header.push_str(&format!(
            "Content-Range: bytes {start_offset}-{end_offset}/{file_size}\r\n"
        ));
    }
    header.push_str("\r\n");

    stream.write_all(header.as_bytes())?;
    if req.method == "HEAD" || content_length == 0 {
        stream.flush()?;
        return Ok(());
    }

    if start_offset > 0 {
        file.seek(SeekFrom::Start(start_offset))?;
    }

    let transfer_id = tracker.start_transfer(
        protocol,
        client_ip,
        rel_display,
        Some(content_length),
        true,
    );

    let mut remaining = content_length;
    let mut buf = vec![0u8; 64 * 1024];
    let mut completed = true;

    while remaining > 0 {
        if stop_flag.load(Ordering::Relaxed) {
            completed = false;
            break;
        }
        let to_read = (remaining as usize).min(buf.len());
        let n = match file.read(&mut buf[..to_read]) {
            Ok(0) => {
                completed = false;
                break;
            }
            Ok(n) => n,
            Err(_) => {
                completed = false;
                break;
            }
        };
        if stream.write_all(&buf[..n]).is_err() {
            completed = false;
            break;
        }
        remaining -= n as u64;
        tracker.add_bytes(transfer_id, n as u64);
    }

    let _ = stream.flush();
    tracker.finish_transfer(transfer_id, completed);
    Ok(())
}

fn parse_byte_range(range_header: &str, file_size: u64) -> Option<(u64, u64)> {
    if file_size == 0 {
        return None;
    }
    let spec = range_header.trim().strip_prefix("bytes=")?;
    let first_range = spec.split(',').next()?.trim();
    let (start_s, end_s) = first_range.split_once('-')?;
    if start_s.is_empty() {
        // Suffix range: bytes=-500
        let suffix: u64 = end_s.parse().ok()?;
        if suffix == 0 {
            return None;
        }
        let start = file_size.saturating_sub(suffix);
        Some((start, file_size - 1))
    } else {
        let start: u64 = start_s.parse().ok()?;
        if start >= file_size {
            return None;
        }
        let end = if end_s.is_empty() {
            file_size - 1
        } else {
            end_s.parse::<u64>().ok()?.min(file_size - 1)
        };
        if start > end {
            return None;
        }
        Some((start, end))
    }
}

/// Handles WebDAV `PROPFIND` requests (RFC 4918), returning `207 Multi-Status` XML.
fn handle_propfind(
    stream: &mut TcpStream,
    req: &HttpRequest,
    canonical_root: &Path,
    target_path: &Path,
    virtual_prefix: &str,
) -> std::io::Result<()> {
    let depth_hdr = req
        .headers
        .get("depth")
        .map(|s| s.trim().to_lowercase())
        .unwrap_or_else(|| "1".to_string());

    let max_depth = match depth_hdr.as_str() {
        "0" => 0usize,
        "1" => 1usize,
        "infinity" => 8usize,
        _ => 1usize,
    };

    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:multistatus xmlns:D=\"DAV:\">\n",
    );

    for entry in WalkDir::new(target_path)
        .max_depth(max_depth)
        .follow_links(false)
        .into_iter()
        .flatten()
    {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let is_dir = meta.is_dir();
        let rel = entry
            .path()
            .strip_prefix(canonical_root)
            .unwrap_or_else(|_| Path::new(""));
        let encoded_rel = rel
            .components()
            .map(|c| percent_encode_path_segment(&c.as_os_str().to_string_lossy()))
            .collect::<Vec<_>>()
            .join("/");

        let mut href = if encoded_rel.is_empty() {
            if virtual_prefix.is_empty() {
                "/".to_string()
            } else {
                format!("{virtual_prefix}/")
            }
        } else if virtual_prefix.is_empty() {
            format!("/{encoded_rel}")
        } else {
            format!("{virtual_prefix}/{encoded_rel}")
        };
        if is_dir && !href.ends_with('/') {
            href.push('/');
        }

        let display_name = if rel.as_os_str().is_empty() {
            "LabDrop".to_string()
        } else {
            entry
                .file_name()
                .to_string_lossy()
                .into_owned()
        };

        let mtime_http = format_http_date(meta.modified().unwrap_or(UNIX_EPOCH));
        xml.push_str("  <D:response>\n");
        xml.push_str(&format!("    <D:href>{}</D:href>\n", xml_escape(&href)));
        xml.push_str("    <D:propstat>\n      <D:prop>\n");
        xml.push_str(&format!(
            "        <D:displayname>{}</D:displayname>\n",
            xml_escape(&display_name)
        ));
        xml.push_str(&format!(
            "        <D:getlastmodified>{mtime_http}</D:getlastmodified>\n"
        ));
        if is_dir {
            xml.push_str("        <D:resourcetype><D:collection/></D:resourcetype>\n");
        } else {
            let mime = guess_mime_type(&display_name);
            xml.push_str("        <D:resourcetype/>\n");
            xml.push_str(&format!(
                "        <D:getcontentlength>{}</D:getcontentlength>\n",
                meta.len()
            ));
            xml.push_str(&format!(
                "        <D:getcontenttype>{mime}</D:getcontenttype>\n"
            ));
        }
        xml.push_str("      </D:prop>\n      <D:status>HTTP/1.1 200 OK</D:status>\n    </D:propstat>\n  </D:response>\n");
    }

    xml.push_str("</D:multistatus>\n");

    send_simple_response(
        stream,
        "207 Multi-Status",
        "application/xml; charset=utf-8",
        xml.as_bytes(),
        &[("DAV", "1, 2"), ("MS-Author-Via", "DAV")],
    )
}

/// Streams a directory tree as an uncompressed standard PKZIP archive (`?zip=1`) on the fly.
fn stream_directory_as_zip(
    stream: &mut TcpStream,
    client_ip: &str,
    stop_flag: &AtomicBool,
    dir_path: &Path,
    rel_display: &str,
    tracker: &TransferTracker,
) -> std::io::Result<()> {
    let archive_base = if rel_display.is_empty() {
        "LabDrop-Files".to_string()
    } else {
        Path::new(rel_display)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "LabDrop-Folder".to_string())
    };
    let zip_filename = format!("{archive_base}.zip");
    let encoded_zip = percent_encode_path_segment(&zip_filename);

    let header = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: application/zip\r\n\
         Content-Disposition: attachment; filename=\"{}\"; filename*=UTF-8''{encoded_zip}\r\n\
         Connection: close\r\n\r\n",
        zip_filename.replace('"', "_")
    );
    stream.write_all(header.as_bytes())?;

    let label = if rel_display.is_empty() {
        "LabDrop-Files.zip (All Staged Files)".to_string()
    } else {
        format!("{rel_display}.zip")
    };
    let transfer_id = tracker.start_transfer(ProtocolKind::Http, client_ip, &label, None, true);

    struct CentralEntry {
        name_bytes: Vec<u8>,
        crc32: u32,
        size: u32,
        local_header_offset: u32,
    }

    let mut central_dir: Vec<CentralEntry> = Vec::new();
    let mut offset: u64 = 0;
    let mut buf = vec![0u8; 64 * 1024];
    let mut completed = true;

    for entry in WalkDir::new(dir_path).follow_links(false).into_iter().flatten() {
        if stop_flag.load(Ordering::Relaxed) {
            completed = false;
            break;
        }
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(dir_path) else {
            continue;
        };
        let rel_str = crate::staging::path_to_forward_slashes(rel);
        if rel_str.is_empty() {
            continue;
        }
        let Ok(mut f) = File::open(entry.path()) else {
            continue;
        };
        let Ok(meta) = f.metadata() else {
            continue;
        };
        if meta.len() > u32::MAX as u64 {
            continue;
        }
        let file_size = meta.len() as u32;

        // Compute CRC32 first so we can emit a standard non-data-descriptor Local File Header
        let mut crc = 0u32;
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            crc = crc32_update(crc, &buf[..n]);
        }
        f.seek(SeekFrom::Start(0))?;

        let name_bytes = rel_str.into_bytes();
        let local_header_offset = offset as u32;

        // Local file header (30 bytes + filename)
        let mut lfh = Vec::with_capacity(30 + name_bytes.len());
        lfh.extend_from_slice(&0x04034b50u32.to_le_bytes()); // signature
        lfh.extend_from_slice(&20u16.to_le_bytes()); // version needed (2.0)
        lfh.extend_from_slice(&0x0800u16.to_le_bytes()); // flags: bit 11 (UTF-8 filename)
        lfh.extend_from_slice(&0u16.to_le_bytes()); // compression: 0 (STORE)
        lfh.extend_from_slice(&0u16.to_le_bytes()); // mod time
        lfh.extend_from_slice(&0u16.to_le_bytes()); // mod date
        lfh.extend_from_slice(&crc.to_le_bytes());
        lfh.extend_from_slice(&file_size.to_le_bytes());
        lfh.extend_from_slice(&file_size.to_le_bytes());
        lfh.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        lfh.extend_from_slice(&0u16.to_le_bytes()); // extra len
        lfh.extend_from_slice(&name_bytes);

        if stream.write_all(&lfh).is_err() {
            completed = false;
            break;
        }
        offset += lfh.len() as u64;
        tracker.add_bytes(transfer_id, lfh.len() as u64);

        let mut remaining = file_size as usize;
        while remaining > 0 {
            let max_chunk = remaining.min(buf.len());
            let n = f.read(&mut buf[..max_chunk])?;
            if n == 0 {
                break;
            }
            if stream.write_all(&buf[..n]).is_err() {
                completed = false;
                break;
            }
            remaining -= n;
            offset += n as u64;
            tracker.add_bytes(transfer_id, n as u64);
        }
        if !completed {
            break;
        }

        central_dir.push(CentralEntry {
            name_bytes,
            crc32: crc,
            size: file_size,
            local_header_offset,
        });
    }

    if completed {
        let cd_start = offset as u32;
        let mut cd_bytes = Vec::new();
        for ce in &central_dir {
            cd_bytes.extend_from_slice(&0x02014b50u32.to_le_bytes()); // central file header sig
            cd_bytes.extend_from_slice(&20u16.to_le_bytes()); // version made by
            cd_bytes.extend_from_slice(&20u16.to_le_bytes()); // version needed
            cd_bytes.extend_from_slice(&0x0800u16.to_le_bytes()); // UTF-8 bit
            cd_bytes.extend_from_slice(&0u16.to_le_bytes()); // STORE
            cd_bytes.extend_from_slice(&0u16.to_le_bytes()); // time
            cd_bytes.extend_from_slice(&0u16.to_le_bytes()); // date
            cd_bytes.extend_from_slice(&ce.crc32.to_le_bytes());
            cd_bytes.extend_from_slice(&ce.size.to_le_bytes());
            cd_bytes.extend_from_slice(&ce.size.to_le_bytes());
            cd_bytes.extend_from_slice(&(ce.name_bytes.len() as u16).to_le_bytes());
            cd_bytes.extend_from_slice(&0u16.to_le_bytes()); // extra len
            cd_bytes.extend_from_slice(&0u16.to_le_bytes()); // comment len
            cd_bytes.extend_from_slice(&0u16.to_le_bytes()); // disk number
            cd_bytes.extend_from_slice(&0u16.to_le_bytes()); // internal attr
            cd_bytes.extend_from_slice(&0u32.to_le_bytes()); // external attr
            cd_bytes.extend_from_slice(&ce.local_header_offset.to_le_bytes());
            cd_bytes.extend_from_slice(&ce.name_bytes);
        }

        let cd_size = cd_bytes.len() as u32;
        let count = central_dir.len() as u16;
        // End of central directory record (22 bytes)
        cd_bytes.extend_from_slice(&0x06054b50u32.to_le_bytes());
        cd_bytes.extend_from_slice(&0u16.to_le_bytes());
        cd_bytes.extend_from_slice(&0u16.to_le_bytes());
        cd_bytes.extend_from_slice(&count.to_le_bytes());
        cd_bytes.extend_from_slice(&count.to_le_bytes());
        cd_bytes.extend_from_slice(&cd_size.to_le_bytes());
        cd_bytes.extend_from_slice(&cd_start.to_le_bytes());
        cd_bytes.extend_from_slice(&0u16.to_le_bytes());

        if stream.write_all(&cd_bytes).is_ok() {
            tracker.add_bytes(transfer_id, cd_bytes.len() as u64);
        } else {
            completed = false;
        }
    }

    let _ = stream.flush();
    tracker.finish_transfer(transfer_id, completed);
    Ok(())
}

fn crc32_update(crc: u32, data: &[u8]) -> u32 {
    let mut c = !crc;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            if (c & 1) != 0 {
                c = (c >> 1) ^ 0xEDB88320;
            } else {
                c >>= 1;
            }
        }
    }
    !c
}

fn render_directory_html(
    canonical_root: &Path,
    dir_path: &Path,
    rel_display: &str,
    sha_map: &HashMap<String, String>,
) -> std::io::Result<String> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir_path)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        let name = entry.file_name().to_string_lossy().into_owned();
        entries.push((meta.is_dir(), name, meta.len(), entry.path()));
    }
    // Directories first, then alphabetical case-insensitive
    entries.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase()))
    });

    let title_path = if rel_display.is_empty() {
        "/".to_string()
    } else {
        format!("/{rel_display}")
    };

    let mut html = format!(
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n\
         <meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>LabDrop — {title}</title>\n\
         <style>\n\
           body {{ font-family: system-ui, -apple-system, sans-serif; margin: 2rem auto; max-width: 960px; padding: 0 1rem; background: #0f172a; color: #f8fafc; }}\n\
           header {{ display: flex; justify-content: space-between; align-items: center; border-bottom: 1px solid #334155; padding-bottom: 1rem; margin-bottom: 1.5rem; flex-wrap: wrap; gap: 1rem; }}\n\
           h1 {{ margin: 0; font-size: 1.4rem; color: #38bdf8; }}\n\
           .sub {{ color: #94a3b8; font-size: 0.9rem; margin-top: 0.25rem; }}\n\
           .actions {{ display: flex; gap: 0.75rem; }}\n\
           .btn {{ background: #0284c7; color: #fff; text-decoration: none; padding: 0.5rem 0.9rem; border-radius: 6px; font-weight: 600; font-size: 0.88rem; }}\n\
           .btn-sec {{ background: #334155; color: #e2e8f0; }}\n\
           table {{ width: 100%; border-collapse: collapse; background: #1e293b; border-radius: 8px; overflow: hidden; }}\n\
           th, td {{ text-align: left; padding: 0.75rem 1rem; border-bottom: 1px solid #334155; font-size: 0.92rem; }}\n\
           th {{ background: #0f172a; color: #94a3b8; font-weight: 600; }}\n\
           a {{ color: #38bdf8; text-decoration: none; }}\n\
           a:hover {{ text-decoration: underline; }}\n\
           code {{ font-family: monospace; font-size: 0.8rem; color: #94a3b8; background: #0f172a; padding: 0.15rem 0.35rem; border-radius: 4px; }}\n\
         </style>\n</head>\n<body>\n\
         <header>\n\
           <div>\n\
             <h1>LabDrop File Distribution</h1>\n\
             <div class=\"sub\">Read-Only Laboratory Share • Path: <code>{title}</code></div>\n\
           </div>\n\
           <div class=\"actions\">\n\
             <a class=\"btn\" href=\"?zip=1\">⬇ Download Folder as .ZIP</a>\n\
             <a class=\"btn btn-sec\" href=\"/SHA256SUMS.txt\">SHA256SUMS.txt</a>\n\
           </div>\n\
         </header>\n\
         <table>\n\
           <thead><tr><th>Name</th><th>Size</th><th>SHA-256</th></tr></thead>\n\
           <tbody>\n",
        title = xml_escape(&title_path)
    );

    if !rel_display.is_empty() {
        let parent_href = match rel_display.rfind('/') {
            Some(idx) => format!(
                "/{}/",
                rel_display[..idx]
                    .split('/')
                    .map(percent_encode_path_segment)
                    .collect::<Vec<_>>()
                    .join("/")
            ),
            None => "/".to_string(),
        };
        html.push_str(&format!(
            "             <tr><td><a href=\"{parent_href}\">📁 .. (Parent Directory)</a></td><td>—</td><td>—</td></tr>\n"
        ));
    }

    for (is_dir, name, size, full_path) in entries {
        let item_rel = full_path
            .strip_prefix(canonical_root)
            .ok()
            .map(crate::staging::path_to_forward_slashes)
            .unwrap_or_else(|| name.clone());

        let href_segments = item_rel
            .split('/')
            .map(percent_encode_path_segment)
            .collect::<Vec<_>>()
            .join("/");

        if is_dir {
            html.push_str(&format!(
                "             <tr><td><a href=\"/{href_segments}/\">📁 {}/</a></td><td>Directory</td><td><a href=\"/{href_segments}/?zip=1\">Download .zip</a></td></tr>\n",
                xml_escape(&name)
            ));
        } else {
            let sha_short = sha_map
                .get(&item_rel)
                .map(|s| format!("<code>{}…</code>", &s[..16.min(s.len())]))
                .unwrap_or_else(|| "—".to_string());
            html.push_str(&format!(
                "             <tr><td><a href=\"/{href_segments}\" download>📄 {}</a></td><td>{}</td><td>{sha_short}</td></tr>\n",
                xml_escape(&name),
                format_bytes(size)
            ));
        }
    }

    html.push_str("           </tbody>\n         </table>\n</body>\n</html>\n");
    Ok(html)
}

fn send_simple_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
    extra_headers: &[(&str, &str)],
) -> std::io::Result<()> {
    let mut hdr = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n",
        body.len()
    );
    for (k, v) in extra_headers {
        hdr.push_str(&format!("{k}: {v}\r\n"));
    }
    hdr.push_str("\r\n");
    stream.write_all(hdr.as_bytes())?;
    if !body.is_empty() {
        stream.write_all(body)?;
    }
    stream.flush()
}

pub fn percent_encode_path_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(*b as char);
        } else {
            use std::fmt::Write;
            let _ = write!(&mut out, "%{b:02X}");
        }
    }
    out
}

pub fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return None;
            }
            let hi = (bytes[i + 1] as char).to_digit(16)?;
            let lo = (bytes[i + 2] as char).to_digit(16)?;
            out.push(((hi << 4) | lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn guess_mime_type(filename: &str) -> &'static str {
    let lower = filename.to_lowercase();
    if lower.ends_with(".txt") || lower.ends_with(".md") || lower.ends_with(".log") {
        "text/plain; charset=utf-8"
    } else if lower.ends_with(".html") || lower.ends_with(".htm") {
        "text/html; charset=utf-8"
    } else if lower.ends_with(".json") {
        "application/json"
    } else if lower.ends_with(".pdf") {
        "application/pdf"
    } else if lower.ends_with(".zip") {
        "application/zip"
    } else {
        "application/octet-stream"
    }
}

fn format_http_date(t: SystemTime) -> String {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Civil date calculation from Unix epoch (Howard Hinnant's algorithm)
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let hour = rem / 3600;
    let min = (rem % 3600) / 60;
    let sec = rem % 60;

    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };

    let wday = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"][(days.rem_euclid(7)) as usize];
    let mon = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ][(m - 1) as usize];

    format!("{wday}, {d:02} {mon} {year:04} {hour:02}:{min:02}:{sec:02} GMT")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use tempfile::TempDir;

    #[test]
    fn embedded_server_should_serve_http_get_range_zip_and_webdav_propfind_and_block_put() {
        let tmp = TempDir::new().unwrap();
        let sub = tmp.path().join("Sub Pasta 🧪");
        fs::create_dir_all(&sub).unwrap();
        fs::write(tmp.path().join("hello.txt"), b"0123456789ABCDEF").unwrap();
        fs::write(sub.join("nested.txt"), b"nested content").unwrap();

        let tracker = TransferTracker::shared();
        let mut srv = EmbeddedFileServer::start(
            Ipv4Addr::LOCALHOST,
            0,
            EmbeddedServerMode::WebDavAndHttp,
            tmp.path().to_path_buf(),
            None,
            Arc::clone(&tracker),
        )
        .unwrap();

        let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, srv.port);

        // 1. Test WebDAV PROPFIND on /LabDrop/
        {
            let mut s = TcpStream::connect(addr).unwrap();
            s.write_all(
                b"PROPFIND /LabDrop/ HTTP/1.1\r\nHost: localhost\r\nDepth: 1\r\n\r\n",
            )
            .unwrap();
            let mut resp = String::new();
            s.read_to_string(&mut resp).unwrap();
            assert!(resp.starts_with("HTTP/1.1 207 Multi-Status"));
            assert!(resp.contains("hello.txt"));
            assert!(resp.contains("Sub%20Pasta"));
        }

        // 2. Test HTTP Range GET
        {
            let mut s = TcpStream::connect(addr).unwrap();
            s.write_all(
                b"GET /hello.txt HTTP/1.1\r\nHost: localhost\r\nRange: bytes=4-9\r\n\r\n",
            )
            .unwrap();
            let mut resp = String::new();
            s.read_to_string(&mut resp).unwrap();
            assert!(resp.starts_with("HTTP/1.1 206 Partial Content"));
            assert!(resp.ends_with("456789"));
        }

        // 3. Test Read-Only enforcement (PUT must return 403 Forbidden)
        {
            let mut s = TcpStream::connect(addr).unwrap();
            s.write_all(
                b"PUT /hacked.txt HTTP/1.1\r\nHost: localhost\r\nContent-Length: 4\r\n\r\nevil",
            )
            .unwrap();
            let mut resp = String::new();
            s.read_to_string(&mut resp).unwrap();
            assert!(resp.starts_with("HTTP/1.1 403 Forbidden"));
            assert!(!tmp.path().join("hacked.txt").exists());
        }

        // 4. Test ZIP download (?zip=1)
        {
            let mut s = TcpStream::connect(addr).unwrap();
            s.write_all(b"GET /?zip=1 HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .unwrap();
            let mut raw = Vec::new();
            s.read_to_end(&mut raw).unwrap();
            let hdr_end = raw
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .unwrap()
                + 4;
            let zip_body = &raw[hdr_end..];
            // Starts with PK\x03\x04 local file header signature
            assert_eq!(&zip_body[..4], b"PK\x03\x04");
        }

        let snap = tracker.snapshot();
        assert!(snap.total_bytes_transferred > 0);
        assert!(snap.completed_transfer_count >= 2);

        srv.stop();
    }
}
