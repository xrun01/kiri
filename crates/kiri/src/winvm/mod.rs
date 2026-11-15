//! Windows 11 ARM64 virtual machine controller backed by `vm-mcp`.
//!
//! Manages configuration (`~/.kiri/windows/config.json`), sparse disk images,
//! APFS copy-on-write snapshots (`clonefile`), SmartGuard rotation, shared
//! macOS profile folders, and persistent JSON-RPC communication with `vm-mcp`.

mod actions;
mod config;
mod session;
mod snapshot;

pub(crate) use actions::{cli, handle_action, snapshot_create, snapshot_delete, snapshots_list, status_json};

#[cfg(test)]
mod tests;
