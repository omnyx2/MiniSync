//! minisync — a tiny peer-to-peer folder sync (full mesh P2P + TLS).
//!
//! Usage:
//!   minisync [--gui] <folder> <listen_addr> [peer1_addr] ...
//!   minisync --gui                (uses saved settings from ~/.config/minisync/app.toml)

use anyhow::Result;
use rustls::{ClientConfig, ServerConfig};
use std::collections::HashMap;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU32, Ordering};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use minisync::catalog::Catalog;
use minisync::config::app::AppConfig;
use minisync::config::SyncConfig;
use minisync::engine::session::run_peer_session;
use minisync::engine::watch::watch_loop;
#[cfg(feature = "gui")]
use minisync::engine::GuiCommand;
use minisync::engine::{CrdtDocs, Seen, SyncEngine};
use minisync::net;
use minisync::net::discovery;
use minisync::net::peers::PeerRegistry;

/// TCP MSS clamp (0 = off). Set in lattice mode: the overlay relays traffic over
/// a path whose effective MTU (~1428) is below the tun MTU (1500), so full-size
/// 1500-byte segments black-hole — TLS handshakes (small) succeed but the larger
/// index/file messages stall. Capping our send MSS keeps every segment inside the
/// tunnel. Both peers clamp, so neither direction over-sends.
static MSS_CLAMP: AtomicU32 = AtomicU32::new(0);

/// Bind a TCP listener, clamping MSS *before* listen so the SYN-ACK advertises
/// the small MSS (and accepted sockets inherit it). Falls back to a plain
/// listener if the address can't be parsed as a socket addr.
fn tcp_listener(addr: &str) -> std::io::Result<TcpListener> {
    let sa: std::net::SocketAddr = match addr.parse() {
        Ok(sa) => sa,
        Err(_) => return TcpListener::bind(addr),
    };
    let domain = if sa.is_ipv4() {
        socket2::Domain::IPV4
    } else {
        socket2::Domain::IPV6
    };
    let sock = socket2::Socket::new(domain, socket2::Type::STREAM, Some(socket2::Protocol::TCP))?;
    sock.set_reuse_address(true)?;
    let mss = MSS_CLAMP.load(Ordering::Relaxed);
    if mss > 0 {
        let _ = sock.set_mss(mss);
    }
    sock.bind(&sa.into())?;
    sock.listen(128)?;
    Ok(sock.into())
}

