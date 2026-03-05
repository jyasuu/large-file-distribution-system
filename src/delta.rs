use std::collections::{HashMap, HashSet};
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::manifest::Manifest;

// ─────────────────────────────────────────────────────────────────────────────
// DeltaPatch
// ─────────────────────────────────────────────────────────────────────────────

/// Describes the minimal set of chunk changes between two model versions.
/// Serialised and shipped to nodes at the start of an update job so they
/// only fetch what actually changed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeltaPatch {
    pub old_version: String,
    pub new_version: String,
    pub file_id:     String,

    /// Chunk indices present in both versions with identical hashes.
    /// Nodes can skip fetching these — they're already correct on disk.
    pub unchanged: HashSet<u32>,

    /// Chunk indices that are new or have changed content.
    /// Value is the new SHA-256 hash (from the new manifest).
    pub changed: HashMap<u32, String>,

    /// Chunk indices present in the old version but absent in the new.
    /// Nodes should delete these from local storage.
    pub deleted: HashSet<u32>,
}

impl DeltaPatch {
    /// Compute the diff between two manifests for the same file_id.
    pub fn compute(old: &Manifest, new: &Manifest) -> Self {
        assert_eq!(old.file_id, new.file_id, "file_id mismatch");

        let mut unchanged = HashSet::new();
        let mut changed   = HashMap::new();
        let mut deleted   = HashSet::new();

        let old_n = old.chunk_count as usize;
        let new_n = new.chunk_count as usize;

        // Chunks present in both versions.
        for i in 0..new_n {
            if i < old_n && old.chunk_hashes[i] == new.chunk_hashes[i] {
                unchanged.insert(i as u32);
            } else {
                changed.insert(i as u32, new.chunk_hashes[i].clone());
            }
        }

        // Chunks in old but not in new (file shrank or chunks were removed).
        for i in new_n..old_n {
            deleted.insert(i as u32);
        }

        DeltaPatch {
            old_version: old.version.clone(),
            new_version: new.version.clone(),
            file_id:     new.file_id.clone(),
            unchanged,
            changed,
            deleted,
        }
    }

    /// Returns the chunk indices that a node must download to apply this patch.
    pub fn chunks_to_fetch(&self) -> Vec<u32> {
        let mut indices: Vec<u32> = self.changed.keys().copied().collect();
        indices.sort_unstable();
        indices
    }

    /// Returns the chunk indices that should be deleted locally.
    pub fn chunks_to_delete(&self) -> Vec<u32> {
        let mut indices: Vec<u32> = self.deleted.iter().copied().collect();
        indices.sort_unstable();
        indices
    }

    /// Check whether fetching a particular chunk index is needed.
    pub fn needs_fetch(&self, chunk_index: u32) -> bool {
        self.changed.contains_key(&chunk_index)
    }

    /// Validate that a received chunk matches the expected hash in this patch.
    pub fn verify_chunk(&self, chunk_index: u32, data: &[u8]) -> Result<()> {
        use sha2::{Digest, Sha256};

        let expected = self.changed.get(&chunk_index).ok_or_else(|| {
            crate::error::DistError::Proto(format!(
                "chunk {chunk_index} is not in the delta patch's changed set"
            ))
        })?;

        let mut h = Sha256::new();
        h.update(data);
        let actual = hex::encode(h.finalize());

        if actual != *expected {
            return Err(crate::error::DistError::HashMismatch {
                index:    chunk_index,
                expected: expected.clone(),
                actual,
            });
        }
        Ok(())
    }

    pub fn total_changed(&self) -> usize  { self.changed.len()   }
    pub fn total_unchanged(&self) -> usize { self.unchanged.len() }
    pub fn total_deleted(&self) -> usize  { self.deleted.len()   }

    /// Human-readable summary.
    pub fn summary(&self) -> String {
        format!(
            "delta {} → {}: {} changed, {} unchanged, {} deleted ({:.1}% transfer reduction)",
            self.old_version,
            self.new_version,
            self.changed.len(),
            self.unchanged.len(),
            self.deleted.len(),
            self.transfer_reduction_pct(),
        )
    }

    /// Fraction of chunks we avoid re-downloading.
    pub fn transfer_reduction_pct(&self) -> f32 {
        let total = (self.changed.len() + self.unchanged.len() + self.deleted.len()) as f32;
        if total == 0.0 { return 100.0; }
        self.unchanged.len() as f32 / total * 100.0
    }

    pub fn serialise(&self) -> crate::error::Result<Vec<u8>> {
        Ok(serde_json::to_vec(self)?)
    }

