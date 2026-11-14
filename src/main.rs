//! skytrace — lightweight live flight tracker.

// Release builds on Windows are GUI apps: no console window.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod config;
mod map;
mod model;
mod sources;
mod ui;

use anyhow::{Context, Result};
use std::time::Duration;

fn main() -> Result<()> {
    // rustls needs an explicit crypto provider (ring: no C toolchain needed on Windows).
    let _ = rustls::crypto::ring::default_provider().install_default();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .context("starting async runtime")?;
    let client = reqwest::Client::builder()
        .user_agent(config::USER_AGENT)
        .connect_timeout(Duration::from_secs(8))
        .timeout(Duration::from_secs(20))
        .build()
        .context("building HTTP client")?;
    ui::run(&rt, client)
}
