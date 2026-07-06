mod build;
mod image;
mod registry;

use build::{BuildRequest, BuildService};
use clap::{Parser, Subcommand};
use image::{ImageReference, ImageStore, OciManifest};
use registry::{PullEvent, PullOptions, RegistryClient};
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
enum AppError {
    #[error("{0}")]
    Message(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Image(#[from] image::ImageError),
    #[error(transparent)]
    Registry(#[from] registry::RegistryError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[derive(Parser, Debug)]
#[command(name = "qbuild", version, about = "Standalone OCI image builder for Quilt-compatible artifacts")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Build an OCI image from a local Docker build context
    Build {
        #[arg(default_value = ".")]
        context: String,
        #[arg(long, default_value = "Dockerfile")]
        dockerfile: String,
        #[arg(long)]
        image: String,
        #[arg(long = "build-arg")]
        build_arg: Vec<String>,
        #[arg(long)]
        target: Option<String>,
        #[arg(long)]
        store_dir: Option<PathBuf>,
        #[arg(long)]
        work_dir: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Pull an OCI image into the local qbuild store
    Pull {
        reference: String,
        #[arg(long)]
        store_dir: Option<PathBuf>,
        #[arg(long)]
        username: Option<String>,
        #[arg(long)]
        password: Option<String>,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        json: bool,
    },
    /// Inspect a locally stored OCI image reference
    Inspect {
        reference: String,
        #[arg(long)]
        store_dir: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// List locally stored image references
    List {
        #[arg(long)]
        store_dir: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Serialize)]
struct InspectOutput {
    reference: String,
    manifest_digest: String,
    config_digest: String,
    architecture: String,
    os: String,
    size_bytes: u64,
    layers: usize,
    env: Vec<String>,
    cmd: Vec<String>,
    entrypoint: Vec<String>,
    working_dir: Option<String>,
    user: Option<String>,
}

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("error: {}", err);
        std::process::exit(1);
    }
}

async fn run() -> Result<(), AppError> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Build {
            context,
            dockerfile,
            image,
            build_arg,
            target,
            store_dir,
            work_dir,
            json,
        } => {
            let store_dir = store_dir.unwrap_or_else(default_store_dir);
            let work_dir = work_dir.unwrap_or_else(default_work_dir);
            std::fs::create_dir_all(&store_dir)?;
            std::fs::create_dir_all(&work_dir)?;
            let service = BuildService::new(store_dir.clone(), work_dir);
            let result = service
                .build_image(BuildRequest {
                    context_dir: PathBuf::from(context),
                    dockerfile_path: dockerfile,
                    image_reference: image,
                    build_args: parse_build_args(&build_arg)?,
                    target_stage: target,
                })
                .await
                .map_err(AppError::Message)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("Built {}", result.image_reference);
                println!("Manifest: {}", result.manifest_digest);
                println!("Config: {}", result.config_digest);
                println!("Store: {}", store_dir.display());
                println!("Size: {} bytes", result.size_bytes);
            }
        }
        Commands::Pull {
            reference,
            store_dir,
            username,
            password,
            force,
            json,
        } => {
            let store_dir = store_dir.unwrap_or_else(default_store_dir);
            std::fs::create_dir_all(&store_dir)?;
            let store = ImageStore::new(&store_dir)?;
            let reference = ImageReference::parse(&reference)
                .map_err(|e| AppError::Message(format!("Invalid reference: {}", e)))?;
            let client = RegistryClient::new()?;
            let progress: registry::PullProgress = Box::new(print_pull_event);
            match (username.as_deref(), password.as_deref()) {
                (Some(username), Some(password)) => {
                    client.set_registry_credentials(&reference.registry, username, password)?;
                }
                (Some(_), None) | (None, Some(_)) => {
                    return Err(AppError::Message(
                        "Both --username and --password are required together".to_string(),
                    ));
                }
                _ => {}
            }
            let image = client
                .pull(
                    &reference,
                    &store,
                    &PullOptions {
                        force,
                        max_concurrent: 4,
                    },
                    Some(&progress),
                )
                .await?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "reference": image.reference.to_string(),
                        "manifest_digest": image.digest,
                        "config_digest": image.manifest.config.digest,
                        "size_bytes": image.size,
                        "layers": image.manifest.layers.len(),
                    }))?
                );
            } else {
                println!("Pulled {}", image.reference);
                println!("Manifest: {}", image.digest);
                println!("Config: {}", image.manifest.config.digest);
                println!("Layers: {}", image.manifest.layers.len());
                println!("Store: {}", store_dir.display());
            }
        }
        Commands::Inspect {
            reference,
            store_dir,
            json,
        } => {
            let store_dir = store_dir.unwrap_or_else(default_store_dir);
            let output = inspect_reference(&store_dir, &reference).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else {
                println!("Reference: {}", output.reference);
                println!("Manifest: {}", output.manifest_digest);
                println!("Config: {}", output.config_digest);
                println!("Platform: {}/{}", output.os, output.architecture);
                println!("Layers: {}", output.layers);
                println!("Size: {} bytes", output.size_bytes);
            }
        }
        Commands::List { store_dir, json } => {
            let store_dir = store_dir.unwrap_or_else(default_store_dir);
            let store = ImageStore::new(&store_dir)?;
            let refs = store.list_image_refs()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&refs)?);
            } else if refs.is_empty() {
                println!("No local images in {}", store_dir.display());
            } else {
                for (reference, digest) in refs {
                    println!("{}\t{}", reference, digest);
                }
            }
        }
    }
    Ok(())
}

