use crate::build::{BuildRequest, BuildService};
use crate::cli::{
    BuildCommand, CreateCommand, InspectCommand, ListCommand, LogsCommand, PsCommand, PullCommand,
    PushCommand, RemoveCommand, RunCommand, StartCommand, StopCommand,
};
use crate::containers::{ContainerStore, CreateContainerRequest};
use crate::error::AppError;
use crate::image::{ImageReference, ImageStore, OciManifest};
use crate::protocol::{
    BuildOutput, CommandRequest, ContainerSummaryOutput, CreateOutput, GuestEvent, GuestResponse,
    ImageReferenceOutput, InspectOutput, ListOutput, LogsOutput, NamespaceConfig, PsOutput,
    PullOutput, PushOutput, RemoveOutput, ResourceLimits, RunOutput, StartOutput, StopOutput,
};
use crate::registry::{PullEvent, PullOptions, PushEvent, RegistryClient};
use crate::runtime::{RunRequest, RunService};
use nix::sys::signal::Signal;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub async fn execute(
    request: CommandRequest,
    emit: Arc<dyn Fn(GuestEvent) + Send + Sync>,
) -> Result<GuestResponse, AppError> {
    match request {
        CommandRequest::Ping => Ok(GuestResponse::Pong),
        CommandRequest::Build(cmd) => Ok(GuestResponse::Build(build(cmd, emit).await?)),
        CommandRequest::Pull(cmd) => Ok(GuestResponse::Pull(pull(cmd, emit).await?)),
        CommandRequest::Push(cmd) => Ok(GuestResponse::Push(push(cmd, emit).await?)),
        CommandRequest::Run(cmd) => Ok(GuestResponse::Run(run(cmd).await?)),
        CommandRequest::Create(cmd) => Ok(GuestResponse::Create(create(cmd)?)),
        CommandRequest::Start(cmd) => Ok(GuestResponse::Start(start(cmd)?)),
        CommandRequest::Stop(cmd) => Ok(GuestResponse::Stop(stop(cmd)?)),
        CommandRequest::Rm(cmd) => Ok(GuestResponse::Removed(remove(cmd)?)),
        CommandRequest::Ps(cmd) => Ok(GuestResponse::Ps(ps(cmd)?)),
        CommandRequest::Logs(cmd) => Ok(GuestResponse::Logs(logs(cmd)?)),
        CommandRequest::Inspect(cmd) => Ok(GuestResponse::Inspect(inspect(cmd).await?)),
        CommandRequest::List(cmd) => Ok(GuestResponse::List(list(cmd)?)),
    }
}

async fn build(
    cmd: BuildCommand,
    emit: Arc<dyn Fn(GuestEvent) + Send + Sync>,
) -> Result<BuildOutput, AppError> {
    let store_dir = cmd.store_dir.unwrap_or_else(crate::default_store_dir);
    let work_dir = cmd.work_dir.unwrap_or_else(crate::default_work_dir);
    std::fs::create_dir_all(&store_dir)?;
    std::fs::create_dir_all(&work_dir)?;
    emit(GuestEvent::Status(format!(
        "building {} from {}",
        cmd.image, cmd.context
    )));
    let service = BuildService::new(store_dir, work_dir);
    let result = service
        .build_image(BuildRequest {
            context_dir: PathBuf::from(cmd.context),
            dockerfile_path: cmd.dockerfile,
            image_reference: cmd.image,
            build_args: parse_key_values(&cmd.build_arg, "build arg")?,
            target_stage: cmd.target,
        })
        .await
        .map_err(AppError::Message)?;
    Ok(BuildOutput {
        image_reference: result.image_reference,
        manifest_digest: result.manifest_digest,
        config_digest: result.config_digest,
        size_bytes: result.size_bytes,
    })
}

async fn pull(
    cmd: PullCommand,
    emit: Arc<dyn Fn(GuestEvent) + Send + Sync>,
) -> Result<PullOutput, AppError> {
    let store_dir = cmd.store_dir.unwrap_or_else(crate::default_store_dir);
    std::fs::create_dir_all(&store_dir)?;
    let store = ImageStore::new(&store_dir)?;
    let reference = ImageReference::parse(&cmd.reference)
        .map_err(|e| AppError::Message(format!("Invalid reference: {}", e)))?;
    let client = RegistryClient::new()?;
    apply_registry_auth(
        &client,
        &reference.registry,
        cmd.username.as_deref(),
        cmd.password.as_deref(),
    )?;
    let progress_emit = Arc::clone(&emit);
    let progress: crate::registry::PullProgress = Box::new(move |event| {
        progress_emit(GuestEvent::Status(format_pull_event(event)));
    });
    let image = client
        .pull(
            &reference,
            &store,
            &PullOptions {
                force: cmd.force,
                max_concurrent: 4,
            },
            Some(&progress),
        )
        .await?;
    Ok(PullOutput {
        reference: image.reference.to_string(),
        manifest_digest: image.digest,
        config_digest: image.manifest.config.digest,
        size_bytes: image.size,
        layers: image.manifest.layers.len(),
    })
}

