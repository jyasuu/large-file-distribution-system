use thiserror::Error;

#[derive(Debug, Error)]
pub enum DistError {
    #[error("chunk hash mismatch for chunk {index}: expected={expected} got={actual}")]
    HashMismatch {
        index:    u32,
        expected: String,
        actual:   String,
    },

    #[error("no peers available for chunk {0}")]
    NoPeers(u32),

    #[error("job not found: {0}")]
    JobNotFound(String),

    #[error("node not found: {0}")]
    NodeNotFound(String),

    #[error("erasure coding error: {0}")]
    ErasureError(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serialisation error: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("protocol error: {0}")]
    Proto(String),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, DistError>;
