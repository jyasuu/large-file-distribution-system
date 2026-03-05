use tokio::net::TcpStream;

use crate::error::{DistError, Result};
use crate::manifest::Manifest;
use crate::proto::{read_msg, write_msg, PeerInfo, Request, Response};

/// Simple synchronous-style client wrapping one TCP connection to the Tracker.
/// Each method opens a fresh connection (stateless; suitable for CLI use).
/// The PeerNode keeps a persistent connection and uses `send_recv` directly.
pub struct TrackerClient {
    addr: String,
}

impl TrackerClient {
    pub async fn connect(addr: &str) -> Result<Self> {
        // Eagerly verify the address is reachable.
        TcpStream::connect(addr).await?;
        Ok(Self { addr: addr.to_owned() })
    }

    async fn send_recv(&self, req: &Request) -> Result<Response> {
        let mut stream = TcpStream::connect(&self.addr).await?;
        write_msg(&mut stream, req).await?;
        read_msg::<Response>(&mut stream).await
    }

    // ── Public API ─────────────────────────────────────────────────────────

    pub async fn create_job(
        &mut self,
        file_path:     &str,
        file_id:       String,
        version:       String,
        chunk_size:    u64,
        policy:        String,
    ) -> Result<String> {
        let data     = tokio::fs::read(file_path).await?;
        let manifest = Manifest::build(file_id, version, &data, chunk_size)?;
        let req = Request::CreateJob {
            manifest_bytes: manifest.serialise()?,
            policy,
        };
        match self.send_recv(&req).await? {
            Response::JobCreated { job_id } => Ok(job_id),
            Response::Error { message }     => Err(DistError::Proto(message)),
            other => Err(DistError::Proto(format!("unexpected: {other:?}"))),
        }
    }

    pub async fn job_status(&mut self, job_id: &str) -> Result<serde_json::Value> {
        match self.send_recv(&Request::GetStatus { job_id: job_id.to_owned() }).await? {
            Response::Status { body } => Ok(body),
            Response::Error { message } => Err(DistError::Proto(message)),
            other => Err(DistError::Proto(format!("unexpected: {other:?}"))),
        }
    }

    pub async fn cancel_job(&mut self, job_id: &str) -> Result<()> {
        match self.send_recv(&Request::CancelJob { job_id: job_id.to_owned() }).await? {
            Response::Ok => Ok(()),
            Response::Error { message } => Err(DistError::Proto(message)),
            other => Err(DistError::Proto(format!("unexpected: {other:?}"))),
        }
    }

    pub async fn heartbeat(
        stream:         &mut tokio::net::TcpStream,
        job_id:         &str,
        node_id:        &str,
        rack_id:        &str,
        peer_addr:      &str,
        bitmap_bytes:   &[u8],
        upload_bw_mbps: u64,
    ) -> Result<bool> {
        let req = Request::Heartbeat {
            job_id:         job_id.to_owned(),
            node_id:        node_id.to_owned(),
            rack_id:        rack_id.to_owned(),
            peer_addr:      peer_addr.to_owned(),
            bitmap_bytes:   bitmap_bytes.to_vec(),
            upload_bw_mbps,
        };
        write_msg(stream, &req).await?;
        match read_msg::<Response>(stream).await? {
            Response::HeartbeatAck { job_complete } => Ok(job_complete),
            Response::Error { message } => Err(DistError::Proto(message)),
            other => Err(DistError::Proto(format!("unexpected: {other:?}"))),
        }
    }

    pub async fn get_peers(
        stream:      &mut tokio::net::TcpStream,
        job_id:      &str,
        node_id:     &str,
        rack_id:     &str,
        chunk_index: u32,
        top_n:       usize,
    ) -> Result<Vec<PeerInfo>> {
        let req = Request::GetPeers {
            job_id:      job_id.to_owned(),
            node_id:     node_id.to_owned(),
            rack_id:     rack_id.to_owned(),
            chunk_index,
            top_n,
        };
        write_msg(stream, &req).await?;
        match read_msg::<Response>(stream).await? {
            Response::Peers { peers } => Ok(peers),
            Response::Error { message } => Err(DistError::Proto(message)),
            other => Err(DistError::Proto(format!("unexpected: {other:?}"))),
        }
    }
}