async fn push(
    cmd: PushCommand,
    emit: Arc<dyn Fn(GuestEvent) + Send + Sync>,
) -> Result<PushOutput, AppError> {
    let store_dir = cmd.store_dir.unwrap_or_else(crate::default_store_dir);
    let store = ImageStore::new(&store_dir)?;
    let reference = ImageReference::parse(&cmd.reference)
        .map_err(|e| AppError::Message(format!("Invalid reference: {}", e)))?;
    let manifest_digest = store
        .resolve_image_ref(&reference)?
        .ok_or_else(|| AppError::Message(format!("Reference '{}' not found locally", reference)))?;
    let client = RegistryClient::new()?;
    apply_registry_auth(
        &client,
        &reference.registry,
        cmd.username.as_deref(),
        cmd.password.as_deref(),
    )?;
    let progress_emit = Arc::clone(&emit);
    let progress: crate::registry::PushProgress = Box::new(move |event| {
        progress_emit(GuestEvent::Status(format_push_event(event)));
    });
    client
        .push(&reference, &store, &manifest_digest, Some(&progress))
        .await?;
    Ok(PushOutput {
        reference: reference.to_string(),
        manifest_digest,
        registry: reference.registry,
        repository: reference.repository,
    })
}

async fn run(cmd: RunCommand) -> Result<RunOutput, AppError> {
    let store_dir = cmd.store_dir.unwrap_or_else(crate::default_store_dir);
    let service = RunService::new();
    let result = service
        .run(RunRequest {
            image_reference: cmd.reference,
            command: cmd.command,
            environment: parse_key_values(&cmd.env, "env override")?,
            working_directory: cmd.workdir,
            store_dir,
            namespace_config: NamespaceConfig {
                mount: !cmd.no_mount_namespace,
                uts: cmd.uts_namespace,
                ipc: cmd.ipc_namespace,
                network: cmd.network_namespace,
            },
            resource_limits: build_limits(cmd.memory_mb, cmd.cpu_percent, cmd.pids_limit),
            clear_image_env: cmd.clear_image_env,
            container_id: None,
            status_file: None,
            started_at: None,
        })
        .await
        .map_err(AppError::Message)?;
    Ok(RunOutput {
        exit_code: result.exit_status.code().unwrap_or(1),
    })
}

fn create(cmd: CreateCommand) -> Result<CreateOutput, AppError> {
    let store_dir = cmd.store_dir.unwrap_or_else(crate::default_store_dir);
    let data_root = cmd.data_root.unwrap_or_else(crate::default_data_root);
    let store = ContainerStore::new(&data_root, &store_dir).map_err(AppError::Message)?;
    let record = store
        .create(CreateContainerRequest {
            image_reference: cmd.reference,
            command: cmd.command,
            environment: parse_key_values(&cmd.env, "env override")?,
            working_directory: cmd.workdir,
            namespace_config: NamespaceConfig {
                mount: !cmd.no_mount_namespace,
                uts: cmd.uts_namespace,
                ipc: cmd.ipc_namespace,
                network: cmd.network_namespace,
            },
            resource_limits: build_limits(cmd.memory_mb, cmd.cpu_percent, cmd.pids_limit),
            clear_image_env: cmd.clear_image_env,
        })
        .map_err(AppError::Message)?;
    Ok(CreateOutput { id: record.id })
}

fn start(cmd: StartCommand) -> Result<StartOutput, AppError> {
    let store_dir = cmd.store_dir.unwrap_or_else(crate::default_store_dir);
    let data_root = cmd.data_root.unwrap_or_else(crate::default_data_root);
    let store = ContainerStore::new(&data_root, &store_dir).map_err(AppError::Message)?;
    let record = store.start(&cmd.id).map_err(AppError::Message)?;
    Ok(StartOutput {
        id: record.id,
        pid: record.pid.unwrap_or_default(),
    })
}

fn stop(cmd: StopCommand) -> Result<StopOutput, AppError> {
    let store_dir = cmd.store_dir.unwrap_or_else(crate::default_store_dir);
    let data_root = cmd.data_root.unwrap_or_else(crate::default_data_root);
    let store = ContainerStore::new(&data_root, &store_dir).map_err(AppError::Message)?;
    let record = store
        .stop(&cmd.id, parse_signal(&cmd.signal)?)
        .map_err(AppError::Message)?;
    Ok(StopOutput { id: record.id })
}

fn remove(cmd: RemoveCommand) -> Result<RemoveOutput, AppError> {
    let store_dir = cmd.store_dir.unwrap_or_else(crate::default_store_dir);
    let data_root = cmd.data_root.unwrap_or_else(crate::default_data_root);
    let store = ContainerStore::new(&data_root, &store_dir).map_err(AppError::Message)?;
    store.remove(&cmd.id).map_err(AppError::Message)?;
    Ok(RemoveOutput { id: cmd.id })
}

