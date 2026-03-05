use std::path::Path;
use bitvec::prelude::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use crate::error::{DistError, Result};

// ─────────────────────────────────────────────────────────────────────────────
// Manifest
// ─────────────────────────────────────────────────────────────────────────────

/// File-level metadata distributed to every node before transfer begins.
/// Serves as the single source of truth for structure and integrity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub file_id:      String,
    pub version:      String,
    pub total_size:   u64,
    pub chunk_size:   u64,
    pub chunk_count:  u32,
    /// SHA-256 hex digest per chunk (indexed by chunk index).
    pub chunk_hashes: Vec<String>,
}

impl Manifest {
    /// Build a manifest by reading a byte slice and hashing each chunk.
    /// Used by the Seed Node / Manifest Service when ingesting a new model.
    pub fn build(
        file_id: impl Into<String>,
        version: impl Into<String>,
        data:    &[u8],
        chunk_size: u64,
    ) -> Result<Self> {
        let total_size  = data.len() as u64;
        let chunk_count = chunks_needed(total_size, chunk_size);

        let chunk_hashes = (0..chunk_count)
            .map(|i| {
                let (start, end) = chunk_range_inner(i, chunk_size, total_size);
                hash_bytes(&data[start as usize..end as usize])
            })
            .collect();

        Ok(Manifest {
            file_id:     file_id.into(),
            version:     version.into(),
            total_size,
            chunk_size,
            chunk_count,
            chunk_hashes,
        })
    }

    /// Build a manifest by streaming a file from disk chunk-by-chunk.
    /// Memory-efficient for very large models.
    pub async fn build_from_file(
        file_id:    impl Into<String>,
        version:    impl Into<String>,
        path:       &Path,
        chunk_size: u64,
    ) -> Result<Self> {
        use tokio::io::{AsyncReadExt, BufReader};
        use tokio::fs::File;

        let file       = File::open(path).await?;
        let total_size = file.metadata().await?.len();
        let chunk_count = chunks_needed(total_size, chunk_size);
        let mut reader = BufReader::new(file);
        let mut chunk_hashes = Vec::with_capacity(chunk_count as usize);
        let mut buf = vec![0u8; chunk_size as usize];

        for _ in 0..chunk_count {
            let n = reader.read(&mut buf).await?;
            chunk_hashes.push(hash_bytes(&buf[..n]));
        }

        Ok(Manifest {
            file_id:    file_id.into(),
            version:    version.into(),
            total_size,
            chunk_size,
            chunk_count,
            chunk_hashes,
        })
    }

    /// Byte range [start, end) for chunk index `i`.
    pub fn chunk_range(&self, index: u32) -> (u64, u64) {
        chunk_range_inner(index, self.chunk_size, self.total_size)
    }

    /// Verify a raw chunk buffer against the manifest hash.
    pub fn verify_chunk(&self, index: u32, data: &[u8]) -> Result<()> {
        let expected = &self.chunk_hashes[index as usize];
        let actual   = hash_bytes(data);
        if actual != *expected {
            return Err(DistError::HashMismatch {
                index,
                expected: expected.clone(),
                actual,
            });
        }
        Ok(())
    }

    pub fn serialise(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(self)?)
    }

    pub fn deserialise(bytes: &[u8]) -> Result<Self> {
        Ok(serde_json::from_slice(bytes)?)
    }
}

fn chunk_range_inner(index: u32, chunk_size: u64, total_size: u64) -> (u64, u64) {
    let start = index as u64 * chunk_size;
    let end   = (start + chunk_size).min(total_size);
    (start, end)
}

fn chunks_needed(total_size: u64, chunk_size: u64) -> u32 {
    ((total_size + chunk_size - 1) / chunk_size) as u32
}

fn hash_bytes(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    hex::encode(h.finalize())
}

// ─────────────────────────────────────────────────────────────────────────────
// ChunkBitmap
// ─────────────────────────────────────────────────────────────────────────────

/// Per-node bitfield — bit[i] set means chunk i is fully received and verified.
#[derive(Clone, Debug)]
pub struct ChunkBitmap {
    bits:        BitVec<u8, Msb0>,
    complete:    u32,
    chunk_count: u32,
}

impl ChunkBitmap {
    pub fn new(chunk_count: u32) -> Self {
        Self {
            bits:        bitvec![u8, Msb0; 0; chunk_count as usize],
            complete:    0,
            chunk_count,
        }
    }

