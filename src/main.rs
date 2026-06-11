//! minisync — a tiny peer-to-peer folder sync (full mesh P2P + TLS).
//!
//! Usage:
//!   minisync [--gui] <folder> <listen_addr> [peer1_addr] ...
//!   minisync --gui                (uses saved settings from ~/.config/minisync/app.toml)

use anyhow::Result;
use rustls::{ClientConfig, ServerConfig};
use std::collections::hash_map::RandomState;
use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::net::{TcpListener, TcpStream};
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
use minisync::net::peers::PeerRegistry;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    // Parse --gui flag
    let gui_mode = args.iter().any(|a| a == "--gui");
    let filtered_args: Vec<&str> = args
        .iter()
        .map(|s| s.as_str())
        .filter(|a| *a != "--gui")
        .collect();

    // Resolve settings: CLI args > saved AppConfig > show usage
    let (folder_str, listen_addr, peer_addrs) = if filtered_args.len() >= 3 {
        // CLI args provided
        let listen = filtered_args[2].to_string();
        let peers: Vec<String> = filtered_args[3..]
            .iter()
            .filter(|a| **a > listen.as_str())
            .map(|s| s.to_string())
            .collect();

        // Save to app config for next time (preserve existing node_name)
        let existing_name = AppConfig::load().map(|c| c.node_name).unwrap_or_default();
        let app_cfg = AppConfig {
            sync_folder: filtered_args[1].to_string(),
            listen_addr: listen.clone(),
            peers: peers.clone(),
            node_name: if existing_name.is_empty() {
                AppConfig::default().node_name
            } else {
                existing_name
            },
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
        let peers: Vec<String> = app_cfg
            .peers
            .iter()
            .filter(|a| a.as_str() > listen.as_str())
            .cloned()
            .collect();
        (app_cfg.sync_folder, listen, peers)
    } else {
        print_usage(&filtered_args[0]);
        std::process::exit(1);
    };

    std::fs::create_dir_all(&folder_str)?;
    let folder: PathBuf = std::fs::canonicalize(&folder_str)?;
    let peer_id = generate_peer_id();
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

    // 2) Listener thread (inbound: TLS server role)
    {
        let listener = TcpListener::bind(&listen_addr)?;
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

    // 4) GUI or headless
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
    eprintln!("  {prog} [--gui] <folder> <listen_addr> [peer1_addr] ...");
    eprintln!("  {prog} --gui                (uses saved settings)");
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
        match TcpStream::connect(addr) {
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

/// 인스턴스별 고유 ID (8자리 hex).
fn generate_peer_id() -> String {
    let mut h = RandomState::new().build_hasher();
    h.write_u32(std::process::id());
    format!("{:08x}", h.finish() as u32)
}