/// Connect a TCP stream, clamping MSS *before* connect so our SYN advertises the
/// small MSS. Falls back to a plain connect if the address isn't a socket addr.
fn tcp_connect(addr: &str) -> std::io::Result<TcpStream> {
    let sa: std::net::SocketAddr = match addr.parse() {
        Ok(sa) => sa,
        Err(_) => return TcpStream::connect(addr),
    };
    let domain = if sa.is_ipv4() {
        socket2::Domain::IPV4
    } else {
        socket2::Domain::IPV6
    };
    let sock = socket2::Socket::new(domain, socket2::Type::STREAM, Some(socket2::Protocol::TCP))?;
    let mss = MSS_CLAMP.load(Ordering::Relaxed);
    if mss > 0 {
        let _ = sock.set_mss(mss);
    }
    sock.connect(&sa.into())?;
    Ok(sock.into())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    // Parse flags (--gui, --lattice[, --lattice-socket <path>]) and strip them,
    // leaving only positional args (folder, listen_addr, peers...).
    let mut gui_mode = false;
    let mut lattice_enabled = false;
    let mut lattice_socket = net::lattice::DEFAULT_SOCKET.to_string();
    let mut filtered_args: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--gui" => gui_mode = true,
            "--lattice" => lattice_enabled = true,
            "--lattice-socket" => {
                lattice_enabled = true;
                if i + 1 < args.len() {
                    lattice_socket = args[i + 1].clone();
                    i += 1;
                }
            }
            other => filtered_args.push(other),
        }
        i += 1;
    }
    // In lattice mode, clamp TCP MSS so segments fit the overlay's relayed path
    // MTU (avoids the large-message black-hole; see MSS_CLAMP).
    if lattice_enabled {
        MSS_CLAMP.store(1300, Ordering::Relaxed);
    }

    // Resolve settings: CLI args > saved AppConfig > show usage
    let (folder_str, listen_addr, peer_addrs) = if filtered_args.len() >= 3 {
        // CLI args provided
        let listen = filtered_args[2].to_string();
        let peers: Vec<String> = filtered_args[3..]
            .iter()
            .map(|s| s.to_string())
            .collect();

        // Save to app config for next time (preserve existing node_name + peer_id)
        let existing = AppConfig::load();
        let existing_name = existing.as_ref().map(|c| c.node_name.clone()).unwrap_or_default();
        let existing_peer_id = existing.as_ref().map(|c| c.peer_id.clone()).unwrap_or_default();
        let app_cfg = AppConfig {
            sync_folder: filtered_args[1].to_string(),
            listen_addr: listen.clone(),
            peers: peers.clone(),
            node_name: if existing_name.is_empty() {
                AppConfig::default().node_name
            } else {
                existing_name
            },
            peer_id: existing_peer_id,
        };
        if let Err(e) = app_cfg.save() {
            eprintln!("[minisync] warning: could not save app config: {e}");
        }

        (filtered_args[1].to_string(), listen, peers)
    } else if let Some(app_cfg) = AppConfig::load() {
        // No CLI args — use saved config
        if app_cfg.sync_folder.is_empty() {
            print_usage(&filtered_args[0]);
            std::process::exit(1);
        }
        println!(
            "[minisync] loaded settings from {}",
            AppConfig::config_path().display()
        );
        let listen = app_cfg.listen_addr.clone();
        let peers: Vec<String> = app_cfg.peers.clone();
        (app_cfg.sync_folder, listen, peers)
    } else {
        print_usage(&filtered_args[0]);
        std::process::exit(1);
    };

    std::fs::create_dir_all(&folder_str)?;
    let folder: PathBuf = std::fs::canonicalize(&folder_str)?;
    // 컴퓨터별로 고정된 peer_id(app.toml에 영속). 재시작해도 유지되어
    // PID 기반 id가 만들던 좀비 피어 연결 문제를 없앤다.
    let peer_id = AppConfig::load_or_create_peer_id();
    let node_name = AppConfig::load()
        .map(|c| c.node_name)
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| AppConfig::default().node_name);

    // TLS 설정: 자체서명 인증서 생성
    let (cert, key) = net::generate_self_signed()?;
    let server_cfg = net::server_config(cert, key)?;
    let client_cfg = net::client_config();
    println!("[minisync] TLS configured (self-signed)");

    let root = Arc::new(folder);
    let registry = Arc::new(PeerRegistry::new());
    let seen: Seen = Arc::new(Mutex::new(HashMap::new()));
    let docs: CrdtDocs = Arc::new(Mutex::new(HashMap::new()));

    // Load per-folder sync configuration
    let config = Arc::new(RwLock::new(SyncConfig::load(&root)));
    println!(
        "[minisync] config loaded: default_mode={:?}",
        config.read().unwrap().default_mode
    );

    // Create catalog
    let catalog = Catalog::new();
    minisync::catalog::store::load_catalog(&root, &catalog);

    // GUI channels (if --gui mode)
    #[allow(unused_variables)]
    let (gui_tx, gui_rx, engine_arc) = if gui_mode {
        let (etx, erx) = std::sync::mpsc::channel();
        let (ctx, crx) = std::sync::mpsc::channel();
        let engine = Arc::new(SyncEngine {
            root: Arc::clone(&root),
            peer_id: peer_id.clone(),
            node_name: node_name.clone(),
            registry: Arc::clone(&registry),
            seen: Arc::clone(&seen),
            docs: Arc::clone(&docs),
            config: Arc::clone(&config),
            catalog: catalog.clone(),
            gui_tx: Some(etx),
            gui_rx: Some(Mutex::new(crx)),
            evicting: Arc::new(Mutex::new(std::collections::HashSet::new())),
            repaint: std::sync::OnceLock::new(),
        });
        (Some(erx), Some(ctx), Some(engine))
    } else {
        (None, None, None)
    };

    println!(
        "[minisync] id={peer_id} name={node_name} listening on {listen_addr}, syncing {:?}{}",
        &*root,
        if gui_mode { " (GUI mode)" } else { "" }
    );
    println!("[minisync] peers to connect: {:?}", &peer_addrs);

    // 1) Watcher thread
    {
        let (reg, r, s, d, pid, nn, cfg, cat, eng) = (
            Arc::clone(&registry),
            Arc::clone(&root),
            Arc::clone(&seen),
            Arc::clone(&docs),
            peer_id.clone(),
            node_name.clone(),
            Arc::clone(&config),
            catalog.clone(),
            engine_arc.clone(),
        );
        std::thread::spawn(move || {
            if let Err(e) = watch_loop(reg, r, s, d, pid, nn, cfg, cat, eng) {
                eprintln!("[watcher] stopped: {e}");
            }
        });
    }

    // 1b) Catalog scanner thread: keep the unified catalog in sync with what's
    //     actually on disk so the GUI shows every file in the shared folder.
    {
        let (r, cfg, cat, eng) = (
            Arc::clone(&root),
            Arc::clone(&config),
            catalog.clone(),
            engine_arc.clone(),
        );
        std::thread::spawn(move || {
            minisync::engine::scan::catalog_scan_loop(r, cat, cfg, eng);
        });
    }

    // 2) Listener thread (inbound: TLS server role)
    {
        let listener = tcp_listener(&listen_addr)?;
        let (reg, r, s, d, pid, nn, scfg, ccfg, cfg, cat, eng) = (
            Arc::clone(&registry),
            Arc::clone(&root),
            Arc::clone(&seen),
            Arc::clone(&docs),
            peer_id.clone(),
            node_name.clone(),
            Arc::clone(&server_cfg),
            Arc::clone(&client_cfg),
            Arc::clone(&config),
            catalog.clone(),
            engine_arc.clone(),
        );
        std::thread::spawn(move || {
            for stream_result in listener.incoming() {
                match stream_result {
                    Ok(stream) => {
                        let addr = stream.peer_addr().ok();
                        println!("[minisync] inbound connection from {addr:?}");
                        let (reg2, r2, s2, d2, pid2, nn2, scfg2, ccfg2, cfg2, cat2, eng2) = (
                            Arc::clone(&reg),
                            Arc::clone(&r),
                            Arc::clone(&s),
                            Arc::clone(&d),
                            pid.clone(),
                            nn.clone(),
                            Arc::clone(&scfg),
                            Arc::clone(&ccfg),
                            Arc::clone(&cfg),
                            cat.clone(),
                            eng.clone(),
                        );
                        std::thread::spawn(move || {
                            if let Err(e) = run_peer_session(
                                stream, true, scfg2, ccfg2, reg2, r2, s2, d2, pid2, nn2, cfg2,
                                cat2, eng2,
                            ) {
                                eprintln!("[sync] inbound session ended: {e}");
                            }
                        });
                    }
                    Err(e) => eprintln!("[minisync] accept error: {e}"),
                }
            }
        });
    }

    // 3) Outbound connector threads (TLS client role)
    for addr in peer_addrs {
        let (reg, r, s, d, pid, nn, scfg, ccfg, cfg, cat, eng) = (
            Arc::clone(&registry),
            Arc::clone(&root),
            Arc::clone(&seen),
            Arc::clone(&docs),
            peer_id.clone(),
            node_name.clone(),
            Arc::clone(&server_cfg),
            Arc::clone(&client_cfg),
            Arc::clone(&config),
            catalog.clone(),
            engine_arc.clone(),
        );
        std::thread::spawn(move || {
            connect_with_retry(&addr, reg, r, s, d, pid, nn, scfg, ccfg, cfg, cat, eng);
        });
    }

    // 4) UDP auto-discovery: beacon broadcaster + listener (LAN peer discovery).
    // Disabled in lattice mode — on a shared LAN both paths would connect the
    // same peer pair, and the duplicate-session dedup churns (EOF flapping)
    // and breaks sync. In lattice mode the overlay is the sole discovery path.
    let listen_port: Option<u16> = listen_addr.rsplit(':').next().and_then(|p| p.parse().ok());
    match listen_port {
        Some(port) if !lattice_enabled => {
            // 4a) Beacon broadcaster
            {
                let (pid, nn) = (peer_id.clone(), node_name.clone());
                std::thread::spawn(move || {
                    discovery::beacon_broadcast_loop(pid, nn, port);
                });
            }
            // 4b) Beacon listener (auto-connects to discovered peers)
            {
                let (reg, r, s, d, pid, nn, scfg, ccfg, cfg, cat, eng) = (
                    Arc::clone(&registry),
                    Arc::clone(&root),
                    Arc::clone(&seen),
                    Arc::clone(&docs),
                    peer_id.clone(),
                    node_name.clone(),
                    Arc::clone(&server_cfg),
                    Arc::clone(&client_cfg),
                    Arc::clone(&config),
                    catalog.clone(),
                    engine_arc.clone(),
                );
                std::thread::spawn(move || {
                    discovery::beacon_listen_loop(
                        pid, reg, r, s, d, nn, scfg, ccfg, cfg, cat, eng, connect_with_retry,
                    );
                });
            }
        }
        Some(_) => {
            // lattice mode: overlay is the sole discovery path.
            println!("[discovery] LAN UDP discovery disabled (lattice mode)");
        }
        None => eprintln!(
            "[discovery] could not parse port from listen_addr '{listen_addr}', auto-discovery disabled"
        ),
    }

    // 4c) Lattice overlay discovery (opt-in via --lattice). Polls the lattice
    // daemon's health_check IPC and dials connected peers on their virtual IPs.
    // Requires all nodes to listen on the same port (peers are dialed at
    // <virtual_ip>:<this listen port>).
    if lattice_enabled {
        match listen_port {
            Some(port) => {
                let (reg, r, s, d, pid, nn, scfg, ccfg, cfg, cat, eng) = (
                    Arc::clone(&registry),
                    Arc::clone(&root),
                    Arc::clone(&seen),
                    Arc::clone(&docs),
                    peer_id.clone(),
                    node_name.clone(),
                    Arc::clone(&server_cfg),
                    Arc::clone(&client_cfg),
                    Arc::clone(&config),
                    catalog.clone(),
                    engine_arc.clone(),
                );
                let sock = lattice_socket.clone();
                std::thread::spawn(move || {
                    net::lattice::lattice_discovery_loop(
                        sock, port, pid, reg, r, s, d, nn, scfg, ccfg, cfg, cat, eng,
                        connect_with_retry,
                    );
                });
            }
            None => eprintln!(
                "[lattice] could not parse port from listen_addr '{listen_addr}', lattice discovery disabled"
            ),
        }
    }

    // 5) GUI or headless
    if gui_mode {
        #[cfg(feature = "gui")]
        {
            // Start GUI command processor thread
            let eng_for_cmds = engine_arc.clone().unwrap();
            let root_for_cmds = Arc::clone(&root);
            std::thread::spawn(move || {
                gui_command_loop(&eng_for_cmds, &root_for_cmds);
            });

            let bridge = minisync::gui::state::GuiBridge {
                events_rx: gui_tx.unwrap(),
                commands_tx: gui_rx.unwrap(),
                catalog: catalog.clone(),
                registry: Arc::clone(&registry),
                config: Arc::clone(&config),
                root: Arc::clone(&root),
                node_name: node_name.clone(),
                engine: engine_arc.clone().unwrap(),
            };
            if let Err(e) = minisync::gui::run_gui(bridge) {
                eprintln!("[gui] error: {e}");
            }
        }
        #[cfg(not(feature = "gui"))]
        {
            eprintln!(
                "[minisync] --gui requires the 'gui' feature. Build with: cargo build --features gui"
            );
            std::process::exit(1);
        }
    } else {
        // Headless mode: park main thread
        loop {
            std::thread::park();
        }
    }

    #[allow(unreachable_code)]
    Ok(())
}

