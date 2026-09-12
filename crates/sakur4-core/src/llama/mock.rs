//! The mock backend, kept for backwards-compatible naming.
//!
//! The full in-process simulation lives in [`crate::llama::embedded`]; this
//! module re-exports it under the name the testkit and older references expect.
//! There is deliberately only one implementation: two divergent mocks is how a
//! test suite starts passing against behaviour the real server does not have.

pub use crate::llama::embedded::{EmbeddedBackend, NullBackend};
