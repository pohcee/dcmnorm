//! dcmnorm's own DICOM Upper Layer Protocol (PS3.8) implementation - association negotiation
//! and PDU framing - replacing `dicom-ul`. Purely transport-layer, same as the crate it replaces:
//! knows about PDUs and presentation-context negotiation, nothing about DIMSE command semantics
//! (that's `dcmnorm`'s own `dimse.rs`/`scp.rs`, unchanged by this crate).
//!
//! New, purpose-built code, not a mechanical port - like `crates/dcmnorm-object`, nothing else in
//! the dependency graph depends on `dicom-ul` (confirmed before starting: `dicom-encoding`/
//! `dicom-transfer-syntax-registry` don't need it, and it has no other dependent besides
//! `dcmnorm` itself), so there's no external-sibling constraint forcing a byte-identical port.
//! Scoped to exactly what `dcmnorm` uses: synchronous `TcpStream` only (no async, no TLS - neither
//! is used anywhere in this dependency graph), no `UserIdentityItem` association-level auth (never
//! sent or read). See `client.rs`/`server.rs`/`pdu.rs` for the specifics of what's simplified
//! relative to `dicom-ul` and why.

mod client;
mod conn;
mod error;
mod pdata;
pub mod pdu;
mod server;

pub use client::{ClientAssociation, ClientAssociationOptions};
pub use error::{Error, Result};
pub use pdata::{PDataReader, PDataWriter};
pub use server::{ServerAssociation, ServerAssociationOptions};

/// Association module, kept for import-path compatibility with dicom-ul-shaped call sites
/// (`dicom_ul::association::{client, server, Error}`).
pub mod association {
    pub use crate::error::Error;
    pub mod client {
        pub use crate::client::{ClientAssociation, ClientAssociationOptions};
    }
    pub mod server {
        pub use crate::server::{ServerAssociation, ServerAssociationOptions};
    }
}

pub(crate) const IMPLEMENTATION_CLASS_UID: &str = "2.25.1.dcmnorm.dimse";
pub(crate) const IMPLEMENTATION_VERSION_NAME: &str = concat!("DCMNORM_", env!("CARGO_PKG_VERSION"));
