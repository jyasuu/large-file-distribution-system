// =============================================================================
// Model Distribution System — Key Rust Examples
// =============================================================================
// Covers:
//   1. Manifest  — file metadata + chunk hash list
//   2. Chunk     — download, SHA-256 verify, bitmap tracking
//   3. Tracker   — global bitmap state, peer selection (Rarity-First)
//   4. Peer Node — async chunk fetch loop with Tracker coordination
//   5. Erasure Coding — tail-phase EC encode/decode (Reed-Solomon sketch)
//   6. Delta Patch — incremental update via chunk diff
// =============================================================================

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

// ─── Dependencies (Cargo.toml) ────────────────────────────────────────────────
// [dependencies]
// tokio        = { version = "1", features = ["full"] }
// sha2         = "0.10"
// bitvec       = "1"
// reqwest      = { version = "0.11", features = ["stream"] }
// bytes        = "1"
// serde        = { version = "1", features = ["derive"] }
// serde_json   = "1"
// reed-solomon-erasure = "6"   # for section 5
// anyhow       = "1"
// tracing      = "0.1"


// =============================================================================
// 1. MANIFEST
// =============================================================================

use serde::{Deserialize, Serialize};

/// The manifest is fetched once by every node before it starts downloading.
/// It is the single source of truth for file structure and integrity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub file_id:     String,
    pub version:     String,
    pub total_size:  u64,
    pub chunk_size:  u64,          // bytes, e.g. 50 * 1024 * 1024
    pub chunk_count: u32,
    /// SHA-256 hex digest for each chunk, indexed by chunk_index
    pub chunk_hashes: Vec<String>,
}

impl Manifest {
    /// Build a manifest by reading the source file and hashing each chunk.
    pub fn build(
        file_id: impl Into<String>,
        version: impl Into<String>,
        data: &[u8],
        chunk_size: u64,
    ) -> Self {
        use sha2::{Digest, Sha256};

        let total_size  = data.len() as u64;
        let chunk_count = ((total_size + chunk_size - 1) / chunk_size) as u32;

        let chunk_hashes = (0..chunk_count)
            .map(|i| {
                let start = (i as u64 * chunk_size) as usize;
                let end   = ((i as u64 + 1) * chunk_size).min(total_size) as usize;
                let mut h = Sha256::new();
                h.update(&data[start..end]);
                format!("{:x}", h.finalize())
            })
            .collect();

        Manifest {
            file_id:     file_id.into(),
            version:     version.into(),
            total_size,
            chunk_size,
            chunk_count,
            chunk_hashes,
        }
    }

    /// Byte range for chunk index i.
    pub fn chunk_range(&self, index: u32) -> (u64, u64) {
        let start = index as u64 * self.chunk_size;
        let end   = (start + self.chunk_size).min(self.total_size);
        (start, end)
    }
}


// =============================================================================
// 2. CHUNK VERIFICATION & BITMAP
// =============================================================================

use bitvec::prelude::*;

/// Verify a downloaded chunk against its manifest hash.
/// Returns an error if the data is corrupt — caller should retry from a
/// different peer and optionally penalise the source.
pub fn verify_chunk(
    data:     &[u8],
    expected: &str,    // hex SHA-256 from manifest
) -> anyhow::Result<()> {
    use sha2::{Digest, Sha256};

    let mut h = Sha256::new();
    h.update(data);
    let actual = format!("{:x}", h.finalize());

    if actual != expected {
        anyhow::bail!(
            "chunk hash mismatch: expected={} got={}",
            expected, actual
        );
    }
    Ok(())
}

/// Per-node chunk ownership bitmap.
/// bit[i] == true  →  chunk i has been received and verified.
#[derive(Clone)]
pub struct ChunkBitmap {
    bits:  BitVec<u8, Msb0>,
    count: u32,    // how many bits are set
}

impl ChunkBitmap {
    pub fn new(chunk_count: u32) -> Self {
        Self {
            bits:  bitvec![u8, Msb0; 0; chunk_count as usize],
            count: 0,
        }
    }

    /// Mark chunk as complete; returns true if it was newly set.
    pub fn mark_complete(&mut self, index: u32) -> bool {
        if !self.bits[index as usize] {
            self.bits.set(index as usize, true);
            self.count += 1;
            true
        } else {
            false
        }
    }

    pub fn has(&self, index: u32) -> bool {
        self.bits[index as usize]
    }

