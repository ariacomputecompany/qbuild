mod build;
mod containers;
mod image;
mod registry;
mod runtime;

use build::{BuildRequest, BuildService};
use clap::{Parser, Subcommand};
use containers::{ContainerStore, CreateContainerRequest};
use image::{ImageReference, ImageStore, OciManifest};
use nix::sys::signal::Signal;
use registry::{PullEvent, PullOptions, PushEvent, RegistryClient};
use runtime::{NamespaceConfig, ResourceLimits, RunRequest, RunService};
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
#[command(
    name = "qbuild",
    version,
    about = "Standalone OCI image builder for Quilt-compatible artifacts"
)]
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
    /// Push a locally stored OCI image reference to a registry
    Push {
        reference: String,
        #[arg(long)]
        store_dir: Option<PathBuf>,
        #[arg(long)]
        username: Option<String>,
        #[arg(long)]
        password: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Run a locally stored OCI image standalone
    Run {
        reference: String,
        #[arg(long)]
        store_dir: Option<PathBuf>,
        #[arg(short = 'e', long = "env")]
        env: Vec<String>,
        #[arg(long)]
        workdir: Option<String>,
        #[arg(long)]
        memory_mb: Option<u64>,
        #[arg(long)]
        cpu_percent: Option<f64>,
        #[arg(long)]
        pids_limit: Option<u64>,
        #[arg(long)]
        clear_image_env: bool,
        #[arg(long)]
        no_mount_namespace: bool,
        #[arg(long)]
        uts_namespace: bool,
        #[arg(long)]
        ipc_namespace: bool,
        #[arg(long)]
        network_namespace: bool,
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// Create a persistent local container definition
    Create {
        reference: String,
        #[arg(long)]
        store_dir: Option<PathBuf>,
        #[arg(long)]
        data_root: Option<PathBuf>,
        #[arg(short = 'e', long = "env")]
        env: Vec<String>,
        #[arg(long)]
        workdir: Option<String>,
        #[arg(long)]
        memory_mb: Option<u64>,
        #[arg(long)]
        cpu_percent: Option<f64>,
        #[arg(long)]
        pids_limit: Option<u64>,
        #[arg(long)]
        clear_image_env: bool,
        #[arg(long)]
        no_mount_namespace: bool,
        #[arg(long)]
        uts_namespace: bool,
        #[arg(long)]
        ipc_namespace: bool,
        #[arg(long)]
        network_namespace: bool,
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// Start a persistent local container
    Start {
        id: String,
        #[arg(long)]
        store_dir: Option<PathBuf>,
        #[arg(long)]
        data_root: Option<PathBuf>,
    },
    /// Stop a running local container
    Stop {
        id: String,
        #[arg(long)]
        store_dir: Option<PathBuf>,
        #[arg(long)]
        data_root: Option<PathBuf>,
        #[arg(long, default_value = "term")]
        signal: String,
    },
    /// Remove a stopped local container
    Rm {
        id: String,
        #[arg(long)]
        store_dir: Option<PathBuf>,
        #[arg(long)]
        data_root: Option<PathBuf>,
    },
    /// List local containers
    Ps {
        #[arg(long)]
        store_dir: Option<PathBuf>,
        #[arg(long)]
        data_root: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Print container logs
    Logs {
        id: String,
        #[arg(long)]
        data_root: Option<PathBuf>,
        #[arg(long)]
        store_dir: Option<PathBuf>,
        #[arg(long)]
        stderr: bool,
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
    #[command(hide = true)]
    __InternalExec {
        #[arg(long)]
        data_root: PathBuf,
        #[arg(long)]
        store_dir: PathBuf,
        #[arg(long)]
        container_id: String,
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
        Commands::Push {
            reference,
            store_dir,
            username,
            password,
            json,
        } => {
            let store_dir = store_dir.unwrap_or_else(default_store_dir);
            let store = ImageStore::new(&store_dir)?;
            let reference = ImageReference::parse(&reference)
                .map_err(|e| AppError::Message(format!("Invalid reference: {}", e)))?;
            let manifest_digest = store.resolve_image_ref(&reference)?.ok_or_else(|| {
                AppError::Message(format!("Reference '{}' not found locally", reference))
            })?;
            let client = RegistryClient::new()?;
            let progress: registry::PushProgress = Box::new(print_push_event);
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
            client
                .push(&reference, &store, &manifest_digest, Some(&progress))
                .await?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "reference": reference.to_string(),
                        "manifest_digest": manifest_digest,
                        "registry": reference.registry,
                        "repository": reference.repository,
                    }))?
                );
            } else {
                println!("Pushed {}", reference);
                println!("Manifest: {}", manifest_digest);
            }
        }
        Commands::Run {
            reference,
            store_dir,
            env,
            workdir,
            memory_mb,
            cpu_percent,
            pids_limit,
            clear_image_env,
            no_mount_namespace,
            uts_namespace,
            ipc_namespace,
            network_namespace,
            command,
        } => {
            let store_dir = store_dir.unwrap_or_else(default_store_dir);
            let service = RunService::new();
            let limits = build_limits(memory_mb, cpu_percent, pids_limit);
            let result = service
                .run(RunRequest {
                    image_reference: reference,
                    command,
                    environment: parse_env_overrides(&env)?,
                    working_directory: workdir,
                    store_dir,
                    namespace_config: NamespaceConfig {
                        mount: !no_mount_namespace,
                        uts: uts_namespace,
                        ipc: ipc_namespace,
                        network: network_namespace,
                    },
                    resource_limits: limits,
                    clear_image_env,
                    container_id: None,
                    status_file: None,
                    started_at: None,
                })
                .await
                .map_err(AppError::Message)?;

            if !result.exit_status.success() {
                let code = result.exit_status.code().unwrap_or(1);
                std::process::exit(code);
            }
        }
        Commands::Create {
            reference,
            store_dir,
            data_root,
            env,
            workdir,
            memory_mb,
            cpu_percent,
            pids_limit,
            clear_image_env,
            no_mount_namespace,
            uts_namespace,
            ipc_namespace,
            network_namespace,
            command,
        } => {
            let store_dir = store_dir.unwrap_or_else(default_store_dir);
            let data_root = data_root.unwrap_or_else(default_data_root);
            let store = ContainerStore::new(&data_root, &store_dir).map_err(AppError::Message)?;
            let record = store
                .create(CreateContainerRequest {
                    image_reference: reference,
                    command,
                    environment: parse_env_overrides(&env)?,
                    working_directory: workdir,
                    namespace_config: NamespaceConfig {
                        mount: !no_mount_namespace,
                        uts: uts_namespace,
                        ipc: ipc_namespace,
                        network: network_namespace,
                    },
                    resource_limits: build_limits(memory_mb, cpu_percent, pids_limit),
                    clear_image_env,
                })
                .map_err(AppError::Message)?;
            println!("{}", record.id);
        }
        Commands::Start {
            id,
            store_dir,
            data_root,
        } => {
            let store_dir = store_dir.unwrap_or_else(default_store_dir);
            let data_root = data_root.unwrap_or_else(default_data_root);
            let store = ContainerStore::new(&data_root, &store_dir).map_err(AppError::Message)?;
            let record = store.start(&id).map_err(AppError::Message)?;
            println!("{} {}", record.id, record.pid.unwrap_or_default());
        }
        Commands::Stop {
            id,
            store_dir,
            data_root,
            signal,
        } => {
            let store_dir = store_dir.unwrap_or_else(default_store_dir);
            let data_root = data_root.unwrap_or_else(default_data_root);
            let store = ContainerStore::new(&data_root, &store_dir).map_err(AppError::Message)?;
            let record = store
                .stop(&id, parse_signal(&signal)?)
                .map_err(AppError::Message)?;
            println!("{}", record.id);
        }
        Commands::Rm {
            id,
            store_dir,
            data_root,
        } => {
            let store_dir = store_dir.unwrap_or_else(default_store_dir);
            let data_root = data_root.unwrap_or_else(default_data_root);
            let store = ContainerStore::new(&data_root, &store_dir).map_err(AppError::Message)?;
            store.remove(&id).map_err(AppError::Message)?;
        }
        Commands::Ps {
            store_dir,
            data_root,
            json,
        } => {
            let store_dir = store_dir.unwrap_or_else(default_store_dir);
            let data_root = data_root.unwrap_or_else(default_data_root);
            let store = ContainerStore::new(&data_root, &store_dir).map_err(AppError::Message)?;
            let containers = store.list().map_err(AppError::Message)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&containers)?);
            } else {
                for container in containers {
                    println!(
                        "{}\t{}\t{:?}\t{}",
                        container.id,
                        container.image_reference,
                        container.state,
                        container.pid.map(|pid| pid.to_string()).unwrap_or_default()
                    );
                }
            }
        }
        Commands::Logs {
            id,
            data_root,
            store_dir,
            stderr,
        } => {
            let store_dir = store_dir.unwrap_or_else(default_store_dir);
            let data_root = data_root.unwrap_or_else(default_data_root);
            let store = ContainerStore::new(&data_root, &store_dir).map_err(AppError::Message)?;
            print!("{}", store.logs(&id, stderr).map_err(AppError::Message)?);
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
        Commands::__InternalExec {
            data_root,
            store_dir,
            container_id,
        } => {
            let store = ContainerStore::new(&data_root, &store_dir).map_err(AppError::Message)?;
            let result = store
                .run_managed(&container_id)
                .await
                .map_err(AppError::Message)?;
            if !result.exit_status.success() {
                std::process::exit(result.exit_status.code().unwrap_or(1));
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

fn parse_env_overrides(values: &[String]) -> Result<HashMap<String, String>, AppError> {
    let mut env = HashMap::new();
    for value in values {
        let Some((key, value)) = value.split_once('=') else {
            return Err(AppError::Message(format!(
                "Invalid env override '{}', expected KEY=VALUE",
                value
            )));
        };
        env.insert(key.to_string(), value.to_string());
    }
    Ok(env)
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

fn build_limits(
    memory_mb: Option<u64>,
    cpu_percent: Option<f64>,
    pids_limit: Option<u64>,
) -> Option<ResourceLimits> {
    if memory_mb.is_some() || cpu_percent.is_some() || pids_limit.is_some() {
        Some(ResourceLimits {
            memory_limit_bytes: memory_mb.map(|mb| mb * 1024 * 1024),
            cpu_quota: cpu_percent.map(|percent| (percent * 1000.0) as i64),
            cpu_period: cpu_percent.map(|_| 100000),
            pids_limit,
        })
    } else {
        None
    }
}

fn parse_signal(value: &str) -> Result<Signal, AppError> {
    match value.to_ascii_lowercase().as_str() {
        "term" | "sigterm" => Ok(Signal::SIGTERM),
        "kill" | "sigkill" => Ok(Signal::SIGKILL),
        "int" | "sigint" => Ok(Signal::SIGINT),
        other => Err(AppError::Message(format!(
            "Unsupported signal '{}', expected term|kill|int",
            other
        ))),
    }
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

fn print_push_event(event: PushEvent) {
    match event {
        PushEvent::Started { reference } => eprintln!("pushing {}", reference),
        PushEvent::UploadingBlob { digest, size } => {
            eprintln!("uploading {} ({} bytes)", digest, size)
        }
        PushEvent::BlobMounted { digest } => eprintln!("blob already present {}", digest),
        PushEvent::BlobUploaded { digest, size } => {
            eprintln!("uploaded {} ({} bytes)", digest, size)
        }
        PushEvent::ManifestUploaded { digest } => eprintln!("uploaded manifest {}", digest),
        PushEvent::Complete { reference, digest } => {
            eprintln!("push complete {} ({})", reference, digest)
        }
    }
}
