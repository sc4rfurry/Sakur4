//! Sakur4 core library.
//!
//! Sakur4 is a cache-coherent memory and context operating system for local
//! coding/research agents. This crate holds every component that is independent
//! of the MCP transport:
//!
//! * [`memory`] — the Memory Fabric (C1): dual-track, multi-tier store.
//! * [`evict`] — the Graduated Eviction Engine (C2).
//! * [`cache`] — the Cache-Coherence Layer (C3), the project's central bet.
//! * [`repo`] — Repo Cortex (C4): deterministic structural code intelligence.
//! * [`recall`] — the Hybrid Recall Engine (C5).
//! * [`consolidate`] — the Idle Consolidator / Dream Cycle (C6).
//! * [`receipt`] — the Context Ledger Receipt (C8).
//!
//! The MCP Gateway (C7) lives in the `sakur4d` binary and is a thin adapter over
//! [`Engine`].

pub mod cache;
pub mod consolidate;
pub mod embed;
pub mod error;
pub mod evict;
pub mod ids;
pub mod llama;
pub mod memory;
pub mod prompt;
pub mod provider_cache;
pub mod recall;
pub mod receipt;
pub mod repo;
pub mod store;
pub mod tokens;

mod engine;

pub use engine::{Engine, EngineConfig};

/// The MCP protocol revision Sakur4 targets.
pub const MCP_PROTOCOL_VERSION: &str = "2026-07-28";

/// Schema version written into the `meta` table by the newest migration.
pub const SCHEMA_VERSION: i64 = 1;
