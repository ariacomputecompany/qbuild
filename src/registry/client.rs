//! Registry Client Implementation
//!
//! Implements OCI Distribution Spec API:
//! - GET /v2/ - API version check
//! - GET /v2/<name>/manifests/<reference> - Get manifest
//! - GET /v2/<name>/blobs/<digest> - Get blob
//! - HEAD /v2/<name>/blobs/<digest> - Check blob existence

use crate::image::{
    parse_manifest, ImageReference, ManifestKind, MediaType, OciImage, OciImageConfig, OciManifest,
};
use crate::registry::{auth::RegistryAuth, RegistryError, Result};
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, USER_AGENT};
use std::sync::Arc;
use tokio::sync::Semaphore;

/// Progress callback for pull operations
pub type PullProgress = Box<dyn Fn(PullEvent) + Send + Sync>;

/// Events during image pull
#[derive(Debug, Clone)]
pub enum PullEvent {
    /// Starting to pull
    Started { reference: String },

    /// Resolving manifest
    ResolvingManifest,

    /// Manifest resolved
    ManifestResolved { digest: String, layers: usize },

    /// Downloading layer
    DownloadingLayer {
        digest: String,
        current: usize,
        total: usize,
    },

    /// Layer downloaded
    LayerDownloaded {
        digest: String,
        size: u64,
        cached: bool,
    },

    /// Pull complete
    Complete { digest: String, size: u64 },

    /// Error occurred
    Error { message: String },
}

/// Options for pull operations
#[derive(Debug, Clone, Default)]
pub struct PullOptions {
    /// Force re-download even if cached
    pub force: bool,

    /// Maximum concurrent layer downloads
    pub max_concurrent: usize,
}

/// OCI Distribution registry client
pub struct RegistryClient {
    /// HTTP client
    http: reqwest::Client,

    /// Authentication manager
    auth: Arc<RegistryAuth>,

    /// User agent string
    user_agent: String,
}

impl RegistryClient {
    /// Create a new registry client
    pub fn new() -> Result<Self> {
        let auth = Arc::new(RegistryAuth::new());

        // Load Docker credentials
        let _ = auth.load_docker_config();

        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .connect_timeout(std::time::Duration::from_secs(30))
            .pool_max_idle_per_host(10)
            .build()?;

        Ok(Self {
            http,
            auth,
            user_agent: format!("quilt/{}", env!("CARGO_PKG_VERSION")),
        })
    }

    /// Inject or override credentials for a target registry host.
    pub fn set_registry_credentials(
        &self,
        registry: &str,
        username: &str,
        password: &str,
    ) -> Result<()> {
        self.auth.set_credentials(registry, username, password)
    }

