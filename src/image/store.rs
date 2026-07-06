//! Content-Addressable Storage for OCI Blobs
//!
//! Provides deduplicated storage for image layers, configs, and manifests.
//! All content is stored by its SHA256 digest, enabling:
//! - Automatic deduplication across images
//! - Content verification on read
//! - Efficient layer sharing

use crate::image::{ImageError, Result};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::SystemTime;
use walkdir::WalkDir;

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct StoredBlob {
    pub digest: String,
    pub path: PathBuf,
    pub size_bytes: u64,
    pub modified_at: SystemTime,
}

/// Content-addressable blob store
pub struct ContentStore {
    /// Base path for blob storage
    base_path: PathBuf,

    /// In-memory cache of blob existence (digest -> size)
    cache: RwLock<HashMap<String, u64>>,
}

#[allow(dead_code)]
impl ContentStore {
    /// Create a new content store at the given path
    pub fn new<P: AsRef<Path>>(base_path: P) -> Result<Self> {
        let base = base_path.as_ref().to_path_buf();

        // Create directory structure
        std::fs::create_dir_all(base.join("sha256"))?;

        Ok(Self {
            base_path: base,
            cache: RwLock::new(HashMap::new()),
        })
    }

    /// Store a blob and return its digest
    pub fn store_blob(&self, data: &[u8]) -> Result<String> {
        let digest = self.compute_digest(data);
        let path = self.blob_path(&digest);

        // Skip only when the existing blob verifies at the expected digest.
        if path.exists() {
            match std::fs::read(&path) {
                Ok(existing) if self.compute_digest(&existing) == digest => {
                    let mut cache = self.cache.write().map_err(|e| {
                        ImageError::StoreError(format!("Failed to acquire write lock: {}", e))
                    })?;
                    cache.insert(digest.clone(), existing.len() as u64);
                    return Ok(digest);
                }
                Ok(_) | Err(_) => {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }

        // Create parent directory if needed
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Write atomically using temp file
        let temp_path = path.with_extension("tmp");
        std::fs::write(&temp_path, data)?;
        std::fs::rename(&temp_path, &path)?;

        // Update cache
        let mut cache = self
            .cache
            .write()
            .map_err(|e| ImageError::StoreError(format!("Failed to acquire write lock: {}", e)))?;
        cache.insert(digest.clone(), data.len() as u64);

        Ok(digest)
    }

    /// Get a blob by digest
    pub fn get_blob(&self, digest: &str) -> Result<Vec<u8>> {
        let path = self.blob_path(digest);

        if !path.exists() {
            return Err(ImageError::BlobNotFound(digest.to_string()));
        }

        let data = std::fs::read(&path)?;

        // Verify content
        let actual_digest = self.compute_digest(&data);
        if actual_digest != digest {
            return Err(ImageError::ContentVerificationError {
                expected: digest.to_string(),
                actual: actual_digest,
            });
        }

        Ok(data)
    }

    /// Check if a blob exists
    pub fn has_blob(&self, digest: &str) -> Result<bool> {
        if self.get_blob(digest).is_ok() {
            return Ok(true);
        }

        // Check cache first
        {
            let cache = self.cache.read().map_err(|e| {
                ImageError::StoreError(format!("Failed to acquire read lock: {}", e))
            })?;
            if cache.contains_key(digest) {
                return Ok(true);
            }
        }

        // Check filesystem
        let path = self.blob_path(digest);
        let exists = path.exists();

        // Update cache if exists
        if exists && self.get_blob(digest).is_ok() {
            if let Ok(metadata) = std::fs::metadata(&path) {
                let mut cache = self.cache.write().map_err(|e| {
                    ImageError::StoreError(format!("Failed to acquire write lock: {}", e))
                })?;
                cache.insert(digest.to_string(), metadata.len());
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// Delete a blob
    pub fn delete_blob(&self, digest: &str) -> Result<()> {
        let path = self.blob_path(digest);

        if path.exists() {
            std::fs::remove_file(&path)?;
        }

        // Remove from cache
        let mut cache = self
            .cache
            .write()
            .map_err(|e| ImageError::StoreError(format!("Failed to acquire write lock: {}", e)))?;
        cache.remove(digest);

        Ok(())
    }

    /// List all stored blobs with their filesystem metadata.
    pub fn list_blobs(&self) -> Result<Vec<StoredBlob>> {
        let mut blobs = Vec::new();

        for entry in WalkDir::new(&self.base_path)
            .follow_links(false)
            .into_iter()
            .filter_map(|entry| entry.ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }

            let path = entry.path();
            let Some(relative_path) = path.strip_prefix(&self.base_path).ok() else {
                continue;
            };
            let mut components = relative_path.components();
            let Some(algo) = components.next() else {
                continue;
            };
            let Some(_) = components.next() else {
                continue;
            };
            let Some(hash) = components.next() else {
                continue;
            };
            if components.next().is_some() {
                continue;
            }

            let digest = format!(
                "{}:{}",
                algo.as_os_str().to_string_lossy(),
                hash.as_os_str().to_string_lossy()
            );
            let metadata = std::fs::metadata(path)?;
            blobs.push(StoredBlob {
                digest,
                path: path.to_path_buf(),
                size_bytes: metadata.len(),
                modified_at: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            });
        }

        Ok(blobs)
    }

    /// Get the filesystem path for a blob
    pub fn blob_path(&self, digest: &str) -> PathBuf {
        // Parse digest: sha256:abc123... -> sha256/ab/abc123...
        let (algo, hash) = digest.split_once(':').unwrap_or(("sha256", digest));

        // Use first two chars as subdirectory for better filesystem distribution
        let prefix = &hash[..2.min(hash.len())];

        self.base_path.join(algo).join(prefix).join(hash)
    }

    /// Compute SHA256 digest of data
    pub fn compute_digest(&self, data: &[u8]) -> String {
        let hash = Sha256::digest(data);
        format!("sha256:{:x}", hash)
    }
}

/// High-level image store - manages images, manifests, and their relationships
pub struct ImageStore {
    /// Underlying content store
    content: ContentStore,

    /// Image metadata storage path
    metadata_path: PathBuf,
}

impl ImageStore {
    /// Create a new image store
    pub fn new<P: AsRef<Path>>(base_path: P) -> Result<Self> {
        let base = base_path.as_ref();

        Ok(Self {
            content: ContentStore::new(base.join("blobs"))?,
            metadata_path: base.join("metadata"),
        })
    }

    /// Get the underlying content store
    pub fn content(&self) -> &ContentStore {
        &self.content
    }

    /// Store a blob and return its digest
    pub fn store_blob(&self, data: &[u8]) -> Result<String> {
        self.content.store_blob(data)
    }

    /// Get a blob by digest
    pub fn get_blob(&self, digest: &str) -> Result<Vec<u8>> {
        self.content.get_blob(digest)
    }

    /// Store image metadata (reference -> manifest digest mapping)
    pub fn store_image_ref(
        &self,
        reference: &crate::image::ImageReference,
        manifest_digest: &str,
    ) -> Result<()> {
        std::fs::create_dir_all(&self.metadata_path)?;

        let ref_path = self.ref_path(reference);
        if let Some(parent) = ref_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        std::fs::write(&ref_path, manifest_digest)?;
        Ok(())
    }

    /// Resolve a reference to its stored manifest digest.
    pub fn resolve_image_ref(
        &self,
        reference: &crate::image::ImageReference,
    ) -> Result<Option<String>> {
        let ref_path = self.ref_path(reference);
        match std::fs::read_to_string(ref_path) {
            Ok(value) => Ok(Some(value.trim().to_string())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    /// List all stored image references and their manifest digests.
    pub fn list_image_refs(&self) -> Result<Vec<(String, String)>> {
        let mut refs = Vec::new();
        if !self.metadata_path.exists() {
            return Ok(refs);
        }

        for entry in WalkDir::new(&self.metadata_path)
            .follow_links(false)
            .into_iter()
            .filter_map(|entry| entry.ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }

            let path = entry.path();
            let Ok(relative) = path.strip_prefix(&self.metadata_path) else {
                continue;
            };
            let components = relative
                .iter()
                .map(|part| part.to_string_lossy().to_string())
                .collect::<Vec<_>>();
            if components.len() < 3 {
                continue;
            }

            let registry = &components[0];
            let tag = components.last().cloned().unwrap_or_default().replace('_', ":");
            let repository = components[1..components.len() - 1].join("/");
            let manifest_digest = std::fs::read_to_string(path)?.trim().to_string();
            refs.push((format!("{}/{}:{}", registry, repository, tag), manifest_digest));
        }

        refs.sort();
        Ok(refs)
    }

    /// Get the path for storing a reference
    fn ref_path(&self, reference: &crate::image::ImageReference) -> PathBuf {
        let tag_or_digest = reference
            .digest
            .as_deref()
            .unwrap_or(&reference.tag)
            .replace(':', "_");

        self.metadata_path
            .join(&reference.registry)
            .join(&reference.repository)
            .join(tag_or_digest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_content_store() {
        let dir = tempdir().unwrap();
        let store = ContentStore::new(dir.path()).unwrap();

        let data = b"hello world";
        let digest = store.store_blob(data).unwrap();

        assert!(store.has_blob(&digest).unwrap());
        assert_eq!(store.get_blob(&digest).unwrap(), data);

        let computed = store.compute_digest(data);
        assert_eq!(digest, computed);
    }

    #[test]
    fn test_blob_deduplication() {
        let dir = tempdir().unwrap();
        let store = ContentStore::new(dir.path()).unwrap();

        let data = b"duplicate data";

        let digest1 = store.store_blob(data).unwrap();
        let digest2 = store.store_blob(data).unwrap();

        assert_eq!(digest1, digest2);

        // Verify both return the same data
        assert_eq!(store.get_blob(&digest1).unwrap(), data);
    }

    #[test]
    fn test_store_blob_repairs_corrupt_existing_file() {
        let dir = tempdir().unwrap();
        let store = ContentStore::new(dir.path()).unwrap();

        let data = b"repair me";
        let digest = store.compute_digest(data);
        let path = store.blob_path(&digest);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"").unwrap();

        let stored = store.store_blob(data).unwrap();
        assert_eq!(stored, digest);
        assert_eq!(store.get_blob(&digest).unwrap(), data);
    }

    #[test]
    fn test_blob_path_distribution() {
        let dir = tempdir().unwrap();
        let store = ContentStore::new(dir.path()).unwrap();

        let path = store.blob_path("sha256:abc123def456");
        assert!(path.to_string_lossy().contains("sha256"));
        assert!(path.to_string_lossy().contains("ab")); // Prefix dir
    }

    #[test]
    fn test_list_blobs_reports_stored_digest() {
        let dir = tempdir().unwrap();
        let store = ContentStore::new(dir.path()).unwrap();

        let digest = store.store_blob(b"blob inventory").unwrap();
        let blobs = store.list_blobs().unwrap();

        assert_eq!(blobs.len(), 1);
        assert_eq!(blobs[0].digest, digest);
        assert!(blobs[0].size_bytes > 0);
    }
}
