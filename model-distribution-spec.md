# System Design Specification
## Large-Scale Model Distribution System
> Controlled P2P Intra-Cluster File Distribution with Centralized Tracker

| Field | Value |
|---|---|
| Version | 1.0 |
| Status | Draft |
| Date | 2026-03-05 |
| Audience | Platform / Infrastructure Engineers |

---

## 1. Overview

This document specifies the design of a large-scale model distribution system that efficiently propagates ML model files (potentially hundreds of gigabytes) from an external model repository to every node in a compute cluster. The approach is a centralized-tracker-controlled P2P protocol, inspired by BitTorrent but adapted for a private datacenter environment.

| Field | Value |
|---|---|
| **Problem** | Distribute a ~500 GB model to hundreds of cluster nodes as fast as possible. |
| **Cluster size** | Tens to thousands of nodes (must scale). |
| **External BW** | 10 Gbps between the external model repo and the cluster. |
| **Per-node BW** | 10 Gbps upload / 10 Gbps download (intra-cluster). |
| **Model size** | 1 MB – 1 TB (configurable; reference scenario: 500 GB). |

---

## 2. Requirements

### 2.1 Functional Requirements

- **File distribution:** Support single-file transfers ranging from 1 MB to 1 TB.
- **Chunked transfer:** Split files into configurable chunks; support configurable chunk size (default 50 MB).
- **Resume & partial transfer:** Any node can resume an interrupted download from the last completed chunk.
- **Integrity verification:** Validate every chunk via SHA-256 hash after receipt.
- **Observability:** Real-time visibility into per-node download progress, chunk distribution, bandwidth utilization, and estimated time to completion.
- **Delta / incremental updates:** For model updates, transfer only changed chunks (delta patch or LoRA adapter).
- **Configurable download strategy:** Operators can select Rarity-First or Topology-Aware chunk scheduling.

### 2.2 Non-Functional Requirements

- **Reliability:** The system must complete distribution even if individual nodes fail or restart mid-transfer.
- **Efficiency:** Saturate available bandwidth; minimize overall completion time with a hard SLA on total transfer duration.
- **Data correctness:** Zero tolerance for corrupted data reaching nodes; every chunk is hash-verified.
- **Scalability:** Operate correctly from a handful of nodes up to several thousand with no architectural change.
- **Fault tolerance:** No single node failure (including the tracker) should permanently stall a distribution job.

---

## 3. Approach Analysis

Three candidate architectures were evaluated before arriving at the chosen design.

### 3.1 Option A — Pipeline Streaming (Single Chain)

All nodes are arranged in a single linear chain. Each node streams data to the next as it downloads.

| Dimension | Pros | Cons |
|---|---|---|
| Simplicity | Simple topology; easy to reason about. | Linear chain is inherently fragile. |
| Failure impact | Fault location is obvious. | Any node failure breaks the entire chain. |
| Bandwidth usage | Near-full utilization on a perfect run. | Upload + download share 10 Gbps → effective 5 Gbps max. |
| Scalability | — | Adding nodes does not reduce completion time. |
| Estimated time | — | ~800 s for 500 GB @ 5 Gbps effective. |

> **Verdict: Rejected.** Fragility and lack of parallelism make this unsuitable for production.

### 3.2 Option B — Multi-Source Parallel (Naive P2P)

The file is split into parts; nodes receive different parts in parallel and then cross-seed each other.

**Example walk-through (4 nodes, 500 GB model split into A+B):**

- **Round 1 (400 s):** Node 1 → Node 2 receives part A, Node 3 receives part B; Node 4 downloads full model from Node 1 in parallel — both upload and download slots are fully saturated.
- **Round 2 (200 s):** Node 4 seeds part A to Node 3; Node 1 seeds part B to Node 2.
- **Total:** ~600 s for 4 nodes — substantially better than Option A.

> **Verdict: Viable**, but lacks coordination intelligence. Adopted as the baseline and enhanced in Option C.

### 3.3 Option C — Centralized-Tracker Controlled P2P ✓ Chosen

Retains the parallelism of Option B but adds a centralized Tracker that holds a global view of all nodes' chunk inventories and bandwidth availability. Nodes query the Tracker for optimal download peers rather than discovering peers organically.

