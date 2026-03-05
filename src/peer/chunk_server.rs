use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, error, warn};

use crate::error::Result;
use crate::manifest::Manifest;
use crate::peer::downloader::chunk_path;

// ─────────────────────────────────────────────────────────────────────────────
// Chunk-server TCP listener
//
// Wire protocol (inbound request):
//   4-byte job_id length  |  job_id bytes
//   4-byte chunk_index    (big-endian u32)
//
// Wire protocol (response):
//   4-byte status         (0 = OK, 1 = not found / error)
//   8-byte data length    (only when status == 0)
//   N bytes data          (raw chunk bytes)
// ─────────────────────────────────────────────────────────────────────────────

pub async fn run(
    addr:     &str,
    manifest: Arc<Manifest>,
    data_dir: Arc<PathBuf>,
) -> Result<()> {
    let listener = TcpListener::bind(addr).await?;
    tracing::info!("chunk server listening on {addr}");

    loop {
        match listener.accept().await {
            Ok((stream, peer_addr)) => {
                debug!("chunk request from {peer_addr}");
                let m  = manifest.clone();
                let dd = data_dir.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_chunk_request(stream, m, dd).await {
                        warn!("chunk serve error: {e}");
                    }
                });
            }
            Err(e) => error!("chunk server accept error: {e}"),
        }
    }
}

async fn handle_chunk_request(
    mut stream:  TcpStream,
    manifest:    Arc<Manifest>,
    data_dir:    Arc<PathBuf>,
) -> Result<()> {
    // ── Read request ──────────────────────────────────────────────────────────
    let job_id_len  = stream.read_u32().await? as usize;
    let mut job_buf = vec![0u8; job_id_len];
    stream.read_exact(&mut job_buf).await?;
    let _job_id     = String::from_utf8_lossy(&job_buf).to_string();
    let chunk_index = stream.read_u32().await?;

    // ── Load chunk from disk ─────────────────────────────────────────────────
    let path = chunk_path(&data_dir, &manifest.file_id, chunk_index);
    match tokio::fs::read(&path).await {
        Ok(data) => {
            stream.write_u32(0).await?;                     // status: OK
            stream.write_u64(data.len() as u64).await?;    // data length
            stream.write_all(&data).await?;                 // chunk bytes
            metrics::increment_counter!("peer_chunks_served_total");
            debug!(chunk = chunk_index, bytes = data.len(), "chunk served");
        }
        Err(_) => {
            // Chunk not found locally — respond with error status.
            stream.write_u32(1).await?;
            warn!(chunk = chunk_index, "chunk not found locally");
        }
    }

    Ok(())
}

use metrics;
