# 🚀 Model Distribution System

> High-speed, fault-tolerant P2P model distribution for ML inference clusters.  
> Distribute 500 GB models to thousands of nodes in minutes, not hours.

[![Rust](https://img.shields.io/badge/rust-1.75+-orange?logo=rust)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/license-MIT-blue)](LICENSE)
[![Status](https://img.shields.io/badge/status-in--development-blue)]()

---

## 📖 Overview

When you need to roll out a new model version across a large inference cluster, naive approaches—copying from a single source, or chaining nodes in a pipeline—are too slow and too fragile. This system solves that with a **centralized-tracker-controlled P2P protocol** inspired by BitTorrent, but built for private datacenters.

- A **Centralized Tracker** holds a global view of which chunks every node owns and directs optimal peer selection — without ever touching the data itself.
- Nodes transfer chunks **directly to each other over raw TCP** (data plane), saturating all available intra-cluster bandwidth.
- **Erasure Coding** at the tail phase eliminates long-tail stalls on the final rare chunks.
- **Delta patching** means model updates only transfer what actually changed.
- Everything ships as a **single binary** with subcommands for server, node agent, and CLI client.

---

## ✨ Features

| Feature | Details |
|---|---|
| **Raw TCP chunk transfer** | Direct node-to-node transfer; no proxy, no HTTP overhead |
| **Chunked P2P transfer** | Configurable chunk size (default 50 MB); parallel multi-peer download |
| **Rarity-First scheduling** | Prioritise spreading the rarest chunks first for fastest saturation |
| **Topology-Aware scheduling** | Prefer same-rack peers to minimise cross-rack traffic |
| **SHA-256 integrity** | Every chunk verified on receipt; corrupt peers retried automatically |
| **Erasure Coding tail** | Reed-Solomon EC activates at >95% completion to eliminate long-tail stalls |
| **Delta patch updates** | Only changed chunks are redistributed on model version bump |
| **Prometheus metrics** | Per-node completion ratio, chunks served/received, peer query rate |
| **Stale node eviction** | Tracker evicts nodes that miss heartbeats; jobs continue uninterrupted |
| **Scalable** | From single-digit to thousands of nodes with no architecture change |

---

## 🏗️ Architecture

```
┌─────────────────────────────────────────────────────┐
│                  External Repo                      │
│          (model source, 10 Gbps link)               │
└───────────────────────┬─────────────────────────────┘
                        │ full model download
                        ▼
          ┌─────────────────────────┐
          │       Seed Node         │
          │  first complete copy    │
          └────────────┬────────────┘
                       │ bitmap heartbeat (TCP control plane)
                       ▼
┌──────────────────────────────────────────────────────┐
│                   Control Plane                      │
│  ┌────────────────────────────────────────────────┐  │
│  │             Centralized Tracker                │  │
│  │  • Global chunk bitmap (DashMap per node)      │  │
│  │  • Rarity-First / Topology-Aware scheduling    │  │
│  │  • Stale node eviction (30 s interval)         │  │
│  │  • Prometheus metrics endpoint (:9000)         │  │
│  └────────────────────────────────────────────────┘  │
└──────────────────────┬───────────────────────────────┘
                       │ peer-list queries / bitmap updates
                       │ (control only — no chunk data)
        ┌──────────────┴──────────────┐
        ▼                             ▼
  ┌───────────┐               ┌───────────┐
  │ Peer Node │◄─── chunk ───►│ Peer Node │   Raw TCP
  │   A-1     │    (TCP)      │   B-1     │   data plane
  └───────────┘               └─────┬─────┘
        ▲                           ▼
        └────────────── … N nodes ──┘
```

See the full diagrams in [`docs/`](docs/):

- [`architecture.mermaid`](docs/architecture.mermaid) — component & data-flow diagram
- [`sequence.mermaid`](docs/sequence.mermaid) — 7-phase sequence diagram

---

## 📐 Design Documents

| Document | Description |
|---|---|
| [`docs/spec.md`](docs/spec.md) | Full system design specification (12 sections) |
| [`docs/spec.docx`](docs/spec.docx) | Word version of the spec |
| [`docs/architecture.mermaid`](docs/architecture.mermaid) | System architecture diagram |
| [`docs/sequence.mermaid`](docs/sequence.mermaid) | End-to-end sequence diagram |

---

## 🗂️ Project Structure

```
model-distribution/
├── Cargo.toml
├── README.md
├── docs/
│   ├── spec.md
│   ├── spec.docx
│   ├── architecture.mermaid
│   └── sequence.mermaid
└── src/
    ├── main.rs                  # CLI: server / node / client / self-test subcommands
    ├── error.rs                 # Unified DistError type (thiserror)
    ├── manifest.rs              # Manifest build + async file streaming, ChunkBitmap
    ├── metrics.rs               # Prometheus exporter setup, metric descriptions
    ├── erasure.rs               # ErasureCoder (Reed-Solomon k+m), tail-phase controller
    ├── delta.rs                 # DeltaPatch compute/apply, PatchApplicator
    ├── tracker/
    │   ├── mod.rs               # Tracker state (DashMap), heartbeat, job lifecycle
    │   ├── scheduler.rs         # Rarity-First + Topology-Aware peer selection
    │   └── server.rs            # TCP accept loop, request dispatch
    ├── peer/
    │   ├── mod.rs               # PeerNode: orchestrates heartbeat + server + downloader
    │   ├── downloader.rs        # Async download loop: Tracker query → TCP fetch → verify
    │   └── chunk_server.rs      # Raw TCP server: serves verified chunks to peers
    └── proto/
        ├── mod.rs               # Length-prefixed JSON framing, Request/Response types
        └── client.rs            # TrackerClient (create job, heartbeat, get_peers, status)
```

---

## ⚡ Quick Start

### Prerequisites

- Rust 1.75+
- `cargo`

### Build

```bash
git clone https://github.com/jyasuu/large-file-distribution-system
cd large-file-distribution-system
cargo build --release
```

### Run the in-process self-test (no network needed)

```bash
cargo run --release -- self-test
```

Expected output:

```
INFO self-test: manifest
INFO   chunk_count=4 ✓
INFO self-test: delta patch
INFO   changed=1 unchanged=3 ✓
INFO self-test: erasure coding
INFO   reconstruct shards [1,3] ✓
INFO self-test: bitmap
INFO   completion_ratio=0.2 ✓
INFO ✅  all self-tests passed
```

### Run tests

```bash
cargo test
```

---

## 🖥️ Usage

### Start the server (Tracker + metrics endpoint)

```bash
model-dist server \
  --tracker-addr 0.0.0.0:7000 \
  --metrics-addr 0.0.0.0:9000
```

### Create a distribution job

```bash
model-dist client \
  --tracker-addr 127.0.0.1:7000 \
  create-job \
  --file-path /data/llama-3-70b.bin \
  --file-id   llama-3-70b \
  --version   2.0.0 \
  --chunk-size-mb 50 \
  --policy    rarity-first
```

### Start a peer node agent

```bash
model-dist node \
  --tracker-addr 127.0.0.1:7000 \
  --peer-addr    0.0.0.0:7100 \
  --rack-id      rack-A \
  --job-id       <JOB_ID> \
  --data-dir     /var/lib/model-dist \
  --concurrency  8
```

### Check job status

```bash
model-dist client --tracker-addr 127.0.0.1:7000 status <JOB_ID>
```

### Cancel a job

```bash
model-dist client --tracker-addr 127.0.0.1:7000 cancel <JOB_ID>
```

---

## 🔌 Wire Protocol

The Tracker uses **raw TCP with length-prefixed JSON framing** for all control-plane messages. Chunk data travels on a separate raw TCP connection directly between peer nodes — the Tracker never relays chunk bytes.

```
Control-plane frame:
  ┌──────────────┬──────────────────┐
  │  4 bytes     │  N bytes         │
  │  payload len │  JSON payload    │
  └──────────────┴──────────────────┘

Chunk data frame (peer → peer):
  Request:   4-byte job_id_len | job_id bytes | 4-byte chunk_index
  Response:  4-byte status | 8-byte data_len | N bytes chunk data
```

---

## 🔧 Configuration

**Node agent flags:**

| Flag | Default | Description |
|---|---|---|
| `--tracker-addr` | `127.0.0.1:7000` | Tracker TCP address |
| `--peer-addr` | `0.0.0.0:7100` | This node's chunk-server listen address |
| `--rack-id` | `rack-0` | Rack identifier for topology-aware scheduling |
| `--data-dir` | `/tmp/model-dist` | Directory where chunk files are written |
| `--concurrency` | `8` | Max concurrent chunk downloads |

**Job creation flags:**

| Flag | Default | Description |
|---|---|---|
| `--chunk-size-mb` | `50` | Chunk size in MB; tune to file-size distribution |
| `--policy` | `rarity-first` | `rarity-first` or `topology-aware` |

**Erasure coding** (configured in `TailConfig` in `src/erasure.rs`):

| Field | Default | Description |
|---|---|---|
| `activation_threshold` | `0.95` | Fraction of nodes complete before EC activates |
| `data_shards` | `4` | Reed-Solomon k |
| `parity_shards` | `2` | Reed-Solomon m — any k-of-(k+m) shards reconstruct the data |
| `scarce_threshold` | `3` | Min replica count before a chunk is considered scarce |

---

## 📊 Performance Estimates

| Scenario | Time |
|---|---|
| 500 GB, single pipeline | ~800 s |
| 500 GB, multi-source (4 nodes) | ~600 s |
| 500 GB, full P2P at scale (N nodes) | → `chunk_size / node_bw` as N grows |
| Tracker bitmap state (10k chunks × 1k nodes) | ~1.25 MB RAM |

---

## 🧩 Key Dependencies

```toml
tokio                       = { version = "1", features = ["full"] }
tokio-util                  = { version = "0.7", features = ["codec", "io"] }
bytes                       = "1"
sha2                        = "0.10"
hex                         = "0.4"
reed-solomon-erasure        = "6"
bitvec                      = "1"
serde                       = { version = "1", features = ["derive"] }
serde_json                  = "1"
tracing                     = "0.1"
tracing-subscriber          = { version = "0.3", features = ["env-filter", "fmt"] }
metrics                     = "0.21"
metrics-exporter-prometheus = "0.12"
clap                        = { version = "4", features = ["derive"] }
uuid                        = { version = "1", features = ["v4"] }
dashmap                     = "5"
parking_lot                 = "0.12"
anyhow                      = "1"
thiserror                   = "1"
rand                        = "0.8"
```

---

## 🔄 Scheduling Policies

### Rarity-First _(default)_

Spreads the rarest chunks across the cluster first. Maximises the number of nodes that can act as uploaders early, driving exponential propagation. Best for fastest overall completion.

```
candidate peers = nodes holding chunk_index, excluding requester
ranked by:        upload_bw_mbps DESC
```

The download loop orders missing chunks by `holder_count ASC` (rarest first) before querying the Tracker.

### Topology-Aware

Prefers peers on the same rack or pod before going cross-rack. Keeps traffic local, reduces network jitter, and plays well with QoS policies.

```
same-rack peers  → sorted by upload_bw_mbps DESC   (tried first)
remote peers     → sorted by upload_bw_mbps DESC   (fallback)
```

> Both strategies are available simultaneously — set `--policy` per job at creation time.

---

## 🛡️ Fault Tolerance

| Failure | Behaviour |
|---|---|
| Peer node crash | Tracker evicts stale node after 60 s; downloaders retry next candidate peer automatically |
| Chunk corruption | SHA-256 mismatch → chunk discarded, retried from a different peer (up to 3 attempts with exponential backoff) |
| Peer chunk not found | Chunk server returns error status; downloader moves to next candidate immediately |
| Long tail | Erasure Coding activates at the configurable threshold; parity shards distributed to stalled nodes |
| Seed node loss | Any node that completed its download can serve chunks; rarity scoring escalates seeding priority |

> Tracker HA (Raft + gossip fallback) is a planned feature — see Roadmap.

---

## 🔭 Observability

Prometheus metrics are exposed at `http://<metrics-addr>/metrics`:

| Metric | Type | Description |
|---|---|---|
| `tracker_jobs_created_total` | Counter | Total distribution jobs created |
| `tracker_jobs_complete_total` | Counter | Jobs where all nodes reached 100% |
| `tracker_peer_queries_total` | Counter | Peer-selection queries served |
| `peer_chunks_received_total` | Counter | Chunks downloaded and verified by peer nodes |
| `peer_chunks_served_total` | Counter | Chunks served to other peers |
| `node_completion_ratio` | Gauge | Cluster-wide completion ratio (0.0–1.0) |

> Per-node and per-job context is emitted via `tracing` structured logs alongside these metrics.

---

## 🗺️ Roadmap

- [ ] Tracker HA — Raft-based hot standby + gossip fallback mode
- [ ] TLS mutual auth for intra-cluster chunk transfers
- [ ] Adaptive EC parameter tuning based on live traffic patterns
- [ ] Multi-datacenter federated Tracker hierarchy
- [ ] Job preemption and priority queuing
- [ ] In-transit compression (model-aware; profile before enabling)
- [ ] gRPC control plane (replace length-prefixed JSON framing)
- [ ] Web dashboard — real-time node heatmap and chunk rarity histogram

---

## 📄 License

MIT — see [LICENSE](LICENSE).

---

## 🙏 References

- [BitTorrent Protocol Specification](https://www.bittorrent.org/beps/bep_0003.html)
- [Reed-Solomon Erasure Coding](https://en.wikipedia.org/wiki/Reed%E2%80%93Solomon_error_correction)
- [LoRA: Low-Rank Adaptation of Large Language Models](https://arxiv.org/abs/2106.09685)
- System design walkthrough: [Model Distribution — YouTube](https://www.youtube.com/watch?v=G-JZq-GT3xc)