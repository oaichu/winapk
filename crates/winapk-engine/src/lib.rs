//! winapk-engine: runtime provisioning, AVD authoring, emulator launch,
//! lifecycle FSM, and Win32 host tuning.

pub mod accel;
pub mod avd;
pub mod control;
pub mod engine;
pub mod fsm;
pub mod gpu;
pub mod provision;
pub mod qmp;
pub mod tuning;
pub mod userdata;

pub use engine::Engine;
pub use fsm::VmState;
