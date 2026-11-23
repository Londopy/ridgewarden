//! ridgewarden core: the LAN-side agent every ridgewarden app runs on.
//!
//! It reads the AREDN node's HTTP API (never writes to the node), keeps a signed
//! append-only event log that merges cleanly after the mesh splits and heals, syncs it with
//! other sites, and gives apps a local API: an event feed to build views from, and a way to
//! log signed events of their own. The apps (service health, Link Doctor, status page, ...)
//! are separate programs in their own repos.

pub mod agent;
pub mod app;
pub mod config;
pub mod diff;
pub mod identity;
pub mod log;
pub mod metrics;
pub mod node;
pub mod store;
pub mod sync;
pub mod web;

pub use ridgewarden_client::{aredn, event, graph, util};
pub mod blobs;
pub mod invite;
pub mod qr;
pub mod trust;
pub mod update;
pub mod usage;
