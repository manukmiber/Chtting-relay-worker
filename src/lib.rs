//! Termux-native, stateful LLM relay.
//!
//! The binary in `main.rs` is a thin shell over this library so the integration
//! tests in `tests/` can drive the real servers rather than a stand-in.

pub mod config;
pub mod logging;
pub mod relay;
pub mod server;
pub mod state;
pub mod store;
pub mod tokenizer;
pub mod tunnel;
pub mod util;
