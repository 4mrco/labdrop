use labdrop_core::{
    bootstrapper::{
        compute_file_sha256, prepare_usb_package, verify_linux_elf_x86_64, verify_windows_pe_x86_64,
        ArtifactSource,
    },
    http_webdav::{EmbeddedFileServer, EmbeddedServerMode},
    network::NetworkInterface,
    session::{ActiveSharingSession, SharingStrategy},
    smb::{probe_smb_environment, SmbLinuxMode},
    staging::{stage_selection, SelectionSet},
    transfer::TransferTracker,
};
use std::{
    fs,
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddrV4, TcpStream},
    process::Command,
    sync::Arc,
};
use tempfile::TempDir;

fn loopback_iface() -> NetworkInterface {
    NetworkInterface {
        name: "lo".to_string(),
        ipv4: Ipv4Addr::LOCALHOST,
        prefix_len: 8,
        subnet_cidr: "127.0.0.0/8".to_string(),
        is_loopback: true,
        is_wireless: false,
        is_virtual: false,
        is_default_route: false,
    }
}

#[test]
fn e2e_large_file_unicode_duplicate_staging_and_layered_webdav_http_session() {
    let tmp = TempDir::new().unwrap();
    let usb_source = tmp.path().join("usb_files");
    let downloads_source = tmp.path().join("Downloads");
    fs::create_dir_all(&usb_source).unwrap();
    fs::create_dir_all(&downloads_source).unwrap();

    // 1. Create a 10 MiB large installer payload
    let large_installer = usb_source.join("lab_installer_x64.bin");
    let large_payload: Vec<u8> = (0..(10 * 1024 * 1024))
        .map(|i| ((i * 31 + 7) & 0xFF) as u8)
        .collect();
    fs::write(&large_installer, &large_payload).unwrap();

    // 2. Create duplicate filename in Downloads
    let duplicate_installer = downloads_source.join("lab_installer_x64.bin");
    fs::write(&duplicate_installer, b"secondary installer payload").unwrap();

    // 3. Create nested folder with spaces and Unicode characters
    let unicode_dir = usb_source
        .join("Apostila de Redes & Segurança 2026 🧪")
        .join("Exercícios Práticos — Módulo 1");
    fs::create_dir_all(&unicode_dir).unwrap();
    let unicode_file = unicode_dir.join("Configuração_Roteador_日本語.txt");
    fs::write(&unicode_file, "Conteúdo em UTF-8 com acentuação e símbolos 🧪\n").unwrap();

    let mut sel = SelectionSet::new();
    sel.add_path(&large_installer).unwrap();
    sel.add_path(&duplicate_installer).unwrap();
    sel.add_path(&usb_source.join("Apostila de Redes & Segurança 2026 🧪"))
        .unwrap();

    let session_dir = tmp.path().join("session_e2e");
    let staged_dir = session_dir.join("staged");
    let manifest = stage_selection("e2e-01", &sel, &staged_dir, None, None, |_| {}).unwrap();

    assert_eq!(manifest.total_files, 3);
    assert!(staged_dir.join("lab_installer_x64.bin").is_file());
    assert!(staged_dir.join("lab_installer_x64 (2).bin").is_file());

    // Start WebDAV + HTTP session
    let iface = loopback_iface();
    let mut session = ActiveSharingSession::start(
        manifest,
        session_dir,
        &iface,
        SharingStrategy::WebDavSecondary,
        None,
        0,
        false,
    )
    .unwrap();

    let http_endpoint = session
        .ledger
        .endpoints
        .iter()
        .find(|e| e.protocol.starts_with("HTTP (Fallback)"))
        .unwrap();
    let port: u16 = http_endpoint
        .address
        .trim_start_matches("http://127.0.0.1:")
        .trim_end_matches('/')
        .parse()
        .unwrap();

    // Verify downloading the 10 MiB file over HTTP matches SHA-256
    let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .write_all(b"GET /lab_installer_x64.bin HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut raw_resp = Vec::new();
    stream.read_to_end(&mut raw_resp).unwrap();
    let body_start = raw_resp
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap()
        + 4;
    assert_eq!(&raw_resp[body_start..], large_payload.as_slice());

    // Stop session with retain_staged = true first to verify retention option
    let retain_report = session.stop_and_cleanup(false);
    assert!(!retain_report.staged_files_deleted);
    assert!(staged_dir.join("lab_installer_x64.bin").is_file());
}

#[test]
fn e2e_linux_samba_smb_share_read_only_and_cleanup() {
    #[cfg(unix)]
    {
        let env = probe_smb_environment();
        if !env.smbd_installed || !env.rootless_unshare_available {
            return;
        }
        let has_smbclient = Command::new("smbclient")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !has_smbclient {
            return;
        }

        let tmp = TempDir::new().unwrap();
        let src_dir = tmp.path().join("src");
        fs::create_dir_all(&src_dir).unwrap();
        let file_a = src_dir.join("Relatório Final 2026.txt");
        fs::write(&file_a, b"Conteudo distribuido via Samba SMB!\n").unwrap();

        let mut sel = SelectionSet::new();
        sel.add_path(&file_a).unwrap();

        let session_dir = tmp.path().join("smb_session");
        let staged_dir = session_dir.join("staged");
        let manifest =
            stage_selection("smb-e2e", &sel, &staged_dir, None, None, |_| {}).unwrap();

        let iface = loopback_iface();
        let smb_port = 14445u16;
        let mut session = ActiveSharingSession::start(
            manifest,
            session_dir.clone(),
            &iface,
            SharingStrategy::SmbOnly,
            Some(SmbLinuxMode::RootlessPort(smb_port)),
            0,
            false,
        )
        .expect("should start rootless Samba SMB share");

        // 1. Download file with real smbclient
        let downloaded = tmp.path().join("downloaded_smb.txt");
        let get_cmd = format!(
            "get \"Relatório Final 2026.txt\" \"{}\"",
            downloaded.display()
        );
        let out = Command::new("smbclient")
            .args([
                "-N",
                "-p",
                &smb_port.to_string(),
                "//127.0.0.1/LabDrop",
                "-c",
                &get_cmd,
            ])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "smbclient get failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            fs::read(&downloaded).unwrap(),
            b"Conteudo distribuido via Samba SMB!\n"
        );

        // 2. Verify Read-Only enforcement: attempting to upload (`put`) or delete (`del`) must fail!
        let put_cmd = format!("put \"{}\" hacked.txt", downloaded.display());
        let put_out = Command::new("smbclient")
            .args([
                "-N",
                "-p",
                &smb_port.to_string(),
                "//127.0.0.1/LabDrop",
                "-c",
                &put_cmd,
            ])
            .output()
            .unwrap();
        assert!(!put_out.status.success());
        assert!(!staged_dir.join("hacked.txt").exists());

        let del_out = Command::new("smbclient")
            .args([
                "-N",
                "-p",
                &smb_port.to_string(),
                "//127.0.0.1/LabDrop",
                "-c",
                "del \"Relatório Final 2026.txt\"",
            ])
            .output()
            .unwrap();
        let del_combined = format!(
            "{}{}",
            String::from_utf8_lossy(&del_out.stdout),
            String::from_utf8_lossy(&del_out.stderr)
        );
        assert!(
            !del_out.status.success() || del_combined.contains("NT_STATUS_"),
            "Expected SMB delete rejection, got: {del_combined}"
        );
        assert!(staged_dir.join("Relatório Final 2026.txt").exists());

        // 3. Stop session and verify daemon is terminated and staged files deleted
        let report = session.stop_and_cleanup(true);
        assert!(report.staged_files_deleted);
        assert!(!session_dir.exists());
        assert!(
            TcpStream::connect(SocketAddrV4::new(Ipv4Addr::LOCALHOST, smb_port)).is_err(),
            "Samba port should be closed after session cleanup"
        );
    }
}

