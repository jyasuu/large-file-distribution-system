use std::sync::Arc;
use reed_solomon_erasure::galois_8::ReedSolomon;
use tracing::{info, warn};

use crate::error::{DistError, Result};

// ─────────────────────────────────────────────────────────────────────────────
// ErasureCoder
// ─────────────────────────────────────────────────────────────────────────────

/// Wraps a Reed-Solomon codec (GF(2^8)).
///
/// Parameters:
///   k = data_shards   — number of original data shards
///   m = parity_shards — number of redundant parity shards
///
/// Any k-of-(k+m) shards suffice to reconstruct all k data shards.
pub struct ErasureCoder {
    rs:           ReedSolomon,
    pub data_shards:  usize,
    pub parity_shards: usize,
}

impl ErasureCoder {
    pub fn new(data_shards: usize, parity_shards: usize) -> Result<Self> {
        let rs = ReedSolomon::new(data_shards, parity_shards)
            .map_err(|e| DistError::ErasureError(e.to_string()))?;
        Ok(Self { rs, data_shards, parity_shards })
    }

    /// Total shard count (k + m).
    pub fn total_shards(&self) -> usize {
        self.data_shards + self.parity_shards
    }

    /// Encode: `shards` must have exactly `total_shards` elements.
    /// The first `data_shards` must be filled; parity slots may be zeroed.
    /// On success the parity slots are written in place.
    pub fn encode(&self, shards: &mut Vec<Vec<u8>>) -> Result<()> {
        if shards.len() != self.total_shards() {
            return Err(DistError::ErasureError(format!(
                "expected {} shards, got {}", self.total_shards(), shards.len()
            )));
        }
        self.rs.encode(shards)
            .map_err(|e| DistError::ErasureError(e.to_string()))
    }

    /// Reconstruct missing shards.
    /// `shards` has `total_shards` elements; missing ones are `None`.
    /// At least `data_shards` must be present.
    /// On success all `None` slots are filled.
    pub fn reconstruct(&self, shards: &mut Vec<Option<Vec<u8>>>) -> Result<()> {
        let present = shards.iter().filter(|s| s.is_some()).count();
        if present < self.data_shards {
            return Err(DistError::ErasureError(format!(
                "need {k} shards to reconstruct, only {present} available",
                k = self.data_shards,
            )));
        }
        self.rs.reconstruct(shards)
            .map_err(|e| DistError::ErasureError(e.to_string()))
    }

    /// Split a flat byte slice into k equal-sized data shards.
    /// Pads the last shard with zeros if `data.len()` is not divisible by k.
    pub fn split_into_shards(&self, data: &[u8]) -> Vec<Vec<u8>> {
        let shard_size = (data.len() + self.data_shards - 1) / self.data_shards;
        let mut shards: Vec<Vec<u8>> = data
            .chunks(shard_size)
            .map(|c| {
                let mut v = c.to_vec();
                v.resize(shard_size, 0); // pad last shard if needed
                v
            })
            .collect();
        // Ensure exactly data_shards slices (may have fewer if data is tiny).
        while shards.len() < self.data_shards {
            shards.push(vec![0u8; shard_size]);
        }
        shards
    }

