// SPDX-License-Identifier: MIT OR Apache-2.0

//! mcp-serial-rs library crate.
//!
//! The binary in `src/main.rs` is a thin tokio bootstrap over the modules
//! declared here. Splitting `lib` and `bin` lets integration tests in
//! `tests/` link against the public surface without re-declaring modules.

#![deny(clippy::all)]

pub mod config;
pub mod errors;
pub mod mcp;
pub mod protocol;
pub mod serial;
pub mod tools;
