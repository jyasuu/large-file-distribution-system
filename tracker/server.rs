use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, error, info, warn};

use crate::error::Result;
use crate::manifest::Manifest;
use crate::proto::{read_msg, write_msg, Request, Response};
use crate::tracker::{SchedulingPolicy, Tracker};

/// Main TCP accept loop for the Tracker control-plane server.
pub async fn run(tracker: Arc<Tracker>, addr: &str) -> Result<()> {
    let listener = TcpListener::bind(addr).await?;
    info!("tracker listening on {addr}");

    // Background task: evict stale nodes every 30 s.
    {
        let t = tracker.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(
                tokio::time::Duration::from_secs(30)
            );
            loop {
                interval.tick().await;
                t.evict_stale_nodes(60);
                debug!("stale node eviction pass done");
            }
        });
    }

    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                debug!("new connection from {peer}");
                let tracker = tracker.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(tracker, stream).await {
                        warn!("connection error: {e}");
                    }
                });
            }
            Err(e) => error!("accept error: {e}"),
        }
    }
}

/// Handle one TCP connection: read a single request, write response, close.
/// Each connection = one request/response pair (stateless design).
async fn handle_connection(tracker: Arc<Tracker>, mut stream: TcpStream) -> Result<()> {
    let req: Request = match read_msg(&mut stream).await {
        Ok(r) => r,
        Err(e) => {
            let _ = write_msg(&mut stream, &Response::Error {
                message: e.to_string(),
            }).await;
            return Err(e);
        }
    };

    let resp = dispatch(&tracker, req).await;
    write_msg(&mut stream, &resp).await?;
    Ok(())
}

async fn dispatch(tracker: &Tracker, req: Request) -> Response {
    match req {
        // ── Create job ─────────────────────────────────────────────────────
        Request::CreateJob { manifest_bytes, policy } => {
            let manifest = match Manifest::deserialise(&manifest_bytes) {
                Ok(m) => m,
                Err(e) => return Response::Error { message: e.to_string() },
            };
            let policy = match policy.parse::<SchedulingPolicy>() {
                Ok(p) => p,
                Err(e) => return Response::Error { message: e.to_string() },
            };
            let job = tracker.create_job(manifest, policy);
            info!(job_id = %job.job_id, "job created");
            Response::JobCreated { job_id: job.job_id.clone() }
        }

        // ── Node heartbeat ─────────────────────────────────────────────────
        Request::Heartbeat {
            job_id, node_id, rack_id, peer_addr, bitmap_bytes, upload_bw_mbps
        } => {
            match tracker.update_node(
                &job_id, node_id.clone(), rack_id, peer_addr,
                &bitmap_bytes, upload_bw_mbps
            ) {
                Ok(()) => {
                    let job_complete = tracker.get_job(&job_id)
                        .map(|j| {
                            use crate::tracker::JobStatus;
                            *j.status.read() == JobStatus::Complete
                        })
                        .unwrap_or(false);
                    Response::HeartbeatAck { job_complete }
                }
                Err(e) => Response::Error { message: e.to_string() },
            }
        }

        // ── Get peers for a chunk ──────────────────────────────────────────
        Request::GetPeers { job_id, node_id, rack_id, chunk_index, top_n } => {
            match tracker.get_peers(&job_id, &node_id, &rack_id, chunk_index, top_n) {
                Ok(peers) => Response::Peers {
                    peers: peers.into_iter().map(Into::into).collect(),
                },
                Err(e) => Response::Error { message: e.to_string() },
            }
        }

        // ── Job status ─────────────────────────────────────────────────────
        Request::GetStatus { job_id } => {
            match tracker.get_job(&job_id) {
                Ok(job) => {
                    let snapshot = job.snapshot();
                    match serde_json::to_value(&snapshot) {
                        Ok(v)  => Response::Status { body: v },
                        Err(e) => Response::Error { message: e.to_string() },
                    }
                }
                Err(e) => Response::Error { message: e.to_string() },
            }
        }

        // ── Cancel job ─────────────────────────────────────────────────────
        Request::CancelJob { job_id } => {
            match tracker.cancel_job(&job_id) {
                Ok(()) => Response::Ok,
                Err(e) => Response::Error { message: e.to_string() },
            }
        }

        // ── List all jobs ──────────────────────────────────────────────────
        Request::ListJobs => {
            let jobs = tracker.all_job_snapshots()
                .into_iter()
                .filter_map(|s| serde_json::to_value(s).ok())
                .collect();
            Response::JobList { jobs }
        }
    }
}
