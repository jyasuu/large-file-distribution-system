pub mod server;
pub mod scheduler;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use dashmap::DashMap;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::manifest::{ChunkBitmap, Manifest};
use crate::error::{DistError, Result};

// ─────────────────────────────────────────────────────────────────────────────
// Types
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SchedulingPolicy {
    RarityFirst,
    TopologyAware,
}

impl std::str::FromStr for SchedulingPolicy {
    type Err = DistError;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "rarity-first"    => Ok(Self::RarityFirst),
            "topology-aware"  => Ok(Self::TopologyAware),
            other => Err(DistError::Proto(format!("unknown policy: {other}"))),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum JobStatus {
    Running,
    Complete,
    Cancelled,
}

/// A distribution job – one model version pushed to the cluster.
#[derive(Debug)]
pub struct Job {
    pub job_id:   String,
    pub manifest: Manifest,
    pub policy:   SchedulingPolicy,
    pub status:   RwLock<JobStatus>,
    pub created_at: std::time::SystemTime,
    /// node_id → per-node state
    pub nodes:    DashMap<String, Arc<RwLock<NodeState>>>,
}

impl Job {
    pub fn new(manifest: Manifest, policy: SchedulingPolicy) -> Arc<Self> {
        Arc::new(Self {
            job_id:     Uuid::new_v4().to_string(),
            manifest,
            policy,
            status:     RwLock::new(JobStatus::Running),
            created_at: std::time::SystemTime::now(),
            nodes:      DashMap::new(),
        })
    }

    /// Fraction of nodes that have fully completed (bitmap == 1111..1).
    pub fn completion_ratio(&self) -> f32 {
        let total = self.nodes.len();
        if total == 0 { return 0.0; }
        let done = self.nodes.iter()
            .filter(|e| e.value().read().bitmap.is_complete())
            .count();
        done as f32 / total as f32
    }

    /// Per-chunk replica counts across all nodes.
    pub fn replica_counts(&self) -> Vec<u32> {
        let n = self.manifest.chunk_count as usize;
        let mut counts = vec![0u32; n];
        for entry in self.nodes.iter() {
            let ns = entry.value().read();
            for i in 0..n as u32 {
                if ns.bitmap.has(i) { counts[i as usize] += 1; }
            }
        }
        counts
    }

    pub fn snapshot(&self) -> JobSnapshot {
        let nodes: Vec<NodeSnapshot> = self.nodes.iter().map(|e| {
            let ns = e.value().read();
            NodeSnapshot {
                node_id:          e.key().clone(),
                rack_id:          ns.rack_id.clone(),
                completed_chunks: ns.bitmap.completed_count(),
                total_chunks:     self.manifest.chunk_count,
                upload_bw_mbps:   ns.upload_bw_mbps,
                last_seen_secs:   ns.last_seen.elapsed().as_secs(),
            }
        }).collect();

        let total_chunks = self.manifest.chunk_count;
        let avg_pct = if nodes.is_empty() { 0.0 } else {
            nodes.iter().map(|n| n.completed_chunks as f32 / total_chunks as f32).sum::<f32>()
                / nodes.len() as f32 * 100.0
        };

        JobSnapshot {
            job_id:          self.job_id.clone(),
            file_id:         self.manifest.file_id.clone(),
            version:         self.manifest.version.clone(),
            status:          format!("{:?}", *self.status.read()),
            policy:          format!("{:?}", self.policy),
            total_chunks,
            node_count:      nodes.len() as u32,
            avg_completion_pct: avg_pct,
            nodes,
        }
    }
}

/// Live state maintained by the Tracker for each peer node.
#[derive(Debug)]
pub struct NodeState {
    pub node_id:        String,
    pub rack_id:        String,
    pub peer_addr:      String,   // TCP address for P2P chunk requests
    pub bitmap:         ChunkBitmap,
    pub upload_bw_mbps: u64,
    pub last_seen:      Instant,
}

impl NodeState {
    pub fn new(
        node_id:        String,
        rack_id:        String,
        peer_addr:      String,
        chunk_count:    u32,
        upload_bw_mbps: u64,
    ) -> Self {
        Self {
            node_id,
            rack_id,
            peer_addr,
            bitmap:         ChunkBitmap::new(chunk_count),
            upload_bw_mbps,
            last_seen:      Instant::now(),
        }
    }

    pub fn touch(&mut self) { self.last_seen = Instant::now(); }