fn print_usage(prog: &str) {
    eprintln!("minisync — tiny peer-to-peer folder sync (full mesh + TLS)\n");
    eprintln!("Usage:");
    eprintln!("  {prog} [--gui] [--lattice] <folder> <listen_addr> [peer1_addr] ...");
    eprintln!("  {prog} --gui                (uses saved settings)");
    eprintln!();
    eprintln!("  --lattice                  discover peers via the lattice VPN overlay");
    eprintln!("                             (dials connected peers on <virtual_ip>:<listen port>;");
    eprintln!("                             all nodes must share the same listen port)");
    eprintln!("  --lattice-socket <path>    lattice IPC socket (default /tmp/lattice.sock)");
    eprintln!();
    eprintln!("Settings are saved to: {}", AppConfig::config_path().display());
    eprintln!();
    eprintln!("Examples:");
    eprintln!("  {prog} ./data 0.0.0.0:9000 10.0.0.2:9001 10.0.0.3:9002");
    eprintln!("  {prog} --gui ~/Sync 0.0.0.0:9000");
    eprintln!("  {prog} --gui   # reuse last settings");
}

/// Process GUI commands in a dedicated thread.
#[cfg(feature = "gui")]
fn gui_command_loop(engine: &SyncEngine, root: &std::path::Path) {
    let rx = match &engine.gui_rx {
        Some(rx) => rx,
        None => return,
    };
    loop {
        let cmd = match rx.lock().unwrap().recv() {
            Ok(cmd) => cmd,
            Err(_) => break,
        };
        match cmd {
            GuiCommand::Download(path) => {
                println!("[gui] download requested: {path}");
                let owners = engine.catalog.owners_of(&path);
                if let Some(owner) = owners.first() {
                    if let Err(e) = engine.registry.send_to_remote(
                        owner,
                        &minisync::protocol::Message::DownloadRequest(path.clone()),
                    ) {
                        eprintln!("[gui] download request failed: {e}");
                    }
                } else {
                    eprintln!("[gui] no owner found for {path}");
                }
            }
            GuiCommand::RemoveLocal(path) => {
                // Selective-sync eviction: drop our local copy only. Mark it in
                // `evicting` FIRST so the watcher won't broadcast a delete, then
                // remove the file and downgrade the catalog to a remote reference.
                println!("[gui] remove-local requested: {path}");
                engine.evicting.lock().unwrap().insert(path.clone());
                let abs = root.join(&path);
                match std::fs::remove_file(&abs) {
                    Ok(()) => {
                        engine.catalog.evict_local(&path);
                        engine.seen.lock().unwrap().remove(&path);
                        engine.notify_gui(minisync::engine::EngineEvent::CatalogUpdated);
                    }
                    Err(e) => {
                        engine.evicting.lock().unwrap().remove(&path);
                        eprintln!("[gui] remove-local failed for {path}: {e}");
                    }
                }
            }
            GuiCommand::UpdateConfig(new_config) => {
                println!("[gui] config updated");
                new_config.save(root);
                *engine.config.write().unwrap() = new_config;
            }
            GuiCommand::Rescan => {
                println!("[gui] rescan requested");
            }
        }
    }
}

