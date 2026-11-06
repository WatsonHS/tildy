//! tildy core library. Module ownership for M1 parallel work is listed in
//! docs/protocol.md. `model` is the frozen shared vocabulary.

pub mod model;

pub mod agent;

pub mod config;
pub mod diagnostics;
pub mod import;
pub mod protocol;
pub mod providers;
pub mod render;
pub mod scheduler;
pub mod session;

pub mod width;
