# 🚀 Model Distribution System

> High-speed, fault-tolerant P2P model distribution for ML inference clusters.  
> Distribute 500 GB models to thousands of nodes in minutes, not hours.

[![Rust](https://img.shields.io/badge/rust-1.75+-orange?logo=rust)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/license-MIT-blue)](LICENSE)
[![Status](https://img.shields.io/badge/status-draft-yellow)]()

---

## 📖 Overview

When you need to roll out a new model version across a large inference cluster, naive approaches—copying from a single source, or chaining nodes in a pipeline—are too slow and too fragile. This system solves that with a **centralized-tracker-controlled P2P protocol** inspired by BitTorrent, but built for private datacenters.

- A **Centralized Tracker** holds a global view of which chunks every node owns and directs optimal peer selection — without ever touching the data itself.
- Nodes transfer chunks **directly to each other** (data plane), saturating all available intra-cluster bandwidth.
- **Erasure Coding** at the tail phase eliminates long-tail stalls on the final rare chunks.
- **Delta patching** means model updates only transfer what changed.

---

## ✨ Features

| Feature | Details |
|---|---|
| **Chunked P2P transfer** | Configurable chunk size (default 50 MB); parallel multi-peer download |
| **Rarity-First scheduling** | Prioritise spreading the rarest chunks first for fastest saturation |
| **Topology-Aware scheduling** | Prefer same-rack peers to minimise cross-rack traffic |
| **SHA-256 integrity** | Every chunk verified on receipt; corrupt peers are blacklisted |
| **Erasure Coding tail** | Reed-Solomon EC activates at >95% completion to kill long-tail stalls |
| **Delta patch updates** | Only changed chunks are redistributed on model version bump |
| **LoRA adapter support** | Distribute fine-tuned adapters independently of the base model |
| **Real-time observability** | Per-node heatmap, chunk rarity histogram, ETC, bandwidth metrics |
| **Tracker HA** | Raft-based hot standby; falls back to gossip P2P if Tracker is unavailable |
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
                       │ reports bitmap
                       ▼
┌──────────────────────────────────────────────┐
│            Control Plane                     │
│  ┌──────────────────┐  ┌──────────────────┐  │
│  │  Centralized     │  │  Manifest        │  │
│  │  Tracker         │  │  Service         │  │
│  │  • Global bitmap │  │  • chunk_hashes  │  │
│  │  • Peer ranking  │  │  • version       │  │
│  │  • BW state      │  │  • chunk_count   │  │
│  └──────────────────┘  └──────────────────┘  │
└──────────────────┬───────────────────────────┘
                   │ peer-list queries / bitmap updates
                   │ (control only — no data flows here)
        ┌──────────┴──────────┐
        ▼                     ▼
  ┌───────────┐         ┌───────────┐
  │ Peer Node │◄───────►│ Peer Node │   Direct P2P
  │   A-1     │         │   B-1     │   chunk transfer
  └───────────┘         └─────┬─────┘   (data plane)
        ▲                     ▼
        └──────────────────────────── … N nodes
```

See the full diagrams in [`docs/`](docs/):

- [`architecture.mermaid`](docs/architecture.mermaid) — component & data-flow diagram
- [`sequence.mermaid`](docs/sequence.mermaid) — 7-phase sequence diagram

---

## 📐 Design Documents

| Document | Description |
|---|---|
| [`docs/spec.md`](docs/spec.md) | Full system design specification |
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
    ├── main.rs               # entry point & integration smoke-test
    ├── manifest.rs           # Manifest build, chunk range, serialisation
    ├── bitmap.rs             # ChunkBitmap — bitvec-backed, SHA-256 verify
    ├── tracker/
    │   ├── mod.rs            # Tracker state, update_node
    │   ├── rarity.rs         # Rarity-First peer selection
    │   └── topology.rs       # Topology-Aware peer selection
    ├── peer.rs               # PeerNode async download loop
    ├── erasure.rs            # Reed-Solomon EC — tail-phase coder
    └── delta.rs              # DeltaPatch — compute, apply, chunks_to_fetch
```

---

## ⚡ Quick Start

### Prerequisites

- Rust 1.75+
- `cargo`

### Build

```bash
git clone https://github.com/your-org/model-distribution
cd model-distribution
cargo build --release
```

### Run the smoke-test

```bash
cargo run --release
```

Expected output:

```
Manifest: 2 chunks
PeerNode ready — would call peer.run().await in production
Delta patch v2.0.0 → v2.0.1: 1 unchanged, 1 changed, 0 deleted
EC encode OK — 6 shards total (4 data + 2 parity)
EC reconstruct OK — missing shards recovered
```

### Run tests

```bash
cargo test
```

---

## 🔧 Configuration

Distribution jobs are configured via a JSON payload when creating a job:

```json
{
  "manifest_id": "llama-3-70b-v2.0.0",
  "scheduling_policy": "rarity-first",
  "chunk_size_mb": 50,
  "max_concurrent_chunks": 8,
  "ec_tail_threshold": 0.95,
  "ec_data_shards": 4,
  "ec_parity_shards": 2
}
```

| Field | Default | Description |
|---|---|---|
| `scheduling_policy` | `rarity-first` | `rarity-first` or `topology-aware` |
| `chunk_size_mb` | `50` | Chunk size in MB. Tune per file-size distribution. |
| `max_concurrent_chunks` | `8` | Max parallel chunk downloads per node. |
| `ec_tail_threshold` | `0.95` | Fraction of nodes complete before EC activates. |
| `ec_data_shards` | `4` | Reed-Solomon k (data shards). |
| `ec_parity_shards` | `2` | Reed-Solomon m (parity shards). |

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
[dependencies]
tokio                  = { version = "1", features = ["full"] }
sha2                   = "0.10"
bitvec                 = "1"
reqwest                = { version = "0.11", features = ["stream"] }
bytes                  = "1"
serde                  = { version = "1", features = ["derive"] }
serde_json             = "1"
reed-solomon-erasure   = "6"
anyhow                 = "1"
tracing                = "0.1"
tracing-subscriber     = "0.3"
```

---

## 🔄 Scheduling Policies

### Rarity-First _(default)_

Spreads the rarest chunks across the cluster first. Maximises the number of nodes that can act as uploaders early, driving exponential propagation. Best for fastest overall completion.

```
chunk_priority = sort by holder_count ASC, then by peer_upload_bw DESC
```

### Topology-Aware

Prefers peers on the same rack or pod before going cross-rack. Keeps traffic local, reduces network jitter, and plays well with QoS policies.

```
peer_priority = same_rack FIRST, then by upload_bw DESC
```

> Both strategies are available simultaneously — set `scheduling_policy` per job.

---

## 🛡️ Fault Tolerance

| Failure | Behaviour |
|---|---|
| Peer node crash | Tracker detects stale heartbeat; re-routes downloaders to next candidate |
| Chunk corruption | SHA-256 mismatch → discard, retry from different peer, flag bad source |
| Tracker primary down | Hot standby via Raft takes over; state reconstructed from node bitmaps |
| Tracker fully unavailable | Nodes fall back to gossip-based peer discovery — slower but keeps progressing |
| Long tail | Erasure Coding activates at configurable threshold, eliminates stall |
| Seed node loss | Any fully-downloaded node promotes itself as a seeder |

---

## 📡 Tracker API

| Endpoint | Method | Description |
|---|---|---|
| `/jobs` | `POST` | Create a distribution job |
| `/jobs/{job_id}/status` | `GET` | Job progress, ETC, per-node summary |
| `/jobs/{job_id}/peers/{chunk_id}` | `GET` | Ranked peer list for a chunk |
| `/jobs/{job_id}/nodes/{node_id}` | `PATCH` | Node heartbeat — bitmap + bandwidth |
| `/jobs/{job_id}` | `DELETE` | Cancel a job |

---

## 🔭 Observability

- **Metrics:** job % complete, per-node bitmap coverage, chunk rarity, tracker query latency (p50/p95/p99)
- **Alerts:** node stall > N s, chunk replica_count drops to 1, ETC exceeds SLA
- **Dashboard:** real-time heatmap, chunk rarity histogram, cross-rack vs. intra-rack traffic split

---

## 🗺️ Roadmap

- [ ] gRPC data plane (replace HTTP range requests)
- [ ] TLS mutual auth for intra-cluster chunk transfer
- [ ] Multi-datacenter federated Tracker hierarchy
- [ ] Job preemption / priority queuing
- [ ] In-transit compression profiling
- [ ] Adaptive EC parameter tuning based on live traffic

---

## 📄 License

MIT — see [LICENSE](LICENSE).

---

## 🙏 References

- [BitTorrent Protocol Specification](https://www.bittorrent.org/beps/bep_0003.html)
- [Reed-Solomon Erasure Coding](https://en.wikipedia.org/wiki/Reed%E2%80%93Solomon_error_correction)
- [LoRA: Low-Rank Adaptation of Large Language Models](https://arxiv.org/abs/2106.09685)
- System design walkthrough: [Model Distribution — YouTube](https://www.youtube.com/watch?v=G-JZq-GT3xc)