    pub fn completion_ratio(&self) -> f32 {
        self.count as f32 / self.bits.len() as f32
    }

    /// Serialise to raw bytes for sending to the Tracker.
    pub fn as_bytes(&self) -> &[u8] {
        self.bits.as_raw_slice()
    }

    /// Missing chunk indices — used by the peer node's download loop.
    pub fn missing(&self) -> impl Iterator<Item = u32> + '_ {
        self.bits
            .iter()
            .enumerate()
            .filter(|(_, b)| !**b)
            .map(|(i, _)| i as u32)
    }
}


// =============================================================================
// 3. TRACKER — GLOBAL STATE & PEER SELECTION
// =============================================================================

/// One entry per cluster node maintained by the Tracker.
#[derive(Debug, Clone)]
pub struct NodeState {
    pub node_id:             String,
    pub bitmap:              ChunkBitmap,
    pub available_upload_bw: u64,    // Mbps
    pub rack_id:             String,
    pub last_seen:           std::time::Instant,
}

/// Tracker holds the global view used for peer-selection decisions.
pub struct Tracker {
    nodes:        RwLock<HashMap<String, NodeState>>,
    chunk_count:  u32,
}

impl Tracker {
    pub fn new(chunk_count: u32) -> Arc<Self> {
        Arc::new(Self {
            nodes:       RwLock::new(HashMap::new()),
            chunk_count,
        })
    }

    /// Called periodically by each node agent to push its latest state.
    pub fn update_node(
        &self,
        node_id:   String,
        bitmap:    ChunkBitmap,
        upload_bw: u64,
        rack_id:   String,
    ) {
        let mut nodes = self.nodes.write().unwrap();
        nodes.insert(node_id.clone(), NodeState {
            node_id,
            bitmap,
            available_upload_bw: upload_bw,
            rack_id,
            last_seen: std::time::Instant::now(),
        });
    }

    // ── Rarity-First peer selection ──────────────────────────────────────────
    //
    // For each missing chunk on the requesting node, count how many OTHER nodes
    // hold it.  Return a ranked list: rarest chunks first, with the peer that
    // has the most available bandwidth as the preferred source.

    pub fn get_peers_rarity_first(
        &self,
        requester_id: &str,
        missing: &[u32],
        top_n: usize,
    ) -> Vec<(u32, Vec<CandidatePeer>)> {
        let nodes = self.nodes.read().unwrap();

        // Count holders per chunk
        let mut holders: HashMap<u32, Vec<&NodeState>> = HashMap::new();
        for chunk_idx in missing {
            let h: Vec<&NodeState> = nodes
                .values()
                .filter(|n| n.node_id != requester_id && n.bitmap.has(*chunk_idx))
                .collect();
            holders.insert(*chunk_idx, h);
        }

        // Sort chunks by holder count (ascending = rarest first)
        let mut ranked: Vec<u32> = missing.to_vec();
        ranked.sort_by_key(|idx| holders.get(idx).map(|v| v.len()).unwrap_or(0));

        ranked
            .into_iter()
            .take(top_n)
            .map(|chunk_idx| {
                let mut peers: Vec<CandidatePeer> = holders
                    .get(&chunk_idx)
                    .unwrap_or(&vec![])
                    .iter()
                    .map(|n| CandidatePeer {
                        node_id:    n.node_id.clone(),
                        rack_id:    n.rack_id.clone(),
                        upload_bw:  n.available_upload_bw,
                    })
                    .collect();
                // Prefer highest bandwidth among holders
                peers.sort_by(|a, b| b.upload_bw.cmp(&a.upload_bw));
                (chunk_idx, peers)
            })
            .collect()
    }

    // ── Topology-Aware peer selection ─────────────────────────────────────────
    //
    // Same interface; prefers peers on the same rack before going cross-rack.

    pub fn get_peers_topology_aware(
        &self,
        requester_id:   &str,
        requester_rack: &str,
        missing:        &[u32],
        top_n:          usize,
    ) -> Vec<(u32, Vec<CandidatePeer>)> {
        let nodes = self.nodes.read().unwrap();

        missing
            .iter()
            .take(top_n)
            .map(|&chunk_idx| {
                let mut peers: Vec<CandidatePeer> = nodes
                    .values()
                    .filter(|n| n.node_id != requester_id && n.bitmap.has(chunk_idx))
                    .map(|n| CandidatePeer {
                        node_id:   n.node_id.clone(),
                        rack_id:   n.rack_id.clone(),
                        upload_bw: n.available_upload_bw,
                    })
                    .collect();

                // Local rack first, then remote; within each group sort by BW
                peers.sort_by(|a, b| {
                    let a_local = a.rack_id == requester_rack;
                    let b_local = b.rack_id == requester_rack;
                    b_local.cmp(&a_local)
                        .then(b.upload_bw.cmp(&a.upload_bw))
                });
                (chunk_idx, peers)
            })
            .collect()
    }