    pub fn deserialise(bytes: &[u8]) -> crate::error::Result<Self> {
        Ok(serde_json::from_slice(bytes)?)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Local patch applicator
// ─────────────────────────────────────────────────────────────────────────────

use std::path::PathBuf;

/// Applies a delta patch to a node's local chunk store:
///   - Removes deleted chunks
///   - Unchanged chunks are left in place
///   - Changed chunks must be fetched separately (via the downloader)
pub struct PatchApplicator {
    pub data_dir: PathBuf,
    pub file_id:  String,
}

impl PatchApplicator {
    pub fn new(data_dir: PathBuf, file_id: impl Into<String>) -> Self {
        Self { data_dir, file_id: file_id.into() }
    }

    /// Remove chunks that belong to the old version but not the new.
    pub async fn delete_stale_chunks(&self, patch: &DeltaPatch) -> Result<usize> {
        let mut count = 0;
        for chunk_index in patch.chunks_to_delete() {
            let path = self.chunk_path(chunk_index);
            match tokio::fs::remove_file(&path).await {
                Ok(())    => { count += 1; }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e)    => return Err(e.into()),
            }
        }
        Ok(count)
    }

    /// Verify that unchanged chunks on disk still match their expected hashes.
    /// Useful as a pre-flight check before starting an update job.
    pub async fn verify_unchanged(
        &self,
        patch:    &DeltaPatch,
        manifest: &Manifest,
    ) -> Result<Vec<u32>> {
        let mut corrupt = vec![];
        for &chunk_index in &patch.unchanged {
            let path = self.chunk_path(chunk_index);
            match tokio::fs::read(&path).await {
                Ok(data) => {
                    if manifest.verify_chunk(chunk_index, &data).is_err() {
                        corrupt.push(chunk_index);
                    }
                }
                Err(_) => { corrupt.push(chunk_index); }
            }
        }
        Ok(corrupt)
    }

    fn chunk_path(&self, chunk_index: u32) -> PathBuf {
        self.data_dir.join(format!("{}.chunk.{chunk_index:06}", self.file_id))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_manifest(version: &str, data: &[u8]) -> Manifest {
        Manifest::build("model-x", version, data, 50 * 1024 * 1024).unwrap()
    }

    fn make_data(n: usize) -> Vec<u8> {
        (0u8..=255).cycle().take(n).collect()
    }

    #[test]
    fn no_change() {
        let data = make_data(100 * 1024 * 1024);
        let m1   = make_manifest("1.0", &data);
        let m2   = make_manifest("1.0.1", &data);
        let p    = DeltaPatch::compute(&m1, &m2);
        assert_eq!(p.changed.len(), 0);
        assert_eq!(p.unchanged.len(), 2);
        assert_eq!(p.deleted.len(), 0);
        assert!(p.transfer_reduction_pct() > 99.0);
    }

    #[test]
    fn one_chunk_changed() {
        let mut data = make_data(100 * 1024 * 1024);
        let m1 = make_manifest("1.0", &data);
        data[0] ^= 0xFF; // corrupt first byte → changes chunk 0
        let m2 = make_manifest("1.1", &data);
        let p  = DeltaPatch::compute(&m1, &m2);
        assert_eq!(p.changed.len(), 1);
        assert!(p.changed.contains_key(&0));
        assert_eq!(p.unchanged.len(), 1);
    }

    #[test]
    fn file_grew() {
        let data1 = make_data(100 * 1024 * 1024);
        let data2 = make_data(150 * 1024 * 1024);
        let m1    = make_manifest("1.0", &data1);
        let m2    = make_manifest("2.0", &data2);
        let p     = DeltaPatch::compute(&m1, &m2);
        // chunk 2 (new) should be in changed
        assert!(p.changed.contains_key(&2));
        // chunk 0 and 1 unchanged
        assert!(p.unchanged.contains(&0));
        assert!(p.unchanged.contains(&1));
    }

    #[test]
    fn file_shrank() {
        let data1 = make_data(150 * 1024 * 1024);
        let data2 = make_data(100 * 1024 * 1024);
        let m1    = make_manifest("1.0", &data1);
        let m2    = make_manifest("2.0", &data2);
        let p     = DeltaPatch::compute(&m1, &m2);
        // chunk 2 should be deleted
        assert!(p.deleted.contains(&2));
        assert_eq!(p.deleted.len(), 1);
    }

    #[test]
    fn chunks_to_fetch_sorted() {
        let data = make_data(200 * 1024 * 1024);
        let m1   = make_manifest("1.0", &data);
        let mut data2 = data.clone();
        data2[0]                 ^= 0xFF; // chunk 0
        data2[100 * 1024 * 1024] ^= 0xFF; // chunk 2
        let m2   = make_manifest("2.0", &data2);
        let p    = DeltaPatch::compute(&m1, &m2);
        let fetch = p.chunks_to_fetch();
        assert_eq!(fetch, vec![0, 2]); // sorted
    }

    #[test]
    fn serialise_roundtrip() {
        let data = make_data(100 * 1024 * 1024);
        let m1   = make_manifest("1.0", &data);
        let m2   = make_manifest("1.1", &data);
        let p    = DeltaPatch::compute(&m1, &m2);
        let bytes = p.serialise().unwrap();
        let p2   = DeltaPatch::deserialise(&bytes).unwrap();
        assert_eq!(p.changed.len(), p2.changed.len());
        assert_eq!(p.unchanged, p2.unchanged);
    }
}
