//! Sakur4's MCP gateway, exposed as a library.
//!
//! The binary in `src/main.rs` is a thin CLI over these modules. They exist as a
//! library as well so the gateway's tool surface can be driven by integration
//! tests over a real HTTP listener — which is the only way to test the parts of an
//! MCP server that a unit test skips: JSON-RPC framing, protocol negotiation, and
//! the wire shape of each tool's arguments and results.

pub mod cli;
pub mod gateway;
pub mod tools;
