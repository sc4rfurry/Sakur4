//! Sakur4 test utilities.
//!
//! Three things live here, all of which exist so that the interesting behaviour
//! can be tested without a GPU, a model, or a network:
//!
//! * [`fixtures`] — synthetic repositories with known structure, so Repo Cortex
//!   and the impact query can be asserted against ground truth.
//! * [`fake_server`] — an in-process HTTP server that speaks llama.cpp's
//!   `/slots`, `/slots/{id}/save|restore|erase`, `/tokenize`, `/props`,
//!   `/metrics` and `/health` routes, with a real checkpoint ring whose wrap
//!   behaviour is configurable. This is what makes the Cache-Coherence Layer
//!   testable against the *HTTP* contract rather than only against its own trait.
//! * [`harness`] — a scripted agent session driver that turns a list of turns into
//!   episodes, so eviction and receipt behaviour can be exercised at scale.

pub mod fake_server;
pub mod fixtures;
pub mod harness;

pub use fake_server::{FakeLlamaServer, FakeServerConfig};
pub use fixtures::{FixtureRepo, FixtureSpec};
pub use harness::ScriptedSession;

/// A short unique suffix for temporary paths, without depending on `uuid`.
pub fn unique_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:x}{:x}", std::process::id(), nanos)
}