fn connect_with_retry(
    addr: &str,
    registry: Arc<PeerRegistry>,
    root: Arc<PathBuf>,
    seen: Seen,
    docs: CrdtDocs,
    peer_id: String,
    node_name: String,
    server_cfg: Arc<ServerConfig>,
    client_cfg: Arc<ClientConfig>,
    config: Arc<RwLock<SyncConfig>>,
    catalog: Catalog,
    engine: Option<Arc<SyncEngine>>,
) {
    const MAX_RETRIES: u32 = 5;
    const RETRY_DELAY: Duration = Duration::from_secs(2);

    for attempt in 1..=MAX_RETRIES {
        match tcp_connect(addr) {
            Ok(stream) => {
                println!("[minisync] connected to {addr}");
                if let Err(e) = run_peer_session(
                    stream, false, server_cfg, client_cfg, registry, root, seen, docs, peer_id,
                    node_name, config, catalog, engine,
                ) {
                    eprintln!("[sync] outbound session to {addr} ended: {e}");
                }
                return;
            }
            Err(e) => {
                if attempt < MAX_RETRIES {
                    println!(
                        "[minisync] connect to {addr} failed ({e}), retry {attempt}/{MAX_RETRIES}..."
                    );
                    std::thread::sleep(RETRY_DELAY);
                } else {
                    println!(
                        "[minisync] giving up on {addr} after {MAX_RETRIES} attempts (peer may connect to us)"
                    );
                }
            }
        }
    }
}