#[test]
fn e2e_bootstrapper_online_http_download_and_checksum_verification() {
    let tmp = TempDir::new().unwrap();
    let release_srv_dir = tmp.path().join("release_server");
    fs::create_dir_all(&release_srv_dir).unwrap();

    // Create valid synthetic ELF x86-64 and PE32+ x86-64 binaries
    let mut elf = vec![0u8; 512];
    elf[0..4].copy_from_slice(b"\x7fELF");
    elf[4] = 2;
    elf[5] = 1;
    elf[6] = 1;
    elf[16..18].copy_from_slice(&2u16.to_le_bytes());
    elf[18..20].copy_from_slice(&0x3Eu16.to_le_bytes());

    let mut pe = vec![0u8; 512];
    pe[0..2].copy_from_slice(b"MZ");
    pe[60..64].copy_from_slice(&0x80u32.to_le_bytes());
    pe[128..132].copy_from_slice(b"PE\0\0");
    pe[132..134].copy_from_slice(&0x8664u16.to_le_bytes());

    let srv_linux = release_srv_dir.join("labdrop");
    let srv_win = release_srv_dir.join("LabDrop.exe");
    fs::write(&srv_linux, &elf).unwrap();
    fs::write(&srv_win, &pe).unwrap();

    let (l_sha, _) = compute_file_sha256(&srv_linux).unwrap();
    let (w_sha, _) = compute_file_sha256(&srv_win).unwrap();
    fs::write(
        release_srv_dir.join("SHA256SUMS.txt"),
        format!("{l_sha}  labdrop\n{w_sha}  LabDrop.exe\n"),
    )
    .unwrap();

    let tracker = TransferTracker::shared();
    let mut srv = EmbeddedFileServer::start(
        Ipv4Addr::LOCALHOST,
        0,
        EmbeddedServerMode::HttpOnly,
        release_srv_dir,
        None,
        Arc::clone(&tracker),
    )
    .unwrap();

    let usb_dest = tmp.path().join("USB_ONLINE_TEST");
    let base_url = format!("http://127.0.0.1:{}", srv.port);
    let report = prepare_usb_package(
        &usb_dest,
        &ArtifactSource::ReleaseBaseUrl(base_url),
        |_| {},
    )
    .expect("online bootstrapper download should succeed");

    verify_linux_elf_x86_64(&report.linux_binary_path).unwrap();
    verify_windows_pe_x86_64(&report.windows_binary_path).unwrap();
    assert_eq!(report.linux_binary_sha256, l_sha);
    assert_eq!(report.windows_binary_sha256, w_sha);

    srv.stop();
}
