use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, Semaphore};
use tracing::{debug, error, warn};

use crate::error::{DistError, Result};
use crate::manifest::{ChunkBitmap, Manifest};
use crate::proto::PeerInfo;
use crate::proto::client::TrackerClient;

// ─────────────────────────────────────────────────────────────────────────────
// Config & entry point
// ─────────────────────────────────────────────────────────────────────────────

pub struct DownloaderConfig {
    pub node_id:      String,
    pub job_id:       String,
    pub rack_id:      String,
    pub tracker_addr: String,
    pub data_dir:     Arc<PathBuf>,
    pub manifest:     Arc<Manifest>,
    pub bitmap:       Arc<Mutex<ChunkBitmap>>,
    pub concurrency:  usize,
}

pub async fn run(cfg: DownloaderConfig) -> Result<()> {
    let sem = Arc::new(Semaphore::new(cfg.concurrency));
    let cfg = Arc::new(cfg);

    loop {
        // Collect missing chunks under a short lock.
        let missing: Vec<u32> = {
            let bm = cfg.bitmap.lock().await;
            if bm.is_complete() { break; }
            bm.missing().collect()
        };

        // Ask Tracker for peers for the rarest chunk we're missing.
        // (We iterate over `missing` in rarity order via the scheduler,
        //  but here we just round-robin through missing for simplicity.)
        let mut handles = vec![];

        for chunk_index in missing.into_iter().take(cfg.concurrency * 2) {
            // Check again: another task may have completed it already.
            if cfg.bitmap.lock().await.has(chunk_index) { continue; }

            let permit = sem.clone().acquire_owned().await
                .map_err(|e| DistError::Proto(e.to_string()))?;
            let cfg    = cfg.clone();

            handles.push(tokio::spawn(async move {
                let _permit = permit;

                // Retry up to 3 candidate peers.
                for attempt in 0..3u32 {
                    let peers = match get_peers_from_tracker(
                        &cfg.tracker_addr, &cfg.job_id, &cfg.node_id,
                        &cfg.rack_id, chunk_index, 5,
                    ).await {
                        Ok(p) => p,
                        Err(e) => {
                            warn!(chunk = chunk_index, attempt, "tracker query failed: {e}");
                            tokio::time::sleep(backoff(attempt)).await;
                            continue;
                        }
                    };

                    if peers.is_empty() {
                        warn!(chunk = chunk_index, "no peers available, backing off");
                        tokio::time::sleep(backoff(attempt)).await;
                        continue;
                    }

                    // Try each candidate in turn.
                    let mut succeeded = false;
                    for peer in &peers {
                        match download_chunk_from_peer(peer, chunk_index, &cfg).await {
                            Ok(()) => { succeeded = true; break; }
                            Err(e) => {
                                warn!(
                                    chunk = chunk_index,
                                    peer  = %peer.node_id,
                                    "fetch/verify failed: {e}"
                                );
                            }
                        }
                    }

                    if succeeded { return; }
                    tokio::time::sleep(backoff(attempt)).await;
                }

                error!(chunk = chunk_index, "all attempts exhausted");
            }));
        }

        for h in handles { let _ = h.await; }

        // Brief yield before the next round to avoid spin-looping
        // when peers are temporarily unavailable.
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

fn backoff(attempt: u32) -> tokio::time::Duration {
    tokio::time::Duration::from_millis(200 * 2u64.pow(attempt))
}

async fn get_peers_from_tracker(
    tracker_addr: &str,
    job_id:       &str,
    node_id:      &str,
    rack_id:      &str,
    chunk_index:  u32,
    top_n:        usize,
) -> Result<Vec<PeerInfo>> {
    let mut stream = TcpStream::connect(tracker_addr).await?;
    TrackerClient::get_peers(
        &mut stream, job_id, node_id, rack_id, chunk_index, top_n
    ).await
}

/// Download one chunk directly from a peer over raw TCP, verify, and persist.
async fn download_chunk_from_peer(
    peer:        &PeerInfo,
    chunk_index: u32,
    cfg:         &DownloaderConfig,
) -> Result<()> {
    let mut stream = TcpStream::connect(&peer.peer_addr).await?;

    // ── Send chunk request header ────────────────────────────────────────────
    //  wire format:  4-byte job_id length | job_id bytes
    //                4-byte chunk_index (big-endian u32)
    let job_id_bytes = cfg.job_id.as_bytes();
    stream.write_u32(job_id_bytes.len() as u32).await?;
    stream.write_all(job_id_bytes).await?;
    stream.write_u32(chunk_index).await?;

    // ── Read chunk response ──────────────────────────────────────────────────
    //  wire format:  4-byte status (0 = ok, 1 = not found)
    //                8-byte data length
    //                N bytes data
    let status = stream.read_u32().await?;
    if status != 0 {
        return Err(DistError::Proto(format!(
            "peer {} returned error status {status} for chunk {chunk_index}",
            peer.node_id
        )));
    }

    let data_len = stream.read_u64().await? as usize;
    let (expected_start, expected_end) = cfg.manifest.chunk_range(chunk_index);
    let expected_len = (expected_end - expected_start) as usize;

    if data_len != expected_len {
        return Err(DistError::Proto(format!(
            "chunk {chunk_index}: expected {expected_len} bytes, got {data_len}"
        )));
    }

    let mut data = vec![0u8; data_len];
    stream.read_exact(&mut data).await?;

    // ── Verify ───────────────────────────────────────────────────────────────
    cfg.manifest.verify_chunk(chunk_index, &data)?;

    // ── Persist ──────────────────────────────────────────────────────────────
    let path = chunk_path(&cfg.data_dir, &cfg.manifest.file_id, chunk_index);
    tokio::fs::write(&path, &data).await?;

    // ── Update bitmap ─────────────────────────────────────────────────────────
    cfg.bitmap.lock().await.mark_complete(chunk_index);

    metrics::increment_counter!("peer_chunks_received_total");
    tracing::debug!(node_id = %cfg.node_id, chunk_index, "chunk received");
    debug!(
        node  = %cfg.node_id,
        chunk = chunk_index,
        from  = %peer.node_id,
        bytes = data_len,
        "chunk ✓"
    );

    Ok(())
}

pub fn chunk_path(data_dir: &PathBuf, file_id: &str, chunk_index: u32) -> PathBuf {
    data_dir.join(format!("{file_id}.chunk.{chunk_index:06}"))
}

use metrics;
