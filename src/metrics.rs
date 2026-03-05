use tracing::info;
use crate::error::Result;

/// Start the Prometheus scrape endpoint on `addr`.
pub async fn start_metrics_server(addr: &str) -> Result<()> {
    let socket: std::net::SocketAddr = addr.parse()
        .map_err(|e: std::net::AddrParseError| crate::error::DistError::Proto(e.to_string()))?;

    metrics_exporter_prometheus::PrometheusBuilder::new()
        .with_http_listener(socket)
        .install()
        .map_err(|e| crate::error::DistError::Proto(e.to_string()))?;

    info!("prometheus metrics on http://{addr}/metrics");
    Ok(())
}

/// Register all application metrics with descriptions.
pub fn describe_metrics() {
    metrics::describe_counter!(
        "tracker_jobs_created_total",
        "Total distribution jobs created."
    );
    metrics::describe_counter!(
        "tracker_jobs_complete_total",
        "Total distribution jobs where all nodes completed."
    );
    metrics::describe_counter!(
        "tracker_peer_queries_total",
        "Total peer-selection queries handled by the Tracker."
    );
    metrics::describe_counter!(
        "peer_chunks_received_total",
        "Total chunks successfully downloaded and verified by peer nodes."
    );
    metrics::describe_counter!(
        "peer_chunks_served_total",
        "Total chunks served to other peers by the chunk server."
    );
    metrics::describe_gauge!(
        "node_completion_ratio",
        "Per-node fraction of chunks completed (0.0 – 1.0)."
    );
}