    /// Pull an image from a registry
    pub async fn pull(
        &self,
        reference: &ImageReference,
        store: &crate::image::ImageStore,
        options: &PullOptions,
        progress: Option<&PullProgress>,
    ) -> Result<OciImage> {
        self.emit_progress(
            progress,
            PullEvent::Started {
                reference: reference.to_string(),
            },
        );

        // Resolve manifest
        self.emit_progress(progress, PullEvent::ResolvingManifest);

        let (manifest, manifest_digest, manifest_bytes) = self.get_manifest(reference).await?;

        self.emit_progress(
            progress,
            PullEvent::ManifestResolved {
                digest: manifest_digest.clone(),
                layers: manifest.layers.len(),
            },
        );

        // Download config blob and verify digest integrity.
        let config_data = self.get_blob(reference, &manifest.config.digest).await?;
        let actual_config_digest = store.content().compute_digest(&config_data);
        if actual_config_digest != manifest.config.digest {
            return Err(RegistryError::InvalidResponse(format!(
                "Config digest mismatch: expected {}, got {}",
                manifest.config.digest, actual_config_digest
            )));
        }
        let config: OciImageConfig = serde_json::from_slice(&config_data)?;

        let stored_config_digest = store.store_blob(&config_data)?;
        if stored_config_digest != manifest.config.digest {
            return Err(RegistryError::InvalidResponse(format!(
                "Config stored digest mismatch: expected {}, stored {}",
                manifest.config.digest, stored_config_digest
            )));
        }

        // Download layers concurrently
        let semaphore = Arc::new(Semaphore::new(options.max_concurrent.max(1)));
        let total_layers = manifest.layers.len();

        let mut handles = Vec::new();

        for (idx, layer) in manifest.layers.iter().enumerate() {
            let digest = layer.digest.clone();
            let reference = reference.clone();
            let client = self.clone_client();
            let store_ref = store.content().blob_path(&digest);
            let sem = semaphore.clone();
            let force = options.force;

            let handle = tokio::spawn(async move {
                let _permit = sem.acquire().await.unwrap();

                // Check if already cached with verified content.
                if !force {
                    match std::fs::read(&store_ref) {
                        Ok(existing) => {
                            use sha2::{Digest, Sha256};
                            let actual_digest = format!("sha256:{:x}", Sha256::digest(&existing));
                            if actual_digest == digest {
                                return Ok((digest, existing.len() as u64, true));
                            }
                            let _ = std::fs::remove_file(&store_ref);
                        }
                        Err(_) => {
                            let _ = std::fs::remove_file(&store_ref);
                        }
                    }
                }

                // Download blob
                let data = client.get_blob(&reference, &digest).await?;

                // Verify and persist layer at its expected digest path.
                let actual_digest = {
                    use sha2::{Digest, Sha256};
                    format!("sha256:{:x}", Sha256::digest(&data))
                };
                if actual_digest != digest {
                    return Err(RegistryError::InvalidResponse(format!(
                        "Layer digest mismatch: expected {}, got {}",
                        digest, actual_digest
                    )));
                }

                if let Some(parent) = store_ref.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let temp_path = store_ref.with_extension("tmp");
                std::fs::write(&temp_path, &data)?;
                std::fs::rename(&temp_path, &store_ref)?;

                Ok::<_, RegistryError>((digest, data.len() as u64, false))
            });

            handles.push((idx, handle));
        }

        // Wait for all downloads and emit progress
        let mut total_size = 0u64;

        for (idx, handle) in handles {
            match handle.await {
                Ok(Ok((digest, size, cached))) => {
                    self.emit_progress(
                        progress,
                        PullEvent::DownloadingLayer {
                            digest: digest.clone(),
                            current: idx + 1,
                            total: total_layers,
                        },
                    );

                    self.emit_progress(
                        progress,
                        PullEvent::LayerDownloaded {
                            digest,
                            size,
                            cached,
                        },
                    );

                    total_size += size;
                }
                Ok(Err(e)) => {
                    self.emit_progress(
                        progress,
                        PullEvent::Error {
                            message: e.to_string(),
                        },
                    );
                    return Err(e);
                }
                Err(e) => {
                    let msg = format!("Download task failed: {}", e);
                    self.emit_progress(
                        progress,
                        PullEvent::Error {
                            message: msg.clone(),
                        },
                    );
                    return Err(RegistryError::InvalidResponse(msg));
                }
            }
        }

        // Store manifest with canonical digest derived from the exact body bytes.
        let canonical_manifest_digest = store.store_blob(&manifest_bytes)?;

        // Store reference mapping
        store.store_image_ref(reference, &canonical_manifest_digest)?;

        self.emit_progress(
            progress,
            PullEvent::Complete {
                digest: manifest_digest.clone(),
                size: total_size,
            },
        );

        Ok(OciImage {
            reference: reference.clone(),
            manifest,
            config,
            digest: canonical_manifest_digest,
            size: total_size,
        })
    }

    /// Get a manifest by reference
    pub async fn get_manifest(
        &self,
        reference: &ImageReference,
    ) -> Result<(OciManifest, String, Vec<u8>)> {
        let url = format!(
            "{}/v2/{}/manifests/{}",
            reference.api_endpoint(),
            reference.repository,
            reference.api_reference()
        );

        let accept_types = [
            MediaType::OciManifest.to_string(),
            MediaType::OciIndex.to_string(),
            MediaType::DockerManifestV2.to_string(),
            MediaType::DockerManifestList.to_string(),
        ]
        .join(", ");

        let response = self
            .authenticated_request(&url, reference, &accept_types)
            .await?;

        // Capture digest from header (if present) and always compute digest
        // from the exact body bytes for canonical local storage.
        let header_digest = response
            .headers()
            .get("docker-content-digest")
            .and_then(|h| h.to_str().ok())
            .map(|s| s.to_string());

        let body = response.bytes().await?;
        use sha2::{Digest, Sha256};
        let computed_digest = format!("sha256:{:x}", Sha256::digest(&body));
        if let Some(h) = &header_digest {
            if h != &computed_digest {
                eprintln!(
                    "warning: manifest digest header/body mismatch for {}: header={}, computed={}",
                    reference, h, computed_digest
                );
            }
        }

        // Parse manifest
        let manifest_kind = parse_manifest(&body)?;

        match manifest_kind {
            ManifestKind::Oci(m) => Ok((m, computed_digest, body.to_vec())),
            ManifestKind::Docker(m) => Ok((m.to_oci_manifest(), computed_digest, body.to_vec())),
            ManifestKind::List(list) => {
                // Multi-arch image - find matching platform
                let platform_manifest = list
                    .find_current_platform()
                    .or_else(|| list.find_platform("amd64", "linux"))
                    .ok_or_else(|| {
                        RegistryError::ManifestNotFound(
                            "No matching platform in manifest list".to_string(),
                        )
                    })?;

                // Fetch the actual manifest
                let new_ref = reference.with_digest(&platform_manifest.descriptor.digest);
                // Use Box::pin to handle recursive async call
                Box::pin(self.get_manifest(&new_ref)).await
            }
        }
    }

