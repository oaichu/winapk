//! winapk-core: shared config, paths, process + ADB plumbing.
//!
//! Everything here is transport-agnostic and side-effect-light: the engine
//! layer decides *when* to spawn things; this layer knows *how* to spawn
//! them without leaking consoles (the upstream conhost problem is solved at
//! the process-creation level, not by hiding windows afterwards).

pub mod adb;
pub mod config;
pub mod error;
pub mod log;
pub mod paths;
pub mod proc;

pub use config::{Config, EngineProfile, GpuMode};
pub use error::{Error, Result};
pub use paths::Paths;
