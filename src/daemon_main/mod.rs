//! The daemon side of the `trellis` binary, split out of `main.rs` (0.2.x refactoring
//! spec, R17).
pub(super) mod connection;
pub(super) mod io;
pub(super) mod metrics_request;
pub(super) mod operation;