    /// Merge reconstructed data shards back into a flat byte slice,
    /// trimming to `original_len`.
    pub fn merge_shards(shards: &[Option<Vec<u8>>], original_len: usize, k: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(original_len);
        for shard in shards.iter().take(k) {
            if let Some(s) = shard {
                out.extend_from_slice(s);
            }
        }
        out.truncate(original_len);
        out
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tail-phase controller
// ─────────────────────────────────────────────────────────────────────────────

/// Configuration for the tail-phase EC controller.
pub struct TailConfig {
    /// Fraction of nodes complete before EC activates (e.g. 0.95).
    pub activation_threshold: f32,
    /// Reed-Solomon data shard count (k).
    pub data_shards:    usize,
    /// Reed-Solomon parity shard count (m).
    pub parity_shards:  usize,
    /// Minimum replica count for a chunk before it's considered "scarce".
    pub scarce_threshold: u32,
}

impl Default for TailConfig {
    fn default() -> Self {
        Self {
            activation_threshold: 0.95,
            data_shards:          4,
            parity_shards:        2,
            scarce_threshold:     3,
        }
    }
}

/// Identifies chunks that need EC treatment and returns EC-encoded stripe sets.
///
/// Each returned `EcStripe` contains:
///   - the original chunk indices forming this stripe
///   - the full shard set (data + parity) ready for distribution
///
/// In a real deployment, parity shards would be pushed to nodes that lack
/// the corresponding data chunks; those nodes can then call `reconstruct`.
pub struct EcStripe {
    pub chunk_indices: Vec<u32>,   // which original chunks are in this stripe
    pub shards:        Vec<Vec<u8>>, // k+m encoded shards
    pub shard_size:    usize,
}

pub fn build_ec_stripes(
    scarce_chunks: &[(u32, Vec<u8>)],  // (chunk_index, chunk_data)
    cfg:           &TailConfig,
) -> Result<Vec<EcStripe>> {
    let coder = ErasureCoder::new(cfg.data_shards, cfg.parity_shards)?;
    let mut stripes = Vec::new();

    for window in scarce_chunks.chunks(cfg.data_shards) {
        // Normalise shard size to the largest chunk in this stripe.
        let shard_size = window.iter().map(|(_, d)| d.len()).max().unwrap_or(0);

        let mut shards: Vec<Vec<u8>> = window.iter().map(|(_, data)| {
            let mut s = data.clone();
            s.resize(shard_size, 0);
            s
        }).collect();

        // Pad to exactly data_shards if window is smaller than k.
        while shards.len() < cfg.data_shards {
            shards.push(vec![0u8; shard_size]);
        }

        // Append empty parity slots.
        for _ in 0..cfg.parity_shards {
            shards.push(vec![0u8; shard_size]);
        }

        coder.encode(&mut shards)?;

        let chunk_indices = window.iter().map(|(i, _)| *i).collect();
        stripes.push(EcStripe { chunk_indices, shards, shard_size });
        info!(
            stripe_chunks = window.len(),
            "EC stripe encoded ({} data + {} parity shards)",
            cfg.data_shards, cfg.parity_shards
        );
    }

    Ok(stripes)
}

/// Given a stripe where some data shards are missing, reconstruct them.
pub fn reconstruct_stripe(
    stripe:          &EcStripe,
    present_indices: &[usize],   // which shard indices (0-based) we have
    cfg:             &TailConfig,
) -> Result<Vec<u8>> {
    let coder = ErasureCoder::new(cfg.data_shards, cfg.parity_shards)?;
    let total  = coder.total_shards();

    let present_set: std::collections::HashSet<usize> =
        present_indices.iter().copied().collect();

    let mut recv: Vec<Option<Vec<u8>>> = (0..total)
        .map(|i| {
            if present_set.contains(&i) {
                Some(stripe.shards[i].clone())
            } else {
                None
            }
        })
        .collect();

    coder.reconstruct(&mut recv)?;
    Ok(ErasureCoder::merge_shards(
        &recv,
        stripe.shards[0].len() * cfg.data_shards, // approximate; caller trims
        cfg.data_shards,
    ))
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_coder() -> ErasureCoder {
        ErasureCoder::new(4, 2).unwrap()
    }

    #[test]
    fn encode_decode_no_loss() {
        let coder = make_coder();
        let sz    = 1024usize;
        let mut shards: Vec<Vec<u8>> = (0..6u8)
            .map(|i| vec![i * 10; sz])
            .collect();
        for s in &mut shards[4..] { s.fill(0); }

        coder.encode(&mut shards).unwrap();

        let originals = shards.clone();
        let mut recv: Vec<Option<Vec<u8>>> = shards.into_iter().map(Some).collect();
        coder.reconstruct(&mut recv).unwrap();

        for i in 0..4 {
            assert_eq!(recv[i].as_ref().unwrap(), &originals[i]);
        }
    }

    #[test]
    fn reconstruct_two_missing_data_shards() {
        let coder = make_coder();
        let sz    = 1024usize;
        let mut shards: Vec<Vec<u8>> = (0..6u8)
            .map(|i| vec![i * 7; sz])
            .collect();
        for s in &mut shards[4..] { s.fill(0); }
        coder.encode(&mut shards).unwrap();

        let originals = shards.clone();
        // Lose data shard 1 and data shard 3
        let mut recv: Vec<Option<Vec<u8>>> = shards.into_iter()
            .enumerate()
            .map(|(i, s)| if i == 1 || i == 3 { None } else { Some(s) })
            .collect();
        coder.reconstruct(&mut recv).unwrap();
        assert_eq!(recv[1].as_ref().unwrap(), &originals[1]);
        assert_eq!(recv[3].as_ref().unwrap(), &originals[3]);
    }

    #[test]
    fn reconstruct_fails_with_too_few_shards() {
        let coder = make_coder(); // k=4, m=2 → need 4 present
        let sz    = 512usize;
        let mut shards: Vec<Vec<u8>> = (0..6).map(|_| vec![1u8; sz]).collect();
        for s in &mut shards[4..] { s.fill(0); }
        coder.encode(&mut shards).unwrap();

        // Lose 3 shards (only 3 remain → cannot reconstruct k=4)
        let mut recv: Vec<Option<Vec<u8>>> = shards.into_iter()
            .enumerate()
            .map(|(i, s)| if i < 3 { None } else { Some(s) })
            .collect();
        assert!(coder.reconstruct(&mut recv).is_err());
    }

    #[test]
    fn split_and_merge() {
        let coder = ErasureCoder::new(4, 2).unwrap();
        let data: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
        let shards = coder.split_into_shards(&data);
        assert_eq!(shards.len(), 4);

        let wrapped: Vec<Option<Vec<u8>>> = shards.into_iter().map(Some).collect();
        let merged = ErasureCoder::merge_shards(&wrapped, data.len(), 4);
        assert_eq!(merged, data);
    }

    #[test]
    fn build_ec_stripes_produces_correct_shard_count() {
        let cfg = TailConfig::default(); // k=4, m=2
        let chunks: Vec<(u32, Vec<u8>)> = (0..4)
            .map(|i| (i as u32, vec![i as u8 * 5; 512]))
            .collect();
        let stripes = build_ec_stripes(&chunks, &cfg).unwrap();
        assert_eq!(stripes.len(), 1);
        assert_eq!(stripes[0].shards.len(), 6); // k+m
    }
}
