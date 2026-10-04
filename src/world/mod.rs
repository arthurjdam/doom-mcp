//! Understanding the game: pure functions over engine state.
//!
//! - `observe`: what the model is told (status, threats, events, screenshots)
//! - `nav`: route planning over the level geometry, with prerequisites
//! - `map`: the ASCII map

pub mod map;
pub mod nav;
pub mod observe;