    /// Global replica count for each chunk — used for rarity scoring and
    /// detecting single-holder (replica_count == 1) alert conditions.
    pub fn replica_counts(&self) -> Vec<u32> {
        let nodes = self.nodes.read().unwrap();
        let mut counts = vec![0u32; self.chunk_count as usize];
        for node in nodes.values() {
            for i in 0..self.chunk_count {
                if node.bitmap.has(i) {
                    counts[i as usize] += 1;
                }
            }
        }
        counts
    }
}

#[derive(Debug, Clone)]
pub struct CandidatePeer {
    pub node_id:   String,
    pub rack_id:   String,
    pub upload_bw: u64,
}


// =============================================================================
// 4. PEER NODE — ASYNC DOWNLOAD LOOP
// =============================================================================
//
// Each node runs this loop concurrently:
//   a) Ask Tracker which chunks to fetch and from whom
//   b) Download directly from the suggested peer (P2P data plane)
//   c) Verify hash, update local bitmap
//   d) Report updated bitmap to Tracker

use tokio::sync::Semaphore;

pub struct PeerNode {
    pub node_id:  String,
    pub rack_id:  String,
    pub manifest: Arc<Manifest>,
    pub bitmap:   Arc<tokio::sync::Mutex<ChunkBitmap>>,
    pub tracker:  Arc<Tracker>,
    /// Max concurrent chunk downloads
    pub max_concurrent: usize,
}

