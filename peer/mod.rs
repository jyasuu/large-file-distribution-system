pub mod chunk_server;
pub mod downloader;

use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use crate::error::Result;
use crate::manifest::{ChunkBitmap, Manifest};
use crate::proto::client::TrackerClient;

// ─────────────────────────────────────────────────────────────────────────────
// Configuration
// ─────────────────────────────────────────────────────────────────────────────

pub struct PeerNodeConfig {
    pub tracker_addr: String,
    pub peer_addr:    String,   // this node's TCP listen addr for P2P
    pub rack_id:      String,
    pub job_id:       String,
    pub data_dir:     PathBuf,
    pub concurrency:  usize,
}

// ─────────────────────────────────────────────────────────────────────────────
// PeerNode
// ─────────────────────────────────────────────────────────────────────────────

pub struct PeerNode {
    pub node_id: String,
    pub config:  PeerNodeConfig,
}

impl PeerNode {
    pub fn new(config: PeerNodeConfig) -> Self {
        let node_id = uuid::Uuid::new_v4().to_string();
        Self { node_id, config }
    }

    pub async fn run(self) -> Result<()> {
        tokio::fs::create_dir_all(&self.config.data_dir).await?;

        // ── 1. Fetch manifest from Tracker ───────────────────────────────
        let manifest = self.fetch_manifest().await?;
        let manifest = Arc::new(manifest);

        // ── 2. Start chunk-serving TCP server (data plane) ────────────────
        let data_dir  = Arc::new(self.config.data_dir.clone());
        let peer_addr = self.config.peer_addr.clone();
        let m_clone   = manifest.clone();
        let dd_clone  = data_dir.clone();
        tokio::spawn(async move {
            if let Err(e) = chunk_server::run(&peer_addr, m_clone, dd_clone).await {
                error!("chunk server error: {e}");
            }
        });

        // ── 3. Shared bitmap ──────────────────────────────────────────────
        let bitmap = Arc::new(Mutex::new(ChunkBitmap::new(manifest.chunk_count)));

        // ── 4. Heartbeat loop ─────────────────────────────────────────────
        {
            let node_id      = self.node_id.clone();
            let job_id       = self.config.job_id.clone();
            let rack_id      = self.config.rack_id.clone();
            let peer_addr    = self.config.peer_addr.clone();
            let tracker_addr = self.config.tracker_addr.clone();
            let bitmap       = bitmap.clone();

            tokio::spawn(async move {
                let mut interval = tokio::time::interval(
                    tokio::time::Duration::from_secs(5)
                );
                loop {
                    interval.tick().await;
                    let bm_bytes = bitmap.lock().await.as_bytes().to_vec();
                    let mut stream = match tokio::net::TcpStream::connect(&tracker_addr).await {
                        Ok(s)  => s,
                        Err(e) => { warn!("heartbeat connect failed: {e}"); continue; }
                    };
                    if let Err(e) = TrackerClient::heartbeat(
                        &mut stream, &job_id, &node_id, &rack_id,
                        &peer_addr, &bm_bytes, 8_000
                    ).await {
                        warn!("heartbeat failed: {e}");
                    }
                }
            });
        }

        // ── 5. Download loop ──────────────────────────────────────────────
        downloader::run(downloader::DownloaderConfig {
            node_id:      self.node_id.clone(),
            job_id:       self.config.job_id.clone(),
            rack_id:      self.config.rack_id.clone(),
            tracker_addr: self.config.tracker_addr.clone(),
            data_dir:     data_dir.clone(),
            manifest:     manifest.clone(),
            bitmap:       bitmap.clone(),
            concurrency:  self.config.concurrency,
        }).await?;

        info!(node = %self.node_id, "download complete ✓");
        Ok(())
    }

    /// Fetch the manifest for our job from the Tracker (via status → manifest).
    /// In practice you'd cache it locally; here we reconstruct from the job.
    async fn fetch_manifest(&self) -> Result<Manifest> {
        let mut client = TrackerClient::connect(&self.config.tracker_addr).await?;
        let status = client.job_status(&self.config.job_id).await?;

        // The status body embeds the manifest fields — extract them.
        let manifest: Manifest = serde_json::from_value(
            status.get("manifest")
                  .cloned()
                  .unwrap_or(status.clone())
        ).map_err(|e| crate::error::DistError::Proto(e.to_string()))?;

        Ok(manifest)
    }
}
