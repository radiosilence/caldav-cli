//! CalDAV client, GraphQL layer, and MCP server.
//!
//! This crate is both a CLI binary (`main.rs`) and a library. The library
//! surface exists so a hosted, multi-tenant MCP service can link the CalDAV
//! and GraphQL machinery directly rather than shelling out to the binary.

pub mod caldav;
pub mod commands;
pub mod config;
pub mod error;
pub mod mcp;
pub mod models;
pub mod util;