impl PeerNode {
    pub async fn run(&self) -> anyhow::Result<()> {
        let sem = Arc::new(Semaphore::new(self.max_concurrent));

        loop {
            // Collect missing chunks
            let missing: Vec<u32> = {
                let bm = self.bitmap.lock().await;
                if bm.completion_ratio() >= 1.0 {
                    tracing::info!(node = %self.node_id, "download complete");
                    break;
                }
                bm.missing().collect()
            };

            // Ask Tracker for peer assignments (Rarity-First)
            let assignments = self.tracker.get_peers_rarity_first(
                &self.node_id,
                &missing,
                /*top_n=*/ self.max_concurrent * 2,
            );

            if assignments.is_empty() {
                // No peers available yet — brief back-off
                tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                continue;
            }

            let mut handles = vec![];

            for (chunk_idx, peers) in assignments {
                let permit  = sem.clone().acquire_owned().await?;
                let bitmap  = self.bitmap.clone();
                let manifest = self.manifest.clone();
                let node_id  = self.node_id.clone();

                handles.push(tokio::spawn(async move {
                    let _permit = permit; // released when task finishes

                    // Try candidates in order; move on if one fails
                    for peer in &peers {
                        match fetch_chunk_from_peer(&peer.node_id, chunk_idx, &manifest).await {
                            Ok(data) => {
                                match verify_chunk(&data, &manifest.chunk_hashes[chunk_idx as usize]) {
                                    Ok(_) => {
                                        let mut bm = bitmap.lock().await;
                                        bm.mark_complete(chunk_idx);
                                        tracing::debug!(
                                            node = %node_id,
                                            chunk = chunk_idx,
                                            from  = %peer.node_id,
                                            "chunk ok"
                                        );
                                        return;
                                    }
                                    Err(e) => {
                                        tracing::warn!(chunk = chunk_idx, peer = %peer.node_id, "hash mismatch: {e}");
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::warn!(chunk = chunk_idx, peer = %peer.node_id, "fetch failed: {e}");
                            }
                        }
                    }
                    tracing::error!(chunk = chunk_idx, "all peers exhausted for chunk");
                }));
            }

            // Await this batch before requesting the next assignment
            for h in handles {
                let _ = h.await;
            }

            // Report updated bitmap to Tracker
            {
                let bm = self.bitmap.lock().await;
                self.tracker.update_node(
                    self.node_id.clone(),
                    bm.clone(),
                    /*upload_bw=*/ 8_000,   // Mbps — real impl reads from NIC
                    self.rack_id.clone(),
                );
            }
        }

        Ok(())
    }
}

/// Direct HTTP range-request to a peer node.
/// In production this would be a gRPC or custom TCP stream.
async fn fetch_chunk_from_peer(
    peer_id:    &str,
    chunk_idx:  u32,
    manifest:   &Manifest,
) -> anyhow::Result<bytes::Bytes> {
    let (start, end) = manifest.chunk_range(chunk_idx);
    let url = format!("http://{peer_id}/chunks/{}/{}", manifest.file_id, chunk_idx);

    let resp = reqwest::Client::new()
        .get(&url)
        .header("Range", format!("bytes={start}-{}", end - 1))
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;

    Ok(resp)
}


// =============================================================================
// 5. ERASURE CODING — TAIL-PHASE LONG-TAIL MITIGATION
// =============================================================================
//
// When >95 % of nodes have completed, the Tracker triggers EC mode:
//   • Encode remaining data shards into parity shards
//   • Broadcast coded shards from multiple nodes in parallel
//   • Late nodes reconstruct missing originals from any k-of-(k+m) shards

use reed_solomon_erasure::galois_8::ReedSolomon;

pub struct ErasureCoder {
    rs:          ReedSolomon,
    data_shards: usize,
    par_shards:  usize,
}

impl ErasureCoder {
    /// k = data_shards, m = parity_shards.
    /// Any k shards from the k+m total suffice for reconstruction.
    pub fn new(data_shards: usize, parity_shards: usize) -> anyhow::Result<Self> {
        Ok(Self {
            rs:          ReedSolomon::new(data_shards, parity_shards)?,
            data_shards,
            par_shards:  parity_shards,
        })
    }

    /// Encode `data_shards` equal-sized slices → append `parity_shards`.
    pub fn encode(&self, shards: &mut Vec<Vec<u8>>) -> anyhow::Result<()> {
        self.rs.encode(shards)?;
        Ok(())
    }

    /// Reconstruct missing shards.  Pass `None` for each shard the node
    /// does not yet hold; at least `data_shards` must be `Some`.
    pub fn reconstruct(
        &self,
        shards: &mut Vec<Option<Vec<u8>>>,
    ) -> anyhow::Result<()> {
        self.rs.reconstruct(shards)?;
        Ok(())
    }
}

/// Tail-phase controller — called by the Tracker when job enters the tail.
pub async fn tail_phase_ec(
    tracker:      Arc<Tracker>,
    manifest:     Arc<Manifest>,
    data_shards:  usize,
    par_shards:   usize,
) -> anyhow::Result<()> {
    let coder = ErasureCoder::new(data_shards, par_shards)?;

    let counts = tracker.replica_counts();

    // Identify scarce chunks (held by fewer than 3 nodes)
    let scarce: Vec<u32> = counts
        .iter()
        .enumerate()
        .filter(|(_, &c)| c < 3)
        .map(|(i, _)| i as u32)
        .collect();

    if scarce.is_empty() {
        return Ok(());
    }

    tracing::info!(scarce_count = scarce.len(), "entering EC tail phase");

    // Group scarce chunks into EC stripe windows of size `data_shards`
    for stripe in scarce.chunks(data_shards) {
        // Fetch data for this stripe (from any available holder)
        // … (omitted: same peer-fetch logic as section 4)

        // Encode: produce parity shards
        let shard_size = manifest.chunk_size as usize;
        let mut shards: Vec<Vec<u8>> = stripe
            .iter()
            .map(|_| vec![0u8; shard_size])   // placeholder; real impl fills from fetched data
            .chain((0..par_shards).map(|_| vec![0u8; shard_size]))
            .collect();
        coder.encode(&mut shards)?;

        // Distribute parity shards to nodes that are missing originals
        // (broadcast logic omitted for brevity)
        tracing::debug!(stripe_len = stripe.len(), "EC stripe encoded & distributed");
    }

    Ok(())
}


// =============================================================================
// 6. DELTA PATCH — INCREMENTAL MODEL UPDATES
// =============================================================================

/// Describes what changed between two model versions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeltaPatch {
    pub old_version:    String,
    pub new_version:    String,
    /// Chunk indices that are identical — nodes skip downloading these.
    pub unchanged:      HashSet<u32>,
    /// Chunk indices that are new or modified, with their new hashes.
    pub changed:        HashMap<u32, String>,   // index → new SHA-256
    /// Chunk indices present in old but absent in new (file shrank).
    pub deleted:        HashSet<u32>,
}

impl DeltaPatch {
    /// Compute the delta between two manifests.
    pub fn compute(old: &Manifest, new: &Manifest) -> Self {
        let mut unchanged = HashSet::new();
        let mut changed   = HashMap::new();
        let mut deleted   = HashSet::new();

        let old_count = old.chunk_count as usize;
        let new_count = new.chunk_count as usize;

        for i in 0..new_count {
            if i < old_count && old.chunk_hashes[i] == new.chunk_hashes[i] {
                unchanged.insert(i as u32);
            } else {
                changed.insert(i as u32, new.chunk_hashes[i].clone());
            }
        }
        for i in new_count..old_count {
            deleted.insert(i as u32);
        }

        DeltaPatch {
            old_version: old.version.clone(),
            new_version: new.version.clone(),
            unchanged,
            changed,
            deleted,
        }
    }

