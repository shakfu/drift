//! `drift-client` — a graphical observer of the living galaxy (egui/eframe).
//!
//! This is a leaf crate: it depends on the simulation crates and never the
//! reverse, so the sim stays renderer-agnostic. It runs in one of two modes:
//!
//! - **in-process** (default): it drives a `drift-sim` [`Session`] and renders it;
//! - **networked** (`--connect <addr>`): it observes an authoritative
//!   `drift-server`, rendering the [`WorldView`](drift_proto::WorldView) broadcasts
//!   it receives.
//!
//! Either way the same [`app::DriftApp`] renders from a read-model; see [`app`].

mod app;
mod net;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use drift_sim::{load_registry, Session};

use crate::net::NetClient;

#[derive(Parser)]
#[command(name = "drift-client", about = "Graphical observer for the Drift galaxy")]
struct Args {
    #[arg(long, default_value = "mods/")]
    mods: PathBuf,
    #[arg(long, default_value = "scenarios/equilibrium.ron")]
    scenario: PathBuf,
    /// Override the scenario's seed (in-process mode only).
    #[arg(long)]
    seed: Option<u64>,
    /// Observe a running server at this address instead of simulating locally
    /// (e.g. `127.0.0.1:4000`). The `--mods` must match the server's content.
    #[arg(long)]
    connect: Option<String>,
}

fn main() -> Result<()> {
    let args = Args::parse();

    let app = if let Some(addr) = &args.connect {
        // Networked observer: load the same content the server runs (for static
        // map data), then connect and render its broadcasts.
        let reg = load_registry(&args.mods).context("loading content")?;
        let net = NetClient::connect(addr, reg.content_hash())
            .with_context(|| format!("connecting to {addr}"))?;
        app::DriftApp::remote(reg, net)
    } else {
        // In-process observer.
        let session = Session::load(&args.mods, &args.scenario, args.seed)
            .context("building session")?;
        let reg = session.registry_arc();
        app::DriftApp::local(reg, session)
    };

    let native_options = eframe::NativeOptions::default();
    eframe::run_native("Drift", native_options, Box::new(|_cc| Ok(Box::new(app))))
        .map_err(|e| anyhow::anyhow!("eframe error: {e}"))
}