    pub fn is_stale(&self, timeout_secs: u64) -> bool {
        self.last_seen.elapsed().as_secs() > timeout_secs
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Snapshots (serialisable views)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct JobSnapshot {
    pub job_id:              String,
    pub file_id:             String,
    pub version:             String,
    pub status:              String,
    pub policy:              String,
    pub total_chunks:        u32,
    pub node_count:          u32,
    pub avg_completion_pct:  f32,
    pub nodes:               Vec<NodeSnapshot>,
}

#[derive(Debug, Serialize)]
pub struct NodeSnapshot {
    pub node_id:          String,
    pub rack_id:          String,
    pub completed_chunks: u32,
    pub total_chunks:     u32,
    pub upload_bw_mbps:   u64,
    pub last_seen_secs:   u64,
}

/// Peer candidate returned to a requesting node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidatePeer {
    pub node_id:        String,
    pub peer_addr:      String,
    pub rack_id:        String,
    pub upload_bw_mbps: u64,
}

// ─────────────────────────────────────────────────────────────────────────────
// Tracker
// ─────────────────────────────────────────────────────────────────────────────

/// The central control-plane service.
/// Holds all job and node state; never touches chunk data.
pub struct Tracker {
    jobs: DashMap<String, Arc<Job>>,
}

impl Tracker {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { jobs: DashMap::new() })
    }

    // ── Job lifecycle ──────────────────────────────────────────────────────

    pub fn create_job(&self, manifest: Manifest, policy: SchedulingPolicy) -> Arc<Job> {
        let job = Job::new(manifest, policy);
        self.jobs.insert(job.job_id.clone(), job.clone());
        metrics::counter!("tracker_jobs_created_total", 1);
        job
    }

    pub fn get_job(&self, job_id: &str) -> Result<Arc<Job>> {
        self.jobs.get(job_id)
            .map(|e| e.value().clone())
            .ok_or_else(|| DistError::JobNotFound(job_id.to_owned()))
    }

    pub fn cancel_job(&self, job_id: &str) -> Result<()> {
        let job = self.get_job(job_id)?;
        *job.status.write() = JobStatus::Cancelled;
        Ok(())
    }

    // ── Node heartbeat ─────────────────────────────────────────────────────

    /// Called when a node sends a heartbeat with its updated bitmap.
    pub fn update_node(
        &self,
        job_id:         &str,
        node_id:        String,
        rack_id:        String,
        peer_addr:      String,
        bitmap_bytes:   &[u8],
        upload_bw_mbps: u64,
    ) -> Result<()> {
        let job = self.get_job(job_id)?;
        let chunk_count = job.manifest.chunk_count;

        let entry = job.nodes.entry(node_id.clone())
            .or_insert_with(|| Arc::new(RwLock::new(NodeState::new(
                node_id.clone(), rack_id.clone(), peer_addr.clone(),
                chunk_count, upload_bw_mbps,
            ))));

        let mut ns = entry.write();
        ns.bitmap         = ChunkBitmap::from_bytes(bitmap_bytes, chunk_count);
        ns.upload_bw_mbps = upload_bw_mbps;
        ns.rack_id        = rack_id;
        ns.peer_addr      = peer_addr;
        ns.touch();

        // Check job completion
        let pct = ns.bitmap.completion_ratio();
        metrics::gauge!("node_completion_ratio", pct as f64,
            "job_id" => job_id.to_string(), "node_id" => node_id.clone());

        if pct >= 1.0 {
            let all_done = job.nodes.iter().all(|e| e.value().read().bitmap.is_complete());
            if all_done {
                *job.status.write() = JobStatus::Complete;
                metrics::counter!("tracker_jobs_complete_total", 1);
                tracing::info!(job_id, "🎉 all nodes complete");
            }
        }
        Ok(())
    }

    // ── Peer selection ─────────────────────────────────────────────────────

    pub fn get_peers(
        &self,
        job_id:         &str,
        requester_id:   &str,
        requester_rack: &str,
        chunk_index:    u32,
        top_n:          usize,
    ) -> Result<Vec<CandidatePeer>> {
        let job = self.get_job(job_id)?;
        let peers = match job.policy {
            SchedulingPolicy::RarityFirst =>
                scheduler::rarity_first(&job, requester_id, chunk_index, top_n),
            SchedulingPolicy::TopologyAware =>
                scheduler::topology_aware(&job, requester_id, requester_rack, chunk_index, top_n),
        };
        metrics::counter!("tracker_peer_queries_total", 1, "job_id" => job_id.to_string());
        Ok(peers)
    }

    // ── Stale node cleanup (run periodically) ──────────────────────────────

    pub fn evict_stale_nodes(&self, timeout_secs: u64) {
        for job_entry in self.jobs.iter() {
            let job = job_entry.value();
            job.nodes.retain(|_, ns| !ns.read().is_stale(timeout_secs));
        }
    }

    pub fn all_job_snapshots(&self) -> Vec<JobSnapshot> {
        self.jobs.iter().map(|e| e.value().snapshot()).collect()
    }
}

use metrics; // bring macro into scope
