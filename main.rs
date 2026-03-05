mod manifest;
mod tracker;
mod peer;
mod erasure;
mod delta;
mod metrics;
mod error;
mod proto;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing::info;

/// Model Distribution System — single binary, all components.
#[derive(Parser)]
#[command(name = "model-dist", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the all-in-one server (Tracker + Manifest Service + metrics endpoint).
    Server(ServerArgs),

    /// Run a peer node agent that joins a distribution job.
    Node(NodeArgs),

    /// CLI client: create / inspect / cancel jobs.
    Client(ClientArgs),

    /// Smoke-test all subsystems in-process.
    SelfTest,
}

#[derive(clap::Args)]
struct ServerArgs {
    /// TCP address for the Tracker control-plane listener.
    #[arg(long, default_value = "0.0.0.0:7000")]
    tracker_addr: String,

    /// TCP address for the Prometheus metrics scrape endpoint.
    #[arg(long, default_value = "0.0.0.0:9000")]
    metrics_addr: String,
}

#[derive(clap::Args)]
struct NodeArgs {
    /// Tracker address to connect to.
    #[arg(long, default_value = "127.0.0.1:7000")]
    tracker_addr: String,

    /// This node's listen address (for incoming P2P chunk requests).
    #[arg(long, default_value = "0.0.0.0:7100")]
    peer_addr: String,

    /// Rack identifier (used for topology-aware scheduling).
    #[arg(long, default_value = "rack-0")]
    rack_id: String,

    /// Distribution job ID to join.
    #[arg(long)]
    job_id: String,

    /// Local directory where chunks are written.
    #[arg(long, default_value = "/tmp/model-dist")]
    data_dir: String,

    /// Max concurrent chunk downloads.
    #[arg(long, default_value_t = 8)]
    concurrency: usize,
}

#[derive(clap::Args)]
struct ClientArgs {
    /// Tracker address.
    #[arg(long, default_value = "127.0.0.1:7000")]
    tracker_addr: String,

    #[command(subcommand)]
    action: ClientAction,
}

#[derive(Subcommand)]
enum ClientAction {
    /// Create a new distribution job.
    CreateJob {
        #[arg(long)] file_path:   String,
        #[arg(long)] file_id:     String,
        #[arg(long)] version:     String,
        #[arg(long, default_value_t = 50)] chunk_size_mb: u64,
        #[arg(long, default_value = "rarity-first")] policy: String,
    },
    /// Show job status.
    Status { job_id: String },
    /// Cancel a job.
    Cancel  { job_id: String },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("model_distribution=debug".parse()?)
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::Server(args) => run_server(args).await,
        Command::Node(args)   => run_node(args).await,
        Command::Client(args) => run_client(args).await,
        Command::SelfTest     => self_test::run().await,
    }
}

async fn run_server(args: ServerArgs) -> Result<()> {
    info!("starting model-distribution server");

    metrics::start_metrics_server(&args.metrics_addr).await?;

    let tracker = tracker::Tracker::new();
    tracker::server::run(tracker, &args.tracker_addr).await
}

async fn run_node(args: NodeArgs) -> Result<()> {
    info!(job = %args.job_id, rack = %args.rack_id, "starting peer node");

    let node = peer::PeerNode::new(peer::PeerNodeConfig {
        tracker_addr: args.tracker_addr,
        peer_addr:    args.peer_addr,
        rack_id:      args.rack_id,
        job_id:       args.job_id,
        data_dir:     std::path::PathBuf::from(args.data_dir),
        concurrency:  args.concurrency,
    });

    node.run().await
}

async fn run_client(args: ClientArgs) -> Result<()> {
    use ClientAction::*;
    match args.action {
        CreateJob { file_path, file_id, version, chunk_size_mb, policy } => {
            let mut client = proto::client::TrackerClient::connect(&args.tracker_addr).await?;
            let job_id = client.create_job(
                &file_path,
                file_id,
                version,
                chunk_size_mb * 1024 * 1024,
                policy,
            ).await?;
            println!("job created: {job_id}");
        }
        Status { job_id } => {
            let mut client = proto::client::TrackerClient::connect(&args.tracker_addr).await?;
            let status = client.job_status(&job_id).await?;
            println!("{}", serde_json::to_string_pretty(&status)?);
        }
        Cancel { job_id } => {
            let mut client = proto::client::TrackerClient::connect(&args.tracker_addr).await?;
            client.cancel_job(&job_id).await?;
            println!("job {job_id} cancelled");
        }
    }
    Ok(())
}

mod self_test {
    use super::*;
    use crate::{manifest::Manifest, erasure::ErasureCoder, delta::DeltaPatch};

    pub async fn run() -> Result<()> {
        info!("── self-test: manifest ──");
        let data: Vec<u8> = (0u8..=255).cycle().take(200 * 1024 * 1024).collect();
        let m1 = Manifest::build("test-model", "1.0.0", &data, 50 * 1024 * 1024)?;
        assert_eq!(m1.chunk_count, 4);
        info!("  chunk_count={} ✓", m1.chunk_count);

        info!("── self-test: delta patch ──");
        let mut data2 = data.clone();
        data2[0] = 0xFF; // mutate chunk 0
        let m2 = Manifest::build("test-model", "1.0.1", &data2, 50 * 1024 * 1024)?;
        let patch = DeltaPatch::compute(&m1, &m2);
        assert_eq!(patch.changed.len(), 1);
        assert_eq!(patch.unchanged.len(), 3);
        info!("  changed={} unchanged={} ✓", patch.changed.len(), patch.unchanged.len());

        info!("── self-test: erasure coding ──");
        let coder = ErasureCoder::new(4, 2)?;
        let shard_size = 64 * 1024usize;
        let mut shards: Vec<Vec<u8>> = (0..6u8)
            .map(|i| vec![i; shard_size])
            .collect();
        for s in &mut shards[4..] { s.fill(0); }
        coder.encode(&mut shards)?;

        let originals: Vec<Vec<u8>> = shards.clone();
        let mut recv: Vec<Option<Vec<u8>>> = shards
            .into_iter()
            .enumerate()
            .map(|(i, s)| if i == 1 || i == 3 { None } else { Some(s) })
            .collect();
        coder.reconstruct(&mut recv)?;

        for i in 0..4 {
            assert_eq!(recv[i].as_ref().unwrap(), &originals[i], "shard {i} mismatch");
        }
        info!("  reconstruct shards [1,3] ✓");

        info!("── self-test: bitmap ──");
        use crate::manifest::ChunkBitmap;
        let mut bm = ChunkBitmap::new(10);
        bm.mark_complete(0);
        bm.mark_complete(5);
        assert_eq!(bm.completion_ratio(), 0.2);
        assert!(bm.has(0) && !bm.has(1));
        info!("  completion_ratio=0.2 ✓");

        info!("✅  all self-tests passed");
        Ok(())
    }
}
