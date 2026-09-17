//! ProxyGate — a small, uniform proxy pool with a CLI, a REST API and an HTTP
//! proxy gateway in front of it.
//!
//! ```text
//! Subscribers (http / file / exec)
//!         |
//!         v
//!     Normalizer  ->  Pool  ->  Checker
//!                       |  \
//!                       |   Selector
//!                       v
//!              CLI get / REST API / HTTP gateway
//! ```
//!
//! The whole point of the design is that from the moment a proxy enters the
//! pool, nothing cares where it came from.

pub mod api;
pub mod checker;
pub mod cli;
pub mod config;
pub mod error;
pub mod gateway;
pub mod model;
pub mod pool;
pub mod selector;
pub mod state;
pub mod subscriber;

pub use error::{Error, Result};
pub use model::Proxy;
pub use pool::ProxyPool;

/// Version string reported by the CLI, the API and logs.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
