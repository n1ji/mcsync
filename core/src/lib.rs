//! mcsync core: launcher adapters, pack manifests, mod resolution, the pack
//! git repo, and world sync against the mcsync server.

pub mod client;
pub mod config;
pub mod error;
pub mod git;
pub mod hashing;
pub mod launcher;
pub mod manifest;
pub mod pack;
pub mod process;
pub mod resolve;
pub mod scope;
pub mod state;
pub mod sync;
pub mod world;

pub use error::{Error, Result};
