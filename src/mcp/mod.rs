//! The MCP front-ends. They define tools, validate arguments, run work on
//! the game thread and format results; game logic lives in `game`, `pilot`
//! and `world`.
//!
//! - `turn`: turn-based play (the model controls every action)
//! - `realtime`: real-time play (the model commands, the pilot plays)
//! - `common`: what both share

pub mod common;
pub mod realtime;
pub mod turn;