async fn inspect_reference(store_dir: &Path, reference: &str) -> Result<InspectOutput, AppError> {
    let store = ImageStore::new(store_dir)?;
    let reference = ImageReference::parse(reference)
        .map_err(|e| AppError::Message(format!("Invalid reference: {}", e)))?;
    let manifest_digest = store
        .resolve_image_ref(&reference)?
        .ok_or_else(|| AppError::Message(format!("Reference '{}' not found locally", reference)))?;
    let manifest_bytes = store.get_blob(&manifest_digest)?;
    let manifest: OciManifest = serde_json::from_slice(&manifest_bytes)?;
    let manager = image::ImageManager::new(store_dir)?;
    let image = manager
        .load_image(&reference, &manifest_digest, &manifest.config.digest)
        .await?;
    let architecture = image.config.architecture.clone();
    let os = image.config.os.clone();
    Ok(InspectOutput {
        reference: image.reference.to_string(),
        manifest_digest,
        config_digest: manifest.config.digest.clone(),
        architecture,
        os,
        size_bytes: image.size,
        layers: manifest.layers.len(),
        env: image.env(),
        cmd: image.default_cmd().unwrap_or_default(),
        entrypoint: image.entrypoint().unwrap_or_default(),
        working_dir: image.working_dir(),
        user: image.user(),
    })
}

fn parse_build_args(values: &[String]) -> Result<HashMap<String, String>, AppError> {
    let mut args = HashMap::new();
    for value in values {
        let Some((key, value)) = value.split_once('=') else {
            return Err(AppError::Message(format!(
                "Invalid build arg '{}', expected KEY=VALUE",
                value
            )));
        };
        args.insert(key.to_string(), value.to_string());
    }
    Ok(args)
}

fn default_data_root() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".qbuild")
}

fn default_store_dir() -> PathBuf {
    default_data_root().join("images")
}

fn default_work_dir() -> PathBuf {
    default_data_root().join("builds")
}

fn print_pull_event(event: PullEvent) {
    match event {
        PullEvent::Started { reference } => eprintln!("pulling {}", reference),
        PullEvent::ResolvingManifest => eprintln!("resolving manifest"),
        PullEvent::ManifestResolved { digest, layers } => {
            eprintln!("manifest {} with {} layers", digest, layers)
        }
        PullEvent::DownloadingLayer {
            digest,
            current,
            total,
        } => eprintln!("layer {}/{} {}", current, total, digest),
        PullEvent::LayerDownloaded {
            digest,
            size,
            cached,
        } => eprintln!(
            "downloaded {} ({} bytes{})",
            digest,
            size,
            if cached { ", cached" } else { "" }
        ),
        PullEvent::Complete { digest, size } => {
            eprintln!("complete {} ({} bytes)", digest, size)
        }
        PullEvent::Error { message } => eprintln!("pull error: {}", message),
    }
}
