//! Shared pieces for ridgewarden and its apps.
//!
//! Every app (service health, Link Doctor, status page, ...) is its own program in its own
//! repo. They all talk to one core agent per site over its local API: they read its event
//! feed into their own SQLite views, and ask it to sign and log the events they produce.
//! Only the core polls the AREDN node and syncs with other sites.

pub mod alerts;
pub mod aredn;
pub mod core;
pub mod event;
pub mod graph;
pub mod mesh;
pub mod svg;
pub mod ui;
pub mod util;

pub use crate::core::{CoreClient, Projection, follow, open_db};
pub use crate::event::{Author, Event};
