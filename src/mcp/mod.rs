//! The local MCP (Model Context Protocol) server.
//!
//! Wisp is two processes for MCP purposes:
//!
//! - the GUI app hosts a loopback endpoint ([`endpoint`]) because that is where
//!   nginx, PHP workers, ports and project state live;
//! - `wisp --mcp` runs the stdio MCP server ([`stdio`]) that a client launches.
//!   It forwards each call to the app and starts the app if it is not running.
//!
//! [`protocol`] is the contract between them, and [`snippets`] generates the
//! per-client configuration shown in Settings.

pub mod endpoint;
pub mod protocol;
pub mod snippets;
pub mod stdio;
