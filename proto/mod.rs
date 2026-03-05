pub mod client;

use bytes::{Buf, BufMut, Bytes, BytesMut};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::error::{DistError, Result};

// ─────────────────────────────────────────────────────────────────────────────
// Wire format
//
//  ┌──────────────────────────────┐
//  │  4 bytes  │  payload length  │
//  │  N bytes  │  JSON payload    │
//  └──────────────────────────────┘
//
// Simple length-prefixed JSON framing over raw TCP.
// Sufficient for control-plane messages; chunk data travels separately
// via a dedicated raw TCP stream (see peer module).
// ─────────────────────────────────────────────────────────────────────────────

pub const MAX_FRAME: usize = 16 * 1024 * 1024; // 16 MB safety cap

pub async fn write_msg<T: Serialize>(stream: &mut TcpStream, msg: &T) -> Result<()> {
    let payload = serde_json::to_vec(msg)?;
    let len     = payload.len() as u32;

    let mut buf = BytesMut::with_capacity(4 + payload.len());
    buf.put_u32(len);
    buf.put_slice(&payload);
    stream.write_all(&buf).await?;
    Ok(())
}

pub async fn read_msg<T: for<'de> Deserialize<'de>>(stream: &mut TcpStream) -> Result<T> {
    let len = stream.read_u32().await? as usize;
    if len > MAX_FRAME {
        return Err(DistError::Proto(format!("frame too large: {len}")));
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    Ok(serde_json::from_slice(&buf)?)
}

// ─────────────────────────────────────────────────────────────────────────────
// Message types (control plane — Tracker ↔ Node / Client)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// Create a new distribution job.
    CreateJob {
        manifest_bytes: Vec<u8>,
        policy:         String,
    },
    /// Node heartbeat: I exist, here is my bitmap.
    Heartbeat {
        job_id:         String,
        node_id:        String,
        rack_id:        String,
        peer_addr:      String,
        bitmap_bytes:   Vec<u8>,
        upload_bw_mbps: u64,
    },
    /// Request peer list for a specific chunk.
    GetPeers {
        job_id:         String,
        node_id:        String,
        rack_id:        String,
        chunk_index:    u32,
        top_n:          usize,
    },
    /// Retrieve job status snapshot.
    GetStatus { job_id: String },
    /// Cancel a job.
    CancelJob { job_id: String },
    /// List all jobs.
    ListJobs,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    /// Job created successfully.
    JobCreated { job_id: String },
    /// Heartbeat acknowledged.
    HeartbeatAck { job_complete: bool },
    /// Peer list for a chunk.
    Peers { peers: Vec<PeerInfo> },
    /// Serialised JobSnapshot.
    Status { body: serde_json::Value },
    /// List of all job snapshots.
    JobList { jobs: Vec<serde_json::Value> },
    /// Generic acknowledgement.
    Ok,
    /// Error response.
    Error { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    pub node_id:        String,
    pub peer_addr:      String,
    pub rack_id:        String,
    pub upload_bw_mbps: u64,
}

impl From<crate::tracker::CandidatePeer> for PeerInfo {
    fn from(p: crate::tracker::CandidatePeer) -> Self {
        Self {
            node_id:        p.node_id,
            peer_addr:      p.peer_addr,
            rack_id:        p.rack_id,
            upload_bw_mbps: p.upload_bw_mbps,
        }
    }
}
