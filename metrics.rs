use metrics_exporter_prometheus::PrometheusBuilder;
use tracing::info;

use crate::error::Result;

/// Start the Prometheus scrape endpoint on `addr`.
pub async fn start_metrics_server(addr: &str) -> Result<()> {
    let builder = PrometheusBuilder::new();
    builder
        .with_http_listener(addr.parse::<std::net::SocketAddr>()
            .map_err(|e| crate::error::DistError::Proto(e.to_string()))?)
        .install()
        .map_err(|e| crate::error::DistError::Proto(e.to_string()))?;

    info!("prometheus metrics on http://{addr}/metrics");
    Ok(())
}

/// Register all application metrics with descriptions.
/// Called once at startup before any counter increments occur.
pub fn describe_metrics() {
    use metrics::describe_counter;
    use metrics::describe_gauge;

    describe_counter!(
        "tracker_jobs_created_total",
        "Total distribution jobs created."
    );
    describe_counter!(
        "tracker_jobs_complete_total",
        "Total distribution jobs where all nodes completed."
    );
    describe_counter!(
        "tracker_peer_queries_total",
        "Total peer-selection queries handled by the Tracker."
    );
    describe_counter!(
        "peer_chunks_received_total",
        "Total chunks successfully downloaded and verified by peer nodes."
    );
    describe_counter!(
        "peer_chunks_served_total",
        "Total chunks served to other peers by the chunk server."
    );
    describe_gauge!(
        "node_completion_ratio",
        "Per-node fraction of chunks completed (0.0 – 1.0)."
    );
}