    /// Apply this patch to a node: returns the set of chunk indices to fetch.
    /// Chunks in `unchanged` are already valid — no download needed.
    pub fn chunks_to_fetch(&self) -> Vec<u32> {
        self.changed.keys().copied().collect()
    }

    /// Verify that we only overwrite chunks that should actually change.
    pub fn validate_no_spurious_overwrites(&self, chunk_idx: u32) -> bool {
        !self.unchanged.contains(&chunk_idx)
    }
}


// =============================================================================
// EXAMPLE USAGE (integration sketch)
// =============================================================================

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    // ── Build manifest ────────────────────────────────────────────────────────
    let data: Vec<u8> = vec![0u8; 100 * 1024 * 1024];   // 100 MB dummy payload
    let manifest = Arc::new(Manifest::build(
        "llama-3-70b",
        "2.0.0",
        &data,
        50 * 1024 * 1024,   // 50 MB chunks → 2 chunks
    ));
    println!("Manifest: {} chunks", manifest.chunk_count);

    // ── Bootstrap Tracker ─────────────────────────────────────────────────────
    let tracker = Tracker::new(manifest.chunk_count);

    // Seed node registers as having all chunks
    let mut seed_bm = ChunkBitmap::new(manifest.chunk_count);
    for i in 0..manifest.chunk_count {
        seed_bm.mark_complete(i);
    }
    tracker.update_node("seed-node".into(), seed_bm, 10_000, "rack-A".into());

    // ── Spin up a peer node ───────────────────────────────────────────────────
    let peer = PeerNode {
        node_id:        "node-B1".into(),
        rack_id:        "rack-B".into(),
        manifest:       manifest.clone(),
        bitmap:         Arc::new(tokio::sync::Mutex::new(
            ChunkBitmap::new(manifest.chunk_count)
        )),
        tracker:        tracker.clone(),
        max_concurrent: 4,
    };

    // In production: peer.run().await?;
    println!("PeerNode ready — would call peer.run().await in production");

    // ── Delta patch example ───────────────────────────────────────────────────
    let mut new_data = data.clone();
    new_data[0] = 0xFF;   // mutate first chunk to simulate a model update
    let new_manifest = Manifest::build("llama-3-70b", "2.0.1", &new_data, 50 * 1024 * 1024);

    let patch = DeltaPatch::compute(&manifest, &new_manifest);
    println!(
        "Delta patch v{} → v{}: {} unchanged, {} changed, {} deleted",
        patch.old_version,
        patch.new_version,
        patch.unchanged.len(),
        patch.changed.len(),
        patch.deleted.len()
    );

    // ── Erasure coder smoke-test ──────────────────────────────────────────────
    let coder = ErasureCoder::new(4, 2)?;   // k=4, m=2
    let shard_size = 1024usize;
    let mut shards: Vec<Vec<u8>> = (0..6)
        .map(|i| vec![i as u8; shard_size])
        .collect();
    // Zero out parity shards before encoding
    for s in &mut shards[4..] { s.iter_mut().for_each(|b| *b = 0); }
    coder.encode(&mut shards)?;
    println!("EC encode OK — {} shards total ({} data + {} parity)", 6, 4, 2);

    // Simulate losing shard 1 and shard 3
    let mut recv: Vec<Option<Vec<u8>>> = shards
        .into_iter()
        .enumerate()
        .map(|(i, s)| if i == 1 || i == 3 { None } else { Some(s) })
        .collect();
    coder.reconstruct(&mut recv)?;
    println!("EC reconstruct OK — missing shards recovered");

    Ok(())
}