> This is the architecture described in the remainder of this document.

---

## 4. System Architecture

### 4.1 Component Overview

| Component | Role |
|---|---|
| **External Repo** | Source of truth for model files. Connected to the cluster via a 10 Gbps link. |
| **Seed Node** | The first cluster node to download the complete model from the External Repo. |
| **Peer Nodes** | All other cluster nodes. Download chunks from the Seed Node and from each other. |
| **Centralized Tracker** | Control-plane service. Maintains global chunk inventory (bitmaps), bandwidth states, and computes optimal peer lists on request. |
| **Manifest Service** | Stores and serves the file manifest (version, total size, chunk count, chunk hashes). |

### 4.2 Data Flow

1. Seed Node downloads the full model from the External Repo at 10 Gbps.
2. Seed Node registers completed chunks with the Tracker (bitmap updates).
3. Each Peer Node fetches the Manifest and begins requesting chunk assignments from the Tracker.
4. Tracker returns a prioritized list of candidate source nodes for each requested chunk (applying Rarity-First or Topology-Aware policy).
5. Peer Nodes transfer chunks directly node-to-node (data plane); no chunk data passes through the Tracker.
6. After each chunk is received, the Peer Node verifies its SHA-256 hash, marks it complete, and reports back to the Tracker.
7. Distribution job is complete when all nodes report 100% bitmap coverage.

### 4.3 Control Plane vs. Data Plane Separation

The Tracker operates exclusively as a control plane. It never relays chunk data. This separation ensures that Tracker load stays proportional to the number of peer-selection queries — O(nodes × chunks) — not the data volume, dramatically reducing the risk of the Tracker becoming a bottleneck.

---

## 5. Key Subsystems

### 5.1 File Manifest

Before any transfer begins, a Manifest is generated and stored in the Manifest Service.

| Field | Description |
|---|---|
| `file_id` | Unique identifier for this model version. |
| `version` | Semantic version string (e.g., `2.1.0`). |
| `total_size` | Total file size in bytes. |
| `chunk_size` | Nominal chunk size in bytes (configurable; default 50 MB). |
| `chunk_count` | Total number of chunks — `ceil(total_size / chunk_size)`. |
| `chunk_hashes` | Array of SHA-256 hashes, one per chunk, for integrity verification. |
| `created_at` | UTC timestamp of manifest creation. |

For a 500 GB model with 50 MB chunks: `chunk_count ≈ 10,000`. The manifest itself is lightweight (~640 KB) and can be distributed to all nodes at the start of a job.

### 5.2 Chunk Sizing

- Configurable per distribution job; default **50 MB**.
- Too small (e.g., 64 KB) → millions of chunks, excessive metadata overhead and Tracker load.
- Too large (e.g., 1 GB) → coarse parallelism, poor pipelining.
- 50 MB strikes a practical balance for models in the hundreds-of-GB range.
- Recommended: analyze production file-size distributions and tune via config file rather than hardcoding.

### 5.3 Chunk Scheduling Strategies

Two strategies are provided. Operators select one via the distribution job configuration.

#### 5.3.1 Rarity-First (Global Scarcity Scheduling)

The Tracker prioritizes assigning chunks that are held by the fewest nodes across the cluster.

| Dimension | Advantage | Disadvantage |
|---|---|---|
| Speed of saturation | Rare chunks spread faster, pushing more nodes into the uploader role sooner. | Requires global chunk inventory view (already provided by Tracker). |
| Long-tail risk | Reduces probability of a chunk being scarce at job end. | May route requests to distant racks, increasing cross-rack traffic. |
| Recovery | Single-node failure is less likely to eliminate a unique chunk. | Local efficiency is lower; neighbors may hold many chunks yet be bypassed. |

#### 5.3.2 Topology-Aware (Local-First Scheduling)

Nodes preferentially download from neighbors on the same rack or switch before seeking remote sources.

