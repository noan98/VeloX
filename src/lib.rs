//! VeloX — a fast, lightweight web browser built with Rust.
//!
//! The crate is split into layers so that each can grow independently:
//!
//! - [`ui`] — window management and the toolbar (chrome) rendering
//! - [`browser`] — browser logic: navigation, per-tab state, visit history
//!   and bookmarks (collection logic plus their JSON persistence)
//! - [`config`] — startup configuration
//! - [`app`] — glues the layers together and runs the event loop

pub mod app;
pub mod browser;
pub mod config;
pub mod ui;
