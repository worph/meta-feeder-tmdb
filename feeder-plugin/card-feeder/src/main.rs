//! `card-feeder` — metadata-bridge feeder sidecar (the card tier).
//!
//! Hosts [`tmdb::TmdbCardPlugin`] over the feeder HTTP contract via
//! [`meta_feeder_sdk::serve_feeders`]. The gateway core reaches it
//! server-to-server on the internal docker network.
//!
//! Env:
//! - `META_FEEDER_HTTP_LISTEN` — listen addr (default `0.0.0.0:8080`)
//! - `META_FEEDER_STATE_DIR`   — per-plugin cache root (default `/data/meta-feeder`)
//! - `TMDB_TOKEN`              — TMDB v4 read token (seed; the config page wins)
//! - `TMDB_RATE_PER_SEC`, `TMDB_BURST`, `CARD_TOP_N` — optional seeds
//! - `RUST_LOG`                — tracing filter (default `info`)

use std::net::SocketAddr;

use card_feeder::tmdb::TmdbCardPlugin;
use meta_feeder_sdk::serve_feeders;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let listen: SocketAddr = std::env::var("META_FEEDER_HTTP_LISTEN")
        .unwrap_or_else(|_| "0.0.0.0:8080".to_string())
        .parse()?;
    let state_dir =
        std::env::var("META_FEEDER_STATE_DIR").unwrap_or_else(|_| "/data/meta-feeder".to_string());

    serve_feeders(vec![Box::new(TmdbCardPlugin::new())], state_dir, listen).await
}
