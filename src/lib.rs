//! minisync — a tiny peer-to-peer folder sync library.
//!
//! Modules:
//!   - engine: core sync engine (session, handlers, watch, transfer)
//!   - config: sync mode rules and configuration
//!   - catalog: unified file catalog (local + remote)
//!   - net: TLS and peer registry
//!   - protocol: wire protocol and message types
//!   - crdt: Automerge CRDT bridge
//!   - index: file indexing and version vectors
//!   - routing: lane routing (CRDT vs File)
//!   - watcher: filesystem change detection

pub mod config;
pub mod catalog;
pub mod crdt;
pub mod engine;
pub mod index;
pub mod net;
pub mod protocol;
pub mod routing;
pub mod watcher;

#[cfg(feature = "gui")]
pub mod gui;
