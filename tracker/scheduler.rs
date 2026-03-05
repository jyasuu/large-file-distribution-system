use super::{CandidatePeer, Job};

// ─────────────────────────────────────────────────────────────────────────────
// Rarity-First Scheduler
// ─────────────────────────────────────────────────────────────────────────────
//
// Returns the top-N peers that hold `chunk_index`, ranked by:
//   1. Scarcity of the chunk across the cluster (fewer holders → higher priority
//      for the caller to prefer this chunk in its outer loop — returned here).
//   2. Within the holder set: highest available upload bandwidth first.

pub fn rarity_first(
    job:          &Job,
    requester_id: &str,
    chunk_index:  u32,
    top_n:        usize,
) -> Vec<CandidatePeer> {
    let mut candidates: Vec<CandidatePeer> = job.nodes.iter()
        .filter(|e| {
            let ns = e.value().read();
            ns.node_id != requester_id && ns.bitmap.has(chunk_index)
        })
        .map(|e| {
            let ns = e.value().read();
            CandidatePeer {
                node_id:        ns.node_id.clone(),
                peer_addr:      ns.peer_addr.clone(),
                rack_id:        ns.rack_id.clone(),
                upload_bw_mbps: ns.upload_bw_mbps,
            }
        })
        .collect();

    // Highest bandwidth first within the holder set.
    candidates.sort_by(|a, b| b.upload_bw_mbps.cmp(&a.upload_bw_mbps));
    candidates.truncate(top_n);
    candidates
}

// ─────────────────────────────────────────────────────────────────────────────
// Topology-Aware Scheduler
// ─────────────────────────────────────────────────────────────────────────────
//
// Prefers peers on the same rack (minimise cross-rack traffic).
// Within each group (same-rack / remote) sorts by available upload bandwidth.
// Falls back to remote peers if no same-rack holder exists.

pub fn topology_aware(
    job:            &Job,
    requester_id:   &str,
    requester_rack: &str,
    chunk_index:    u32,
    top_n:          usize,
) -> Vec<CandidatePeer> {
    let mut local:  Vec<CandidatePeer> = Vec::new();
    let mut remote: Vec<CandidatePeer> = Vec::new();

    for entry in job.nodes.iter() {
        let ns = entry.value().read();
        if ns.node_id == requester_id || !ns.bitmap.has(chunk_index) { continue; }

        let peer = CandidatePeer {
            node_id:        ns.node_id.clone(),
            peer_addr:      ns.peer_addr.clone(),
            rack_id:        ns.rack_id.clone(),
            upload_bw_mbps: ns.upload_bw_mbps,
        };

        if ns.rack_id == requester_rack {
            local.push(peer);
        } else {
            remote.push(peer);
        }
    }

    // Sort each group by bandwidth (descending).
    local.sort_by(|a, b|  b.upload_bw_mbps.cmp(&a.upload_bw_mbps));
    remote.sort_by(|a, b| b.upload_bw_mbps.cmp(&a.upload_bw_mbps));

    // Local peers come first; fill remainder from remote.
    let mut result = local;
    result.extend(remote);
    result.truncate(top_n);
    result
}

// ─────────────────────────────────────────────────────────────────────────────
// Rarity scoring (used by PeerNode to order its download queue)
// ─────────────────────────────────────────────────────────────────────────────

/// Returns a sorted list of (chunk_index, holder_count) — rarest first.
/// The PeerNode uses this to prioritise which missing chunks to request next.
pub fn rarity_order(job: &Job, missing: &[u32]) -> Vec<(u32, u32)> {
    let counts = job.replica_counts();
    let mut ranked: Vec<(u32, u32)> = missing.iter()
        .map(|&i| (i, counts.get(i as usize).copied().unwrap_or(0)))
        .collect();
    ranked.sort_by_key(|&(_, c)| c); // ascending: rarest first
    ranked
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{ChunkBitmap, Manifest};
    use crate::tracker::{Job, NodeState, SchedulingPolicy};
    use parking_lot::RwLock;
    use std::sync::Arc;

    fn make_job() -> Arc<Job> {
        let data  = vec![0u8; 100 * 1024 * 1024];
        let mf    = Manifest::build("m", "1", &data, 50 * 1024 * 1024).unwrap();
        Job::new(mf, SchedulingPolicy::RarityFirst)
    }

    fn add_node(job: &Arc<Job>, id: &str, rack: &str, has_chunks: &[u32], bw: u64) {
        let mut ns = NodeState::new(id.into(), rack.into(), format!("{id}:7100"), 2, bw);
        for &c in has_chunks { ns.bitmap.mark_complete(c); }
        job.nodes.insert(id.into(), Arc::new(RwLock::new(ns)));
    }

    #[test]
    fn rarity_first_returns_holders() {
        let job = make_job();
        add_node(&job, "seed", "rack-A", &[0, 1], 10_000);
        add_node(&job, "n1",   "rack-B", &[0],    5_000);

        let peers = rarity_first(&job, "requester", 0, 5);
        assert_eq!(peers.len(), 2);
        assert_eq!(peers[0].node_id, "seed"); // higher BW first
    }

    #[test]
    fn topology_aware_prefers_local() {
        let job = make_job();
        add_node(&job, "seed",    "rack-A", &[0, 1], 10_000);
        add_node(&job, "local1",  "rack-B", &[0],     8_000);
        add_node(&job, "remote1", "rack-C", &[0],     9_000); // higher BW but remote

        let peers = topology_aware(&job, "requester", "rack-B", 0, 5);
        // local1 (rack-B) should come before remote1 despite lower BW
        assert_eq!(peers[0].node_id, "local1");
    }

    #[test]
    fn rarity_order_sorts_correctly() {
        let job = make_job();
        // chunk 0 held by 2 nodes, chunk 1 held by 1 node → chunk 1 is rarer
        add_node(&job, "seed", "rack-A", &[0, 1], 10_000);
        add_node(&job, "n1",   "rack-B", &[0],     5_000);

        let order = rarity_order(&job, &[0, 1]);
        assert_eq!(order[0].0, 1); // chunk 1 rarest
    }
}
