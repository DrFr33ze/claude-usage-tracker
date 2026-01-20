#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![deny(unsafe_code)]
#![warn(clippy::pedantic)]

//! Claude Usage Tracker - Binary entry point
//!
//! This binary is a thin wrapper around the `claude_usage_tracker` library.
//! All application logic is in the library crate.

use anyhow::Result;
use claude_usage_tracker::run;

fn main() -> Result<()> {
    run()
}
