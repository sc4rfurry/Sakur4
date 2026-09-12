//! Sakur4's MCP gateway, as a library.
//!
//! The `sakur4d` binary is a thin CLI over these modules; they are exposed as a
//! library so the tool surface can be driven by integration tests over a real
//! transport. That is not a formality — the bugs that reached a real harness were
//! transport bugs: one unconstrained output schema made a client reject the whole
//! tool catalog, and a log line on stdout corrupted the stdio channel. Neither is
//! visible to a handler-level unit test.
//!
//! # The surface
//!
//! [`tools::Sakur4Server`] implements MCP: 17 tools, 4 resources and 1 prompt,
//! targeting the 2026-07-28 revision. [`gateway`] serves it over either transport:
//!
//! | Transport | Who needs it |
//! |---|---|
//! | stdio | any client that can spawn a child process — the default, because that is every client |
//! | streamable HTTP | shared stores, remote harnesses, several sessions at once |
//!
//! # Running it
//!
//! ```
//! use sakur4d::gateway::Transport;
//!
//! // The transport parser accepts the forms an operator actually types.
//! assert_eq!(Transport::parse("stdio"), Transport::Stdio);
//! assert_eq!(Transport::parse("http"), Transport::Http("127.0.0.1:8765".into()));
//! assert_eq!(
//!     Transport::parse("http://10.0.0.5:9000/"),
//!     Transport::Http("10.0.0.5:9000".into())
//! );
//! assert_eq!(Transport::parse("0.0.0.0:1234"), Transport::Http("0.0.0.0:1234".into()));
//! ```
//!
//! # A detail that matters more than it looks
//!
//! Over stdio, stdout **is** the JSON-RPC channel. Anything written there that is
//! not a frame corrupts the session, and the client has no way to recover — there
//! is no reconnect for a child process. Diagnostics therefore go to stderr, and
//! [`cli::init_tracing`] pins the log writer accordingly. The stdio integration
//! tests fail loudly on any non-JSON line rather than skipping it, because that is
//! how the original bug was found.

pub mod cli;
pub mod gateway;
pub mod tools;
