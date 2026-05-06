//! Stem Player USB protocol implementation.
//!
//! All knowledge in here was reverse-engineered from a live capture of
//! `stem1.stemplayer.com/connect/stem` performing a firmware update against
//! a real device on 2026-05-05. See `../stemplayer_protocol_findings.md`
//! in the repo root for the full write-up.

pub mod commands;
pub mod device;
pub mod error;
pub mod files;
pub mod frame;

#[allow(unused_imports)]
pub use error::{Error, Result};