| Dimension | Advantage | Disadvantage |
|---|---|---|
| Network stability | Minimizes cross-rack traffic; reduces risk of congestion and jitter. | Locality clusters can stall on the same missing chunks (long-tail risk). |
| QoS compatibility | Traffic patterns are predictable; easier to apply rate limiting. | Global completion time can lag behind Rarity-First. |
| Simplicity | Peer selection logic is straightforward (prefer same rack/pod first). | "Everyone missing the same chunk" deadlock needs a fallback escape. |

#### 5.3.3 Recommended Approach

Default to **Rarity-First** for fastest completion. Expose both strategies as a configuration option so operators can tune based on their network topology and operational constraints.

### 5.4 Tracker Design

#### 5.4.1 State Maintained per Node

- **Chunk bitmap** — a bitfield of length `chunk_count` indicating which chunks the node has fully received and verified.
- **Available upload bandwidth** — reported periodically by the node agent; used by the Tracker for load-aware peer selection.
- **Last-seen timestamp** — for liveness detection.

#### 5.4.2 Tracker API

| Endpoint | Method | Description |
|---|---|---|
| `/jobs/{job_id}/peers/{chunk_id}` | `GET` | Returns ranked list of candidate peers for a given chunk; applies scheduling policy. |
| `/jobs/{job_id}/nodes/{node_id}` | `PATCH` | Node reports its latest bitmap and available bandwidth. |
| `/jobs/{job_id}/status` | `GET` | Returns aggregate job progress: % complete, estimated ETA, per-node summary. |
| `/jobs` | `POST` | Create a new distribution job; provide manifest and scheduling policy. |
| `/jobs/{job_id}` | `DELETE` | Cancel an in-progress distribution job. |

#### 5.4.3 Tracker High Availability

- Deploy at least one hot standby Tracker replica, with state replicated via consensus (e.g., Raft).
- If the primary Tracker becomes unavailable, nodes fall back to a fully decentralized gossip-based peer discovery mode, eliminating global optimization but maintaining forward progress.
- Tracker state can be reconstructed by polling all nodes for their current bitmaps upon leader election.

---

## 6. Long-Tail Mitigation

The final phase of any chunked transfer is susceptible to a long-tail effect: a small number of chunks remain scarce while bandwidth concentrates on those few source nodes, creating hotspots and stalling completion.

### 6.1 Root Causes

- Uneven chunk propagation during the main transfer phase.
- Node failures or restarts that eliminate the only holder of a rare chunk.
- Topology-Aware scheduling amplifying locality gaps — adjacent nodes may all be missing the same chunk.

### 6.2 Erasure Coding (EC) at Tail Phase

Erasure Coding is applied at the tail of the distribution job to reduce long-tail latency.

| Field | Detail |
|---|---|
| **Principle** | Encode `k` data chunks into `k+m` coded chunks. Any `k` of the `k+m` are sufficient to reconstruct the originals. |
| **Application** | When the job enters the tail phase (e.g., >95% of nodes complete), remaining incomplete chunks are EC-encoded and broadcast from multiple source nodes simultaneously. |
| **Overhead** | Moderate increase in total bytes transferred (controlled by code rate `m/k`), but significant reduction in wall-clock completion time. |
| **Trigger** | Configurable threshold (e.g., activate EC when fewer than 5% of chunks remain globally unconfirmed). |

EC shifts the bottleneck away from scarce originals by generating redundant coded blocks that many nodes can seed simultaneously, allowing late-completing nodes to reconstruct missing chunks from any sufficient subset.

---

## 7. Incremental Model Updates

When a model is updated, it is often unnecessary to redistribute the full file. Two complementary mechanisms reduce update transfer volume.

### 7.1 Delta Patch

- The Manifest Service computes a binary diff between the new and old model versions.
- Only changed chunks are distributed; unchanged chunks are already present on all nodes.
- The delta patch manifest lists: chunk indices to delete, new/modified chunks with their hashes.

### 7.2 LoRA / Adapter Distribution

When only a fine-tuned adapter or LoRA (Low-Rank Adaptation) is updated rather than the base model weights, only the adapter file — typically orders of magnitude smaller than the full model — is distributed via the same P2P pipeline, making update latency negligible.

---

## 8. Observability

### 8.1 Metrics

