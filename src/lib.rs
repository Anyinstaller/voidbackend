//! AIP v1 backend for Anyinstaller.
//!
//! The backend executes one installation session. It connects to the private
//! named pipe created by the stub, applies client settings, securely extracts
//! `payload.zip`, creates shortcuts, and reports exactly one
//! `installFinished` message.

#![cfg(windows)]

pub mod acl;
pub mod app;
pub mod cli;
pub mod config;
pub mod frame;
pub mod install;
pub mod log;
pub mod message;
pub mod path;
pub mod payload;
pub mod pipe;
pub mod shortcut;
pub mod winutil;

pub use app::run;
