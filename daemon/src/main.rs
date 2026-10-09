//! OpenSilexLink daemon.
//!
//! Background service that speaks the Silex wire protocol and bridges it to the Linux
//! kernel's USB/IP stack (`vhci-hcd`). It owns device discovery, the connection/
//! reservation state, and a line-based JSON IPC surface consumed by the desktop client.
//!
//! Without a subcommand it runs in daemon mode. The `discover`/`probe`/`serve`
//! subcommands are bring-up helpers for checking the tunnel against real hardware.

mod daemon;
mod discovery;
mod ipc;
mod protocol;
mod settings;
mod usbip;
mod usbip_control;
mod wire;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Result};

use crate::daemon::Daemon;
use crate::ipc::IpcAddr;
use crate::protocol::udp_discovery as ud;
use crate::settings::SettingsStore;
use crate::usbip::proto::TCP_PORT;
use crate::usbip::server::UsbipServer;
use crate::usbip::session::{parse_device_descriptor, SilexSession};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--version") | Some("-V") => {
            println!("silex-daemon {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("discover") => cmd_discover(&args[2..]).await,
        Some("probe") => cmd_probe(&args[2..]).await,
        Some("serve") => cmd_serve(&args[2..]).await,
        _ => cmd_daemon(&args[1..]).await,
    }
}

/// Daemon mode: discovery + IPC server + orchestration.
/// `[--socket <path>] [--tcp <host:port>] [--escalate pkexec|sudo|none]`.
async fn cmd_daemon(args: &[String]) -> Result<()> {
    let escalation = get_flag(args, "--escalate").unwrap_or_else(|| "pkexec".to_string());
    let addr = if let Some(tcp) = get_flag(args, "--tcp") {
        let (host, port) = tcp.rsplit_once(':').ok_or_else(|| anyhow::anyhow!("--tcp needs host:port"))?;
        IpcAddr::Inet(if host.is_empty() { "127.0.0.1".to_string() } else { host.to_string() }, port.parse()?)
    } else if let Some(sock) = get_flag(args, "--socket") {
        IpcAddr::Unix(PathBuf::from(sock))
    } else {
        IpcAddr::Unix(crate::ipc::default_socket_path())
    };
    let store = Arc::new(SettingsStore::new(None));
    let daemon = Daemon::with_escalation(store, &escalation);
    tracing::info!("silex-daemon running (IPC {addr:?}, escalation {escalation})");
    daemon.run(addr).await
}

/// `discover [--timeout <secs>]`: find Silex device servers on the LAN.
async fn cmd_discover(args: &[String]) -> Result<()> {
    let secs: f64 = get_flag(args, "--timeout").and_then(|s| s.parse().ok()).unwrap_or(3.0);
    let broadcast = get_flag(args, "--broadcast").unwrap_or_else(|| "255.255.255.255".to_string());
    let servers = discovery::discover(std::time::Duration::from_secs_f64(secs), &broadcast).await;
    if servers.is_empty() {
        println!("No device servers found.");
        return Ok(());
    }
    for srv in servers {
        println!(
            "{}  [{}  {}]  {}  {}",
            if srv.hostname.is_empty() { "?" } else { &srv.hostname },
            srv.mac_str(),
            srv.model,
            srv.ip,
            srv.version
        );
        for d in &srv.devices {
            println!(
                "    {}  {:?}  USB {:04x}:{:04x}  {}",
                d.dev_id, d.description, d.id_vendor, d.id_product, d.device_type()
            );
        }
    }
    Ok(())
}

/// `probe <server_ip> <dev_id> --mac <mac>`: connect and read the device descriptor.
async fn cmd_probe(args: &[String]) -> Result<()> {
    let pos = positionals(args);
    if pos.len() < 2 {
        bail!("usage: probe <server_ip> <dev_id> --mac <mac>");
    }
    let server_ip = &pos[0];
    let dev_id = &pos[1];
    let mac = parse_mac(&get_flag(args, "--mac").ok_or_else(|| anyhow::anyhow!("--mac required"))?)?;

    let sess = SilexSession::new(server_ip, mac, dev_id, None, ud::PORT, std::time::Duration::ZERO);
    println!("CONNECT {server_ip} {dev_id} (host ip {}) ...", sess.host_ip);
    sess.connect().await?;
    let result = async {
        let desc = sess.get_device_descriptor().await.map_err(|e| anyhow::anyhow!("{e}"))?;
        println!("  device descriptor: {}", hex::encode(&desc));
        let info = parse_device_descriptor(&desc)?;
        println!("  idVendor:idProduct = {:04x}:{:04x}", info.id_vendor, info.id_product);
        println!(
            "  bcdUSB={:04x}  bcdDevice={:04x}  Class={:02x}/{:02x}/{:02x}  MaxPacket0={}  Configs={}",
            info.bcd_usb,
            info.bcd_device,
            info.b_device_class,
            info.b_device_subclass,
            info.b_device_protocol,
            info.b_max_packet_size0,
            info.b_num_configurations
        );
        Ok::<(), anyhow::Error>(())
    }
    .await;
    sess.disconnect().await;
    println!("DISCONNECT ok.");
    result
}

/// `serve <server_ip> <dev_id...> --mac <mac> [--bind <ip>] [--host-ip <ip>]`.
async fn cmd_serve(args: &[String]) -> Result<()> {
    let pos = positionals(args);
    if pos.len() < 2 {
        bail!("usage: serve <server_ip> <dev_id...> --mac <mac> [--bind <ip>] [--host-ip <ip>]");
    }
    let server_ip = &pos[0];
    let mac = parse_mac(&get_flag(args, "--mac").ok_or_else(|| anyhow::anyhow!("--mac required"))?)?;
    let bind = get_flag(args, "--bind").unwrap_or_else(|| "0.0.0.0".to_string());
    let host_ip = get_flag(args, "--host-ip");
    let devices: Vec<(String, String)> = pos[1..]
        .iter()
        .enumerate()
        .map(|(i, d)| (format!("1-{}", i + 1), d.clone()))
        .collect();

    let server = Arc::new(UsbipServer::new(
        server_ip,
        mac,
        host_ip,
        (bind, TCP_PORT),
        ud::PORT,
        devices,
    ));
    let srv = server.clone();
    tokio::select! {
        r = srv.serve() => { r?; }
        _ = tokio::signal::ctrl_c() => {
            println!("\nshutting down - disconnecting active sessions ...");
        }
    }
    server.shutdown().await;
    Ok(())
}

// --------------------------------------------------------------------------- //
//  Tiny argument helpers
// --------------------------------------------------------------------------- //
fn get_flag(args: &[String], name: &str) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == name {
            return it.next().cloned();
        }
    }
    None
}

fn positionals(args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i].starts_with("--") {
            i += 2; // skip flag and its value
        } else {
            out.push(args[i].clone());
            i += 1;
        }
    }
    out
}

fn parse_mac(s: &str) -> Result<[u8; 6]> {
    let raw = s.replace(':', "");
    let b = hex::decode(raw).map_err(|_| anyhow::anyhow!("invalid MAC hex"))?;
    if b.len() != 6 {
        bail!("MAC must be 6 bytes");
    }
    let mut m = [0u8; 6];
    m.copy_from_slice(&b);
    Ok(m)
}