fn ps(cmd: PsCommand) -> Result<PsOutput, AppError> {
    let store_dir = cmd.store_dir.unwrap_or_else(crate::default_store_dir);
    let data_root = cmd.data_root.unwrap_or_else(crate::default_data_root);
    let store = ContainerStore::new(&data_root, &store_dir).map_err(AppError::Message)?;
    let containers = store.list().map_err(AppError::Message)?;
    Ok(PsOutput {
        containers: containers
            .into_iter()
            .map(|container| ContainerSummaryOutput {
                id: container.id,
                image_reference: container.image_reference,
                state: container.state,
                pid: container.pid,
                exit_code: container.exit_code,
                created_at: container.created_at,
                started_at: container.started_at,
                finished_at: container.finished_at,
            })
            .collect(),
    })
}

fn logs(cmd: LogsCommand) -> Result<LogsOutput, AppError> {
    let store_dir = cmd.store_dir.unwrap_or_else(crate::default_store_dir);
    let data_root = cmd.data_root.unwrap_or_else(crate::default_data_root);
    let store = ContainerStore::new(&data_root, &store_dir).map_err(AppError::Message)?;
    Ok(LogsOutput {
        contents: store.logs(&cmd.id, cmd.stderr).map_err(AppError::Message)?,
    })
}

async fn inspect(cmd: InspectCommand) -> Result<InspectOutput, AppError> {
    inspect_reference(
        &cmd.store_dir.unwrap_or_else(crate::default_store_dir),
        &cmd.reference,
    )
    .await
}

fn list(cmd: ListCommand) -> Result<ListOutput, AppError> {
    let store_dir = cmd.store_dir.unwrap_or_else(crate::default_store_dir);
    let store = ImageStore::new(&store_dir)?;
    let images = store
        .list_image_refs()?
        .into_iter()
        .map(|(reference, digest)| ImageReferenceOutput {
            reference,
            manifest_digest: digest,
        })
        .collect();
    Ok(ListOutput { images })
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
    let manager = crate::image::ImageManager::new(store_dir)?;
    let image = manager
        .load_image(&reference, &manifest_digest, &manifest.config.digest)
        .await?;
    Ok(InspectOutput {
        reference: image.reference.to_string(),
        manifest_digest,
        config_digest: manifest.config.digest.clone(),
        architecture: image.config.architecture.clone(),
        os: image.config.os.clone(),
        size_bytes: image.size,
        layers: manifest.layers.len(),
        env: image.env(),
        cmd: image.default_cmd().unwrap_or_default(),
        entrypoint: image.entrypoint().unwrap_or_default(),
        working_dir: image.working_dir(),
        user: image.user(),
    })
}

fn parse_key_values(values: &[String], kind: &str) -> Result<HashMap<String, String>, AppError> {
    let mut items = HashMap::new();
    for value in values {
        let Some((key, value)) = value.split_once('=') else {
            return Err(AppError::Message(format!(
                "Invalid {} '{}', expected KEY=VALUE",
                kind, value
            )));
        };
        items.insert(key.to_string(), value.to_string());
    }
    Ok(items)
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

fn apply_registry_auth(
    client: &RegistryClient,
    registry: &str,
    username: Option<&str>,
    password: Option<&str>,
) -> Result<(), AppError> {
    match (username, password) {
        (Some(username), Some(password)) => {
            client.set_registry_credentials(registry, username, password)?;
            Ok(())
        }
        (Some(_), None) | (None, Some(_)) => Err(AppError::Message(
            "Both --username and --password are required together".to_string(),
        )),
        (None, None) => Ok(()),
    }
}

fn format_pull_event(event: PullEvent) -> String {
    match event {
        PullEvent::Started { reference } => format!("pulling {}", reference),
        PullEvent::ResolvingManifest => "resolving manifest".to_string(),
        PullEvent::ManifestResolved { digest, layers } => {
            format!("manifest {} with {} layers", digest, layers)
        }
        PullEvent::DownloadingLayer {
            digest,
            current,
            total,
        } => format!("layer {}/{} {}", current, total, digest),
        PullEvent::LayerDownloaded {
            digest,
            size,
            cached,
        } => format!(
            "downloaded {} ({} bytes{})",
            digest,
            size,
            if cached { ", cached" } else { "" }
        ),
        PullEvent::Complete { digest, size } => format!("complete {} ({} bytes)", digest, size),
        PullEvent::Error { message } => format!("pull error: {}", message),
    }
}

fn format_push_event(event: PushEvent) -> String {
    match event {
        PushEvent::Started { reference } => format!("pushing {}", reference),
        PushEvent::UploadingBlob { digest, size } => {
            format!("uploading {} ({} bytes)", digest, size)
        }
        PushEvent::BlobMounted { digest } => format!("blob already present {}", digest),
        PushEvent::BlobUploaded { digest, size } => {
            format!("uploaded {} ({} bytes)", digest, size)
        }
        PushEvent::ManifestUploaded { digest } => format!("uploaded manifest {}", digest),
        PushEvent::Complete { reference, digest } => {
            format!("push complete {} ({})", reference, digest)
        }
    }
}