    /// Build from raw bytes received from a peer's heartbeat.
    pub fn from_bytes(bytes: &[u8], chunk_count: u32) -> Self {
        let mut bits = BitVec::<u8, Msb0>::from_slice(bytes);
        bits.truncate(chunk_count as usize);
        let complete = bits.count_ones() as u32;
        Self { bits, complete, chunk_count }
    }

    /// Mark chunk as complete; returns true if it was newly set.
    pub fn mark_complete(&mut self, index: u32) -> bool {
        if index >= self.chunk_count { return false; }
        if !self.bits[index as usize] {
            self.bits.set(index as usize, true);
            self.complete += 1;
            true
        } else {
            false
        }
    }

    pub fn has(&self, index: u32) -> bool {
        index < self.chunk_count && self.bits[index as usize]
    }

    pub fn is_complete(&self) -> bool {
        self.complete == self.chunk_count
    }

    pub fn completion_ratio(&self) -> f32 {
        if self.chunk_count == 0 { return 1.0; }
        self.complete as f32 / self.chunk_count as f32
    }

    pub fn completed_count(&self) -> u32 { self.complete }

    /// Iterator over missing chunk indices.
    pub fn missing(&self) -> impl Iterator<Item = u32> + '_ {
        self.bits
            .iter()
            .enumerate()
            .filter(|(_, b)| !**b)
            .map(|(i, _)| i as u32)
    }

    /// Serialise to raw bytes for network transmission.
    pub fn as_bytes(&self) -> &[u8] {
        self.bits.as_raw_slice()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_data(size: usize) -> Vec<u8> {
        (0u8..=255).cycle().take(size).collect()
    }

    #[test]
    fn manifest_chunk_count() {
        let data = make_data(200 * 1024 * 1024);
        let m = Manifest::build("m", "1", &data, 50 * 1024 * 1024).unwrap();
        assert_eq!(m.chunk_count, 4);
    }

    #[test]
    fn manifest_verify_ok() {
        let data = make_data(100 * 1024 * 1024);
        let m    = Manifest::build("m", "1", &data, 50 * 1024 * 1024).unwrap();
        let (s, e) = m.chunk_range(0);
        m.verify_chunk(0, &data[s as usize..e as usize]).unwrap();
    }

    #[test]
    fn manifest_verify_corrupt() {
        let data = make_data(100 * 1024 * 1024);
        let m    = Manifest::build("m", "1", &data, 50 * 1024 * 1024).unwrap();
        let mut corrupt = data[..50 * 1024 * 1024].to_vec();
        corrupt[0] ^= 0xFF;
        assert!(m.verify_chunk(0, &corrupt).is_err());
    }

    #[test]
    fn bitmap_operations() {
        let mut bm = ChunkBitmap::new(10);
        assert_eq!(bm.completion_ratio(), 0.0);
        assert!(bm.mark_complete(0));
        assert!(!bm.mark_complete(0)); // idempotent
        bm.mark_complete(5);
        assert!(bm.has(0));
        assert!(!bm.has(1));
        assert_eq!(bm.completed_count(), 2);
        let missing: Vec<_> = bm.missing().collect();
        assert_eq!(missing.len(), 8);
        assert!(!missing.contains(&0));
        assert!(!missing.contains(&5));
    }

    #[test]
    fn bitmap_round_trip() {
        let mut bm = ChunkBitmap::new(16);
        bm.mark_complete(0); bm.mark_complete(7); bm.mark_complete(15);
        let bytes = bm.as_bytes().to_vec();
        let bm2   = ChunkBitmap::from_bytes(&bytes, 16);
        assert!(bm2.has(0)); assert!(bm2.has(7)); assert!(bm2.has(15));
        assert!(!bm2.has(1));
        assert_eq!(bm2.completed_count(), 3);
    }

    #[test]
    fn manifest_serialise_roundtrip() {
        let data = make_data(10 * 1024 * 1024);
        let m    = Manifest::build("x", "2", &data, 5 * 1024 * 1024).unwrap();
        let bytes = m.serialise().unwrap();
        let m2    = Manifest::deserialise(&bytes).unwrap();
        assert_eq!(m.chunk_count, m2.chunk_count);
        assert_eq!(m.chunk_hashes, m2.chunk_hashes);
    }
}