    /// Get a blob by digest
    pub async fn get_blob(&self, reference: &ImageReference, digest: &str) -> Result<Vec<u8>> {
        let url = format!(
            "{}/v2/{}/blobs/{}",
            reference.api_endpoint(),
            reference.repository,
            digest
        );

        let response = self.authenticated_request(&url, reference, "*/*").await?;

        let bytes = response.bytes().await?;

        // Verify digest
        use sha2::{Digest, Sha256};
        let actual_digest = format!("sha256:{:x}", Sha256::digest(&bytes));

        if actual_digest != digest {
            return Err(RegistryError::InvalidResponse(format!(
                "Digest mismatch: expected {}, got {}",
                digest, actual_digest
            )));
        }

        Ok(bytes.to_vec())
    }

    /// Make an authenticated GET request
    async fn authenticated_request(
        &self,
        url: &str,
        reference: &ImageReference,
        accept: &str,
    ) -> Result<reqwest::Response> {
        // First attempt without auth
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_str(&self.user_agent).unwrap());
        headers.insert(ACCEPT, HeaderValue::from_str(accept).unwrap());

        let response = self.http.get(url).headers(headers.clone()).send().await?;

        if response.status() == 401 {
            // Need authentication
            let www_auth = response
                .headers()
                .get("www-authenticate")
                .and_then(|h| h.to_str().ok())
                .ok_or_else(|| {
                    RegistryError::AuthError("No WWW-Authenticate header".to_string())
                })?;

            let token = self.auth.get_token(www_auth, &reference.registry).await?;

            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {}", token)).unwrap(),
            );

            let response = self.http.get(url).headers(headers).send().await?;

            if !response.status().is_success() {
                if response.status() == 404 {
                    return Err(RegistryError::ManifestNotFound(url.to_string()));
                }
                if response.status() == 429 {
                    let retry_after = response
                        .headers()
                        .get("retry-after")
                        .and_then(|h| h.to_str().ok())
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(60);
                    return Err(RegistryError::RateLimited(retry_after));
                }

                return Err(RegistryError::InvalidResponse(format!(
                    "{}: {}",
                    response.status(),
                    response.text().await.unwrap_or_default()
                )));
            }

            Ok(response)
        } else if response.status().is_success() {
            Ok(response)
        } else if response.status() == 404 {
            Err(RegistryError::ManifestNotFound(url.to_string()))
        } else {
            Err(RegistryError::InvalidResponse(format!(
                "{}: {}",
                response.status(),
                response.text().await.unwrap_or_default()
            )))
        }
    }

    /// Emit a progress event
    fn emit_progress(&self, progress: Option<&PullProgress>, event: PullEvent) {
        if let Some(cb) = progress {
            cb(event);
        }
    }

    /// Clone the client (shares HTTP client and auth)
    fn clone_client(&self) -> Self {
        Self {
            http: self.http.clone(),
            auth: self.auth.clone(),
            user_agent: self.user_agent.clone(),
        }
    }
}

impl Default for RegistryClient {
    fn default() -> Self {
        Self::new().expect("Failed to create registry client")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pull_options_default() {
        let opts = PullOptions::default();
        assert!(!opts.force);
        assert_eq!(opts.max_concurrent, 0);
    }

    #[tokio::test]
    async fn test_client_creation() {
        let client = RegistryClient::new();
        assert!(client.is_ok());
    }
}