- **Job-level:** Overall % complete, estimated time to completion (ETC), total bytes transferred, current aggregate throughput.
- **Node-level:** Bitmap coverage %, current download/upload speed, number of active peer connections, chunks awaiting verification.
- **Chunk-level:** Per-chunk replica count across all nodes (for rarity scoring and long-tail detection).
- **Tracker:** Query rate, peer-selection latency (p50/p95/p99), memory footprint of global bitmap state.

### 8.2 Alerting

- Alert when any node stalls for > N seconds without a new completed chunk (configurable threshold).
- Alert when replica count for any chunk drops to 1 (single point of chunk loss).
- Alert when estimated ETC exceeds the SLA deadline.

### 8.3 Dashboard

- Real-time heatmap of per-node completion progress.
- Chunk rarity histogram updated every 30 s.
- Network topology view showing cross-rack vs. intra-rack transfer split.

---

## 9. Scalability Analysis

| Scenario | Estimate |
|---|---|
| **Baseline (pipeline)** | ~800 s — single chain, 5 Gbps effective BW. |
| **Multi-source (4 nodes)** | ~600 s — 2 rounds of parallel seeding. |
| **P2P at scale** | With N nodes fully seeding, effective aggregate upload BW = N × 10 Gbps; completion time approaches `chunk_size / node_upload_bw` as N → ∞. |
| **Tracker state size** | 10,000 chunks × 1,000 nodes = 10 M bits ≈ 1.25 MB of bitmap state — trivially in-memory. |
| **Tracker query rate** | 10,000 chunks × 1,000 nodes / transfer_duration — well within a single service's capacity. |

---

## 10. Failure Modes & Mitigations

| Failure | Impact | Mitigation |
|---|---|---|
| Peer node crash | Chunks in transit are lost; other nodes may need alternative peers. | Tracker detects stale last-seen; re-routes affected downloaders to alternative peers. |
| Tracker crash | No peer-selection optimization until recovery. | Hot standby via Raft. Fallback to gossip-based peer discovery. |
| Network partition | Subset of nodes cannot reach peers in the other partition. | Topology-Aware scheduling keeps intra-rack traffic alive. Job resumes when partition heals. |
| Chunk hash mismatch | Corrupted chunk received from a peer. | Discard and re-request from a different peer. Repeated failures flag the source peer. |
| Seed node loss | Primary source of the model becomes unavailable. | Any fully-downloaded node can seed. Chunk rarity detection escalates re-seeding priority. |
| Long tail | Job stalls on final few rare chunks. | Rarity-First scheduling + Erasure Coding activation at tail threshold. |

---

## 11. Design Decision Summary

| Decision | Choice | Rationale |
|---|---|---|
| Peer topology | Controlled P2P | Parallelism without losing observability or scheduling intelligence. |
| Coordination model | Centralized Tracker | Global view enables optimal scheduling; data plane remains distributed. |
| Chunk size | 50 MB (configurable) | Balances parallelism overhead vs. metadata cost for large models. |
| Integrity check | SHA-256 per chunk | Industry standard; fast enough not to be a bottleneck at chunk granularity. |
| Scheduling policy | Both Rarity-First & Topo-Aware (user config) | Different cluster topologies have different optimal strategies. |
| Long-tail handling | Erasure Coding at tail | Reduces final-phase completion time without penalizing the main transfer. |
| Tracker HA | Raft + gossip fallback | Eliminates single point of failure while maintaining graceful degradation. |
| Update strategy | Delta patch + LoRA adapter | Avoids redistributing hundreds of GB for minor model changes. |

---

## 12. Open Questions & Future Work

- **Security:** Should chunk transfers be authenticated/encrypted intra-cluster? TLS adds latency; consider whether the cluster network is trusted.
- **Compression:** Model weight files may not compress well; profiling needed before enabling in-transit compression.
- **Cross-datacenter:** This design assumes a single datacenter. Multi-region distribution would require inter-DC bandwidth accounting and potentially a federated Tracker hierarchy.
- **Prioritization:** Should high-priority jobs pre-empt in-flight transfers on shared nodes? Requires a job scheduler layer.
- **Erasure code parameters:** Optimal `k` and `m` values for the EC scheme require empirical tuning against production traffic patterns.
