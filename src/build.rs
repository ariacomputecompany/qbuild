use crate::image::{
    Descriptor, ImageManager, ImageReference, ImageStore as BlobImageStore, MediaType, OciImage,
    OciImageConfig, OciManifest, RootFs,
};
use crate::registry::{PullOptions, RegistryClient};
use flate2::Compression;
use flate2::write::GzEncoder;
use glob::glob;
use nix::mount::{MntFlags, MsFlags, mount, umount2};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::CString;
use std::fs::File;
use std::io::{Cursor, Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use tar::{Builder as TarBuilder, EntryType, Header};

#[derive(Debug, Clone)]
pub struct BuildRequest {
    pub context_dir: PathBuf,
    pub dockerfile_path: String,
    pub image_reference: String,
    pub build_args: HashMap<String, String>,
    pub target_stage: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct BuildResult {
    pub image_reference: String,
    pub manifest_digest: String,
    pub config_digest: String,
    pub size_bytes: u64,
}

pub struct BuildService {
    image_store_path: PathBuf,
    build_work_path: PathBuf,
}

struct BuildExecutionContext<'a> {
    stages: &'a [StageSpec],
    prior_outputs: &'a [StageOutput],
    request: &'a BuildRequest,
    context_root: &'a Path,
    work_root: &'a Path,
}

impl BuildService {
    pub fn new(image_store_path: PathBuf, build_work_path: PathBuf) -> Self {
        Self {
            image_store_path,
            build_work_path,
        }
    }

    pub async fn build_image(&self, req: BuildRequest) -> Result<BuildResult, String> {
        if !req.context_dir.is_dir() {
            return Err(format!(
                "Build context '{}' is not a directory",
                req.context_dir.display()
            ));
        }

        let dockerfile_path = normalize_relative_path(&req.dockerfile_path).ok_or_else(|| {
            "dockerfile_path must be a relative path inside the build context".to_string()
        })?;
        let full_dockerfile_path = req.context_dir.join(&dockerfile_path);
        if !full_dockerfile_path.is_file() {
            return Err(format!(
                "Dockerfile '{}' not found in build context",
                dockerfile_path.display()
            ));
        }

        let dockerfile_text = std::fs::read_to_string(&full_dockerfile_path).map_err(|e| {
            format!(
                "Failed to read Dockerfile '{}': {}",
                dockerfile_path.display(),
                e
            )
        })?;
        let instructions = parse_dockerfile(&dockerfile_text)?;
        verify_build_execution_requirements(&instructions)?;
        let stages = group_stages(&instructions)?;
        let target_stage_index = resolve_target_stage_index(&stages, req.target_stage.as_deref())?;

        let work_root = self.build_work_path.join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&work_root)
            .map_err(|e| format!("Failed to create build workspace: {}", e))?;
        let _workspace_guard = BuildWorkspaceGuard::new(work_root.clone());

        let mut stage_outputs = Vec::with_capacity(stages.len());
        for (idx, stage) in stages.iter().enumerate() {
            let context = BuildExecutionContext {
                stages: &stages,
                prior_outputs: &stage_outputs,
                request: &req,
                context_root: &req.context_dir,
                work_root: &work_root,
            };
            let output = self.build_stage(idx, stage, &context).await?;
            stage_outputs.push(output);
            if idx == target_stage_index {
                break;
            }
        }

        let mut final_stage = stage_outputs
            .into_iter()
            .nth(target_stage_index)
            .ok_or_else(|| "Build target stage did not produce output".to_string())?;

        let config_json = serde_json::to_vec(&final_stage.config)
            .map_err(|e| format!("Failed to encode OCI config: {}", e))?;
        let image_store = BlobImageStore::new(&self.image_store_path)
            .map_err(|e| format!("Failed to initialize OCI image store: {}", e))?;
        let config_digest = image_store
            .store_blob(&config_json)
            .map_err(|e| format!("Failed to store image config: {}", e))?;
        final_stage.manifest.config = Descriptor {
            media_type: MediaType::OciImageConfig.to_string(),
            digest: config_digest.clone(),
            size: config_json.len() as u64,
            urls: None,
            annotations: None,
        };
        let manifest_json = serde_json::to_vec(&final_stage.manifest)
            .map_err(|e| format!("Failed to encode OCI manifest: {}", e))?;
        let manifest_digest = image_store
            .store_blob(&manifest_json)
            .map_err(|e| format!("Failed to store image manifest: {}", e))?;

        let reference = ImageReference::parse(&req.image_reference).map_err(|e| {
            format!(
                "Invalid output image reference '{}': {}",
                req.image_reference, e
            )
        })?;
        image_store
            .store_image_ref(&reference, &manifest_digest)
            .map_err(|e| format!("Failed to store image reference metadata: {}", e))?;

        Ok(BuildResult {
            image_reference: req.image_reference,
            manifest_digest,
            config_digest,
            size_bytes: final_stage.total_size,
        })
    }

    async fn build_stage(
        &self,
        idx: usize,
        stage: &StageSpec,
        context: &BuildExecutionContext<'_>,
    ) -> Result<StageOutput, String> {
        let stage_root = context.work_root.join(format!("stage-{}", idx));
        std::fs::create_dir_all(&stage_root)
            .map_err(|e| format!("Failed to create stage workspace: {}", e))?;
        let rootfs = stage_root.join("rootfs");
        std::fs::create_dir_all(&rootfs)
            .map_err(|e| format!("Failed to create stage rootfs: {}", e))?;

        let mut config = crate::image::OciContainerConfig::default();
        let mut config_arch = "amd64".to_string();
        let mut config_os = "linux".to_string();
        let mut layer_entries = Vec::new();
        let mut diff_ids = Vec::new();
        let mut history = Vec::new();

        if stage.base_image != "scratch" {
            let image = self.ensure_image_available(&stage.base_image).await?;
            let image_manager = ImageManager::new(&self.image_store_path)
                .map_err(|e| format!("Failed to initialize image manager: {}", e))?;
            let prepared = image_manager
                .prepare_rootfs(&image, &format!("build-{}-{}", idx, uuid::Uuid::new_v4()))
                .await
                .map_err(|e| format!("Failed to prepare base image rootfs: {}", e))?;
            copy_tree(&prepared, &rootfs)?;
            config_arch = image.config.architecture.clone();
            config_os = image.config.os.clone();
            if let Some(existing) = image.config.config.clone() {
                config = existing;
            }
            diff_ids.extend(image.config.rootfs.diff_ids.clone());
            if let Some(existing_history) = image.config.history.clone() {
                history.extend(existing_history);
            }
            for layer in image.manifest.layers {
                layer_entries.push(BuiltLayer {
                    descriptor: layer,
                    diff_id: String::new(),
                });
            }
        }

        let mut env_map = env_vec_to_map(config.env.clone().unwrap_or_default());
        let mut build_args = collect_stage_build_args(context.stages, context.request, idx);
        let mut current_shell = config
            .shell
            .clone()
            .unwrap_or_else(|| vec!["/bin/sh".to_string(), "-c".to_string()]);
        let mut current_workdir = config
            .working_dir
            .clone()
            .unwrap_or_else(|| "/".to_string());

        for instruction in &stage.instructions {
            match instruction {
                Instruction::Arg { name, default } => {
                    if !build_args.contains_key(name)
                        && let Some(default) = default.clone()
                    {
                        build_args
                            .insert(name.clone(), interpolate(&default, &env_map, &build_args));
                    }
                    history.push(history_entry("ARG", &format!("ARG {}", name), true));
                }
                Instruction::Env(pairs) => {
                    for (key, value) in pairs {
                        let resolved = interpolate(value, &env_map, &build_args);
                        env_map.insert(key.clone(), resolved);
                    }
                    config.env = Some(env_map_to_vec(&env_map));
                    history.push(history_entry("ENV", "ENV", true));
                }
                Instruction::Workdir(path) => {
                    current_workdir = resolve_container_destination(&current_workdir, path);
                    let dir_on_disk = host_path_for_container_path(&rootfs, &current_workdir)?;
                    std::fs::create_dir_all(&dir_on_disk).map_err(|e| {
                        format!("Failed to create WORKDIR '{}': {}", current_workdir, e)
                    })?;
                    config.working_dir = Some(current_workdir.clone());
                    history.push(history_entry(
                        "WORKDIR",
                        &format!("WORKDIR {}", current_workdir),
                        true,
                    ));
                }
                Instruction::Run { command, exec_form } => {
                    let before = snapshot_rootfs(&rootfs, &stage_root, "before-run")?;
                    execute_run(
                        &rootfs,
                        &current_workdir,
                        &env_map,
                        &config.user,
                        &current_shell,
                        command,
                        *exec_form,
                    )?;
                    let built_layer = build_layer_from_diff(
                        &self.image_store_path,
                        &before,
                        &rootfs,
                        &format!("RUN {}", command),
                    )?;
                    diff_ids.push(built_layer.diff_id.clone());
                    history.push(history_entry("RUN", &format!("RUN {}", command), false));
                    layer_entries.push(built_layer);
                }
                Instruction::Copy(copy) => {
                    let before = snapshot_rootfs(&rootfs, &stage_root, "before-copy")?;
                    perform_copy(
                        copy,
                        context.context_root,
                        &rootfs,
                        context.prior_outputs,
                        &current_workdir,
                        &env_map,
                        &build_args,
                    )?;
                    let built_layer =
                        build_layer_from_diff(&self.image_store_path, &before, &rootfs, "COPY")?;
                    diff_ids.push(built_layer.diff_id.clone());
                    history.push(history_entry("COPY", "COPY", false));
                    layer_entries.push(built_layer);
                }
                Instruction::Add(copy) => {
                    let before = snapshot_rootfs(&rootfs, &stage_root, "before-add")?;
                    perform_add(
                        copy,
                        context.context_root,
                        &rootfs,
                        &current_workdir,
                        &env_map,
                        &build_args,
                    )?;
                    let built_layer =
                        build_layer_from_diff(&self.image_store_path, &before, &rootfs, "ADD")?;
                    diff_ids.push(built_layer.diff_id.clone());
                    history.push(history_entry("ADD", "ADD", false));
                    layer_entries.push(built_layer);
                }
                Instruction::Cmd(cmd) => {
                    config.cmd = Some(parse_exec_or_shell_command(cmd, &current_shell)?);
                    history.push(history_entry("CMD", "CMD", true));
                }
                Instruction::Entrypoint(cmd) => {
                    config.entrypoint = Some(parse_exec_or_shell_command(cmd, &current_shell)?);
                    history.push(history_entry("ENTRYPOINT", "ENTRYPOINT", true));
                }
                Instruction::User(user) => {
                    config.user = Some(interpolate(user, &env_map, &build_args));
                    history.push(history_entry("USER", &format!("USER {}", user), true));
                }
                Instruction::Label(pairs) => {
                    let mut labels = config.labels.clone().unwrap_or_default();
                    for (key, value) in pairs {
                        labels.insert(key.clone(), interpolate(value, &env_map, &build_args));
                    }
                    config.labels = Some(labels);
                    history.push(history_entry("LABEL", "LABEL", true));
                }
                Instruction::Expose(ports) => {
                    let mut exposed = config.exposed_ports.clone().unwrap_or_default();
                    for port in ports {
                        exposed.insert(
                            interpolate(port, &env_map, &build_args),
                            crate::image::EmptyObject::default(),
                        );
                    }
                    config.exposed_ports = Some(exposed);
                    history.push(history_entry("EXPOSE", "EXPOSE", true));
                }
                Instruction::Volume(paths) => {
                    let mut volumes = config.volumes.clone().unwrap_or_default();
                    for path in paths {
                        volumes.insert(
                            interpolate(path, &env_map, &build_args),
                            crate::image::EmptyObject::default(),
                        );
                    }
                    config.volumes = Some(volumes);
                    history.push(history_entry("VOLUME", "VOLUME", true));
                }
                Instruction::Shell(shell) => {
                    current_shell = shell.clone();
                    config.shell = Some(shell.clone());
                    history.push(history_entry("SHELL", "SHELL", true));
                }
                Instruction::From { .. } => {}
            }
        }

        config.env = Some(env_map_to_vec(&env_map));
        config.working_dir = Some(current_workdir);
        config.shell = Some(current_shell);

        let config_blob = OciImageConfig {
            architecture: config_arch,
            os: config_os,
            os_version: None,
            config: Some(config),
            rootfs: RootFs {
                fs_type: "layers".to_string(),
                diff_ids,
            },
            history: Some(history),
            created: Some(chrono::Utc::now().to_rfc3339()),
            author: Some("qbuild".to_string()),
        };

        let layers = layer_entries
            .into_iter()
            .filter(|layer| !layer.descriptor.digest.is_empty())
            .collect::<Vec<_>>();
        let total_size = layers
            .iter()
            .map(|layer| layer.descriptor.size)
            .sum::<u64>();
        let manifest = OciManifest {
            schema_version: 2,
            media_type: Some(MediaType::OciManifest.to_string()),
            config: Descriptor {
                media_type: MediaType::OciImageConfig.to_string(),
                digest: String::new(),
                size: 0,
                urls: None,
                annotations: None,
            },
            layers: layers
                .iter()
                .map(|layer| layer.descriptor.clone())
                .collect(),
            annotations: None,
        };

        Ok(StageOutput {
            name: stage.name.clone(),
            rootfs,
            config: config_blob,
            manifest,
            total_size,
        })
    }

    async fn ensure_image_available(&self, image_ref: &str) -> Result<OciImage, String> {
        let reference = ImageReference::parse(image_ref)
            .map_err(|e| format!("Invalid base image reference '{}': {}", image_ref, e))?;
        if let Some(image) = self.load_local_image(&reference).await? {
            return Ok(image);
        }

        let client = RegistryClient::new()
            .map_err(|e| format!("Failed to initialize OCI registry client: {}", e))?;
        let store = BlobImageStore::new(&self.image_store_path)
            .map_err(|e| format!("Failed to initialize OCI image store: {}", e))?;
        client
            .pull(
                &reference,
                &store,
                &PullOptions {
                    force: false,
                    max_concurrent: 4,
                },
                None,
            )
            .await
            .map_err(|e| format!("Failed to pull base image '{}': {}", image_ref, e))
    }

    async fn load_local_image(
        &self,
        reference: &ImageReference,
    ) -> Result<Option<OciImage>, String> {
        let store = BlobImageStore::new(&self.image_store_path)
            .map_err(|e| format!("Failed to initialize OCI image store: {}", e))?;
        let Some(manifest_digest) = store
            .resolve_image_ref(reference)
            .map_err(|e| format!("Failed to resolve cached image reference: {}", e))?
        else {
            return Ok(None);
        };

        let manifest_bytes = match store.get_blob(&manifest_digest) {
            Ok(bytes) => bytes,
            Err(_) => return Ok(None),
        };
        let manifest: OciManifest = match serde_json::from_slice(&manifest_bytes) {
            Ok(manifest) => manifest,
            Err(_) => return Ok(None),
        };
        if store.get_blob(&manifest.config.digest).is_err() {
            return Ok(None);
        }
        for layer in &manifest.layers {
            if store.get_blob(&layer.digest).is_err() {
                return Ok(None);
            }
        }

        let image_manager = ImageManager::new(&self.image_store_path)
            .map_err(|e| format!("Failed to initialize image manager: {}", e))?;
        let image = image_manager
            .load_image(reference, &manifest_digest, &manifest.config.digest)
            .await
            .map_err(|e| format!("Failed to load cached image '{}': {}", reference, e))?;
        Ok(Some(image))
    }
}

#[derive(Debug, Clone)]
struct StageSpec {
    name: Option<String>,
    base_image: String,
    instructions: Vec<Instruction>,
}

#[derive(Debug, Clone)]
struct StageOutput {
    name: Option<String>,
    rootfs: PathBuf,
    config: OciImageConfig,
    manifest: OciManifest,
    total_size: u64,
}

#[derive(Debug, Clone)]
struct BuiltLayer {
    descriptor: Descriptor,
    diff_id: String,
}

#[derive(Debug, Clone)]
enum Instruction {
    From {
        image: String,
        alias: Option<String>,
    },
    Arg {
        name: String,
        default: Option<String>,
    },
    Env(Vec<(String, String)>),
    Workdir(String),
    Run {
        command: String,
        exec_form: bool,
    },
    Copy(CopyInstruction),
    Add(CopyInstruction),
    Cmd(String),
    Entrypoint(String),
    User(String),
    Label(Vec<(String, String)>),
    Expose(Vec<String>),
    Volume(Vec<String>),
    Shell(Vec<String>),
}

#[derive(Debug, Clone)]
struct CopyInstruction {
    from: Option<String>,
    sources: Vec<String>,
    destination: String,
}

fn parse_dockerfile(input: &str) -> Result<Vec<Instruction>, String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for raw in input.lines() {
        let trimmed = raw.trim_end();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(prefix) = trimmed.strip_suffix('\\') {
            current.push_str(prefix);
            current.push(' ');
            continue;
        }
        current.push_str(trimmed);
        lines.push(current.trim().to_string());
        current.clear();
    }
    if !current.trim().is_empty() {
        lines.push(current.trim().to_string());
    }

    let mut instructions = Vec::new();
    for line in lines {
        let (keyword, rest) = line
            .split_once(char::is_whitespace)
            .ok_or_else(|| format!("Invalid Dockerfile instruction '{}'", line))?;
        let key = keyword.to_ascii_uppercase();
        let rest = rest.trim();
        let instruction = match key.as_str() {
            "FROM" => parse_from_instruction(rest)?,
            "ARG" => parse_arg_instruction(rest)?,
            "ENV" => parse_env_instruction(rest)?,
            "WORKDIR" => Instruction::Workdir(rest.to_string()),
            "RUN" => Instruction::Run {
                command: rest.to_string(),
                exec_form: rest.starts_with('['),
            },
            "COPY" => Instruction::Copy(parse_copy_instruction(rest)?),
            "ADD" => Instruction::Add(parse_copy_instruction(rest)?),
            "CMD" => Instruction::Cmd(rest.to_string()),
            "ENTRYPOINT" => Instruction::Entrypoint(rest.to_string()),
            "USER" => Instruction::User(rest.to_string()),
            "LABEL" => Instruction::Label(parse_key_value_pairs(rest)?),
            "EXPOSE" => {
                Instruction::Expose(shlex::split(rest).unwrap_or_else(|| vec![rest.to_string()]))
            }
            "VOLUME" => {
                if rest.starts_with('[') {
                    Instruction::Volume(parse_json_array(rest)?)
                } else {
                    Instruction::Volume(
                        shlex::split(rest).unwrap_or_else(|| vec![rest.to_string()]),
                    )
                }
            }
            "SHELL" => Instruction::Shell(parse_json_array(rest)?),
            unsupported => {
                return Err(format!(
                    "Unsupported Dockerfile instruction '{}'",
                    unsupported
                ));
            }
        };
        instructions.push(instruction);
    }
    Ok(instructions)
}

fn verify_build_execution_requirements(instructions: &[Instruction]) -> Result<(), String> {
    let requires_run = instructions
        .iter()
        .any(|instruction| matches!(instruction, Instruction::Run { .. }));
    if !requires_run {
        return Ok(());
    }

    if !nix::unistd::Uid::effective().is_root() {
        return Err(
            "Dockerfiles with RUN require elevated privileges in qbuild's current execution model. Re-run as root or use a privileged build worker.".to_string(),
        );
    }

    let probe_root =
        std::env::temp_dir().join(format!("qbuild-run-probe-{}", uuid::Uuid::new_v4()));
    let proc_path = probe_root.join("proc");
    let dev_path = probe_root.join("dev");
    std::fs::create_dir_all(&proc_path)
        .and_then(|_| std::fs::create_dir_all(&dev_path))
        .map_err(|e| format!("Failed to initialize RUN privilege probe: {}", e))?;

    let mount_result = mount(
        Some("proc"),
        &proc_path,
        Some("proc"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
        None::<&str>,
    );
    let mount_err = mount_result.err();
    if mount_err.is_none() {
        let _ = umount2(&proc_path, MntFlags::MNT_DETACH);
    }

    let null_probe = dev_path.join("null");
    let mknod_result = ensure_device_node_from_host(&null_probe, Path::new("/dev/null"));

    let _ = std::fs::remove_dir_all(&probe_root);

    if let Some(err) = mount_err {
        return Err(format!(
            "Dockerfiles with RUN require proc-mount capability in qbuild's current execution model: {}",
            err
        ));
    }
    if let Err(err) = mknod_result {
        return Err(format!(
            "Dockerfiles with RUN require device-node creation capability in qbuild's current execution model: {}",
            err
        ));
    }

    Ok(())
}

fn group_stages(instructions: &[Instruction]) -> Result<Vec<StageSpec>, String> {
    let mut stages = Vec::new();
    let mut current: Option<StageSpec> = None;
    for instruction in instructions {
        match instruction {
            Instruction::From { image, alias } => {
                if let Some(existing) = current.take() {
                    stages.push(existing);
                }
                current = Some(StageSpec {
                    name: alias.clone(),
                    base_image: image.clone(),
                    instructions: Vec::new(),
                });
            }
            _ => {
                let stage = current
                    .as_mut()
                    .ok_or_else(|| "Dockerfile must begin with FROM".to_string())?;
                stage.instructions.push(instruction.clone());
            }
        }
    }
    if let Some(existing) = current.take() {
        stages.push(existing);
    }
    if stages.is_empty() {
        return Err("Dockerfile does not define any build stage".to_string());
    }
    Ok(stages)
}

fn resolve_target_stage_index(stages: &[StageSpec], target: Option<&str>) -> Result<usize, String> {
    match target {
        None => Ok(stages.len().saturating_sub(1)),
        Some(target) => stages
            .iter()
            .position(|stage| stage.name.as_deref() == Some(target))
            .ok_or_else(|| format!("Target stage '{}' not found in Dockerfile", target)),
    }
}

fn parse_from_instruction(rest: &str) -> Result<Instruction, String> {
    let tokens =
        shlex::split(rest).ok_or_else(|| format!("Invalid FROM instruction '{}'", rest))?;
    if tokens.is_empty() {
        return Err("FROM requires an image".to_string());
    }
    let image = tokens[0].clone();
    let alias = if tokens.len() >= 3 && tokens[1].eq_ignore_ascii_case("AS") {
        Some(tokens[2].clone())
    } else {
        None
    };
    Ok(Instruction::From { image, alias })
}

fn parse_arg_instruction(rest: &str) -> Result<Instruction, String> {
    let (name, default) = match rest.split_once('=') {
        Some((name, default)) => (name.trim(), Some(default.trim().to_string())),
        None => (rest.trim(), None),
    };
    if name.is_empty() {
        return Err("ARG requires a name".to_string());
    }
    Ok(Instruction::Arg {
        name: name.to_string(),
        default,
    })
}

fn parse_env_instruction(rest: &str) -> Result<Instruction, String> {
    if rest.contains('=') {
        return Ok(Instruction::Env(parse_key_value_pairs(rest)?));
    }
    let tokens = shlex::split(rest).ok_or_else(|| format!("Invalid ENV instruction '{}'", rest))?;
    if tokens.len() < 2 {
        return Err("ENV requires at least a key and value".to_string());
    }
    Ok(Instruction::Env(vec![(
        tokens[0].clone(),
        tokens[1..].join(" "),
    )]))
}

fn parse_copy_instruction(rest: &str) -> Result<CopyInstruction, String> {
    let tokens =
        shlex::split(rest).ok_or_else(|| format!("Invalid COPY/ADD instruction '{}'", rest))?;
    if tokens.len() < 2 {
        return Err("COPY/ADD requires at least one source and one destination".to_string());
    }
    let mut from = None;
    let mut filtered = Vec::new();
    for token in tokens {
        if let Some(value) = token.strip_prefix("--from=") {
            from = Some(value.to_string());
            continue;
        }
        if token.starts_with("--") {
            return Err(format!("Unsupported COPY/ADD option '{}'", token));
        }
        filtered.push(token);
    }
    if filtered.len() < 2 {
        return Err("COPY/ADD requires at least one source and one destination".to_string());
    }
    let destination = filtered.last().cloned().unwrap_or_default();
    let sources = filtered[..filtered.len() - 1].to_vec();
    Ok(CopyInstruction {
        from,
        sources,
        destination,
    })
}

fn parse_key_value_pairs(rest: &str) -> Result<Vec<(String, String)>, String> {
    let tokens =
        shlex::split(rest).ok_or_else(|| format!("Invalid key-value instruction '{}'", rest))?;
    let mut pairs = Vec::new();
    for token in tokens {
        let (key, value) = token
            .split_once('=')
            .ok_or_else(|| format!("Expected key=value token, got '{}'", token))?;
        pairs.push((key.to_string(), value.to_string()));
    }
    Ok(pairs)
}

fn parse_json_array(rest: &str) -> Result<Vec<String>, String> {
    serde_json::from_str(rest).map_err(|e| format!("Invalid JSON array '{}': {}", rest, e))
}

fn collect_stage_build_args(
    stages: &[StageSpec],
    req: &BuildRequest,
    current_stage: usize,
) -> HashMap<String, String> {
    let mut values = req.build_args.clone();
    for stage in stages.iter().take(current_stage + 1) {
        for instruction in &stage.instructions {
            if let Instruction::Arg { name, default } = instruction {
                values
                    .entry(name.clone())
                    .or_insert_with(|| default.clone().unwrap_or_default());
            }
        }
    }
    values
}

fn execute_run(
    rootfs: &Path,
    workdir: &str,
    env_map: &HashMap<String, String>,
    user_spec: &Option<String>,
    shell: &[String],
    command: &str,
    exec_form: bool,
) -> Result<(), String> {
    prepare_build_rootfs(rootfs)?;

    let (program, args) = if exec_form {
        let parsed: Vec<String> = serde_json::from_str(command)
            .map_err(|e| format!("Invalid exec-form RUN instruction '{}': {}", command, e))?;
        let Some(program) = parsed.first() else {
            return Err("RUN exec-form instruction requires a non-empty argv".to_string());
        };
        (program.clone(), parsed[1..].to_vec())
    } else {
        if shell.is_empty() {
            return Err("SHELL for RUN must not be empty".to_string());
        }
        let program = shell[0].clone();
        let mut args = shell[1..].to_vec();
        args.push(wrap_shell_run_command(command));
        (program, args)
    };

    let uid_gid = resolve_user(rootfs, user_spec.as_deref())?;
    let host_workdir = host_path_for_container_path(rootfs, workdir)?;
    std::fs::create_dir_all(&host_workdir).map_err(|e| {
        format!(
            "Failed to create build working directory '{}': {}",
            workdir, e
        )
    })?;

    let mut cmd = Command::new(&program);
    cmd.args(&args);
    cmd.current_dir("/");
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.env_clear();
    for (key, value) in env_map {
        cmd.env(key, value);
    }
    cmd.env(
        "PATH",
        env_map.get("PATH").cloned().unwrap_or_else(|| {
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string()
        }),
    );
    let _apt_guard = BuildAptGuard::new(rootfs)?;
    let _mount_guard = BuildMountGuard::new(rootfs)?;
    let rootfs = rootfs.to_path_buf();
    let host_workdir = host_workdir.clone();
    unsafe {
        cmd.pre_exec(move || {
            std::env::set_current_dir(&host_workdir)?;
            nix::unistd::chroot(&rootfs).map_err(|e| std::io::Error::other(e.to_string()))?;
            nix::unistd::chdir("/").map_err(|e| std::io::Error::other(e.to_string()))?;
            if let Some((uid, gid)) = uid_gid {
                nix::unistd::setgid(nix::unistd::Gid::from_raw(gid))
                    .map_err(|e| std::io::Error::other(e.to_string()))?;
                nix::unistd::setuid(nix::unistd::Uid::from_raw(uid))
                    .map_err(|e| std::io::Error::other(e.to_string()))?;
            }
            Ok(())
        });
    }

    let output = cmd
        .output()
        .map_err(|e| format!("Failed to execute RUN '{}': {}", command, e))?;
    if !output.status.success() {
        return Err(format!(
            "RUN '{}' failed with status {:?}: {}",
            command,
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

fn perform_copy(
    copy: &CopyInstruction,
    context_root: &Path,
    rootfs: &Path,
    prior_outputs: &[StageOutput],
    current_workdir: &str,
    env_map: &HashMap<String, String>,
    build_args: &HashMap<String, String>,
) -> Result<(), String> {
    let destination = resolve_container_destination(
        current_workdir,
        &interpolate(&copy.destination, env_map, build_args),
    );
    let destination_host = host_path_for_container_path(rootfs, &destination)?;

    let (source_root, source_is_stage) = if let Some(from) = copy.from.as_deref() {
        let stage = prior_outputs
            .iter()
            .find(|stage| stage.name.as_deref() == Some(from))
            .or_else(|| {
                from.parse::<usize>()
                    .ok()
                    .and_then(|idx| prior_outputs.get(idx))
            })
            .ok_or_else(|| {
                format!(
                    "COPY --from='{}' references an unknown completed stage",
                    from
                )
            })?;
        (stage.rootfs.clone(), true)
    } else {
        (context_root.to_path_buf(), false)
    };

    let multiple_sources = copy.sources.len() > 1;
    if multiple_sources {
        std::fs::create_dir_all(&destination_host)
            .map_err(|e| format!("Failed to create COPY destination '{}': {}", destination, e))?;
    }

    for source in &copy.sources {
        let resolved = interpolate(source, env_map, build_args);
        let matches = resolve_copy_matches(&source_root, &resolved, source_is_stage)?;
        if matches.is_empty() {
            return Err(format!("COPY source '{}' not found", source));
        }
        for matched in matches {
            let target = compute_copy_destination(&matched, &destination_host, multiple_sources)?;
            copy_tree_or_file(&matched, &target)?;
        }
    }
    Ok(())
}

fn perform_add(
    copy: &CopyInstruction,
    context_root: &Path,
    rootfs: &Path,
    current_workdir: &str,
    env_map: &HashMap<String, String>,
    build_args: &HashMap<String, String>,
) -> Result<(), String> {
    let destination = resolve_container_destination(
        current_workdir,
        &interpolate(&copy.destination, env_map, build_args),
    );
    let destination_host = host_path_for_container_path(rootfs, &destination)?;
    for source in &copy.sources {
        let resolved = interpolate(source, env_map, build_args);
        let matches = resolve_copy_matches(context_root, &resolved, false)?;
        if matches.is_empty() {
            return Err(format!("ADD source '{}' not found", source));
        }
        for matched in matches {
            if is_archive_path(&matched) {
                std::fs::create_dir_all(&destination_host).map_err(|e| {
                    format!("Failed to create ADD destination '{}': {}", destination, e)
                })?;
                extract_archive_to_directory(&matched, &destination_host)?;
            } else {
                let target =
                    compute_copy_destination(&matched, &destination_host, copy.sources.len() > 1)?;
                copy_tree_or_file(&matched, &target)?;
            }
        }
    }
    Ok(())
}

fn resolve_copy_matches(
    base: &Path,
    source: &str,
    stage_root: bool,
) -> Result<Vec<PathBuf>, String> {
    let relative = normalize_relative_path(source)
        .ok_or_else(|| format!("Source path '{}' escapes the build context", source))?;
    let pattern = base.join(&relative).to_string_lossy().to_string();
    if source.contains('*') || source.contains('?') || source.contains('[') {
        let mut matches = Vec::new();
        for entry in
            glob(&pattern).map_err(|e| format!("Invalid COPY pattern '{}': {}", source, e))?
        {
            let matched = entry.map_err(|e| format!("Failed to read COPY match: {}", e))?;
            matches.push(matched);
        }
        return Ok(matches);
    }
    let path = if stage_root {
        host_path_for_container_path(base, &format!("/{}", relative.display()))?
    } else {
        base.join(relative)
    };
    if !path.exists() {
        return Ok(Vec::new());
    }
    Ok(vec![path])
}

fn compute_copy_destination(
    source: &Path,
    destination_host: &Path,
    multi: bool,
) -> Result<PathBuf, String> {
    if multi || destination_host.is_dir() || source.is_dir() {
        let name = source.file_name().ok_or_else(|| {
            format!(
                "Unable to resolve destination name for '{}'",
                source.display()
            )
        })?;
        Ok(destination_host.join(name))
    } else {
        Ok(destination_host.to_path_buf())
    }
}

fn build_layer_from_diff(
    image_store_path: &Path,
    before: &Path,
    after: &Path,
    created_by: &str,
) -> Result<BuiltLayer, String> {
    let diff = compute_diff_entries(before, after)?;
    let mut tar_data = Vec::new();
    {
        let mut builder = TarBuilder::new(&mut tar_data);
        for entry in &diff.added_or_modified {
            append_path_to_tar(&mut builder, after, entry)?;
        }
        for removed in &diff.removed {
            append_whiteout(&mut builder, removed)?;
        }
        builder.finish().map_err(|e| {
            format!(
                "Failed to finalize build layer tar for '{}': {}",
                created_by, e
            )
        })?;
    }

    let uncompressed_digest = format!("sha256:{:x}", Sha256::digest(&tar_data));
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(&tar_data)
        .map_err(|e| format!("Failed to compress build layer: {}", e))?;
    let compressed = encoder
        .finish()
        .map_err(|e| format!("Failed to finish build layer compression: {}", e))?;
    let store = BlobImageStore::new(image_store_path)
        .map_err(|e| format!("Failed to initialize OCI image store: {}", e))?;
    let digest = store
        .store_blob(&compressed)
        .map_err(|e| format!("Failed to store build layer blob: {}", e))?;
    Ok(BuiltLayer {
        descriptor: Descriptor {
            media_type: MediaType::OciLayerGzip.to_string(),
            digest,
            size: compressed.len() as u64,
            urls: None,
            annotations: None,
        },
        diff_id: uncompressed_digest,
    })
}

struct DiffEntries {
    added_or_modified: Vec<PathBuf>,
    removed: Vec<PathBuf>,
}

fn compute_diff_entries(before: &Path, after: &Path) -> Result<DiffEntries, String> {
    let before_index = index_tree(before)?;
    let after_index = index_tree(after)?;
    let before_paths = before_index.keys().cloned().collect::<HashSet<_>>();
    let after_paths = after_index.keys().cloned().collect::<HashSet<_>>();
    let mut changed = Vec::new();
    for path in &after_paths {
        let before_meta = before_index.get(path);
        let after_meta = after_index.get(path).expect("after entry missing");
        if before_meta != Some(after_meta) {
            changed.push(path.clone());
        }
    }
    let mut removed = before_paths
        .difference(&after_paths)
        .cloned()
        .collect::<Vec<_>>();
    removed.sort();
    changed.sort();
    Ok(DiffEntries {
        added_or_modified: changed,
        removed,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FsEntryMeta {
    kind: String,
    mode: u32,
    size: u64,
    digest: Option<String>,
    symlink_target: Option<PathBuf>,
}

fn index_tree(root: &Path) -> Result<BTreeMap<PathBuf, FsEntryMeta>, String> {
    let mut index = BTreeMap::new();
    for entry in walkdir::WalkDir::new(root)
        .sort_by_file_name()
        .into_iter()
        .filter_map(Result::ok)
    {
        let path = entry.path();
        if path == root {
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|e| format!("Failed to normalize tree path '{}': {}", path.display(), e))?
            .to_path_buf();
        if is_ephemeral_build_support_path(&relative) {
            continue;
        }
        let metadata = std::fs::symlink_metadata(path)
            .map_err(|e| format!("Failed to read metadata for '{}': {}", path.display(), e))?;
        let file_type = metadata.file_type();
        let meta =
            if file_type.is_dir() {
                FsEntryMeta {
                    kind: "dir".to_string(),
                    mode: metadata.permissions().mode(),
                    size: 0,
                    digest: None,
                    symlink_target: None,
                }
            } else if file_type.is_symlink() {
                FsEntryMeta {
                    kind: "symlink".to_string(),
                    mode: metadata.permissions().mode(),
                    size: 0,
                    digest: None,
                    symlink_target: Some(std::fs::read_link(path).map_err(|e| {
                        format!("Failed to read symlink '{}': {}", path.display(), e)
                    })?),
                }
            } else if file_type.is_file() {
                FsEntryMeta {
                    kind: "file".to_string(),
                    mode: metadata.permissions().mode(),
                    size: metadata.len(),
                    digest: Some(hash_file(path)?),
                    symlink_target: None,
                }
            } else {
                continue;
            };
        index.insert(relative, meta);
    }
    Ok(index)
}

fn append_path_to_tar(
    builder: &mut TarBuilder<&mut Vec<u8>>,
    root: &Path,
    relative: &Path,
) -> Result<(), String> {
    let full = root.join(relative);
    let metadata = std::fs::symlink_metadata(&full).map_err(|e| {
        format!(
            "Failed to read metadata for layer entry '{}': {}",
            full.display(),
            e
        )
    })?;
    if metadata.is_dir() {
        builder.append_dir(relative, &full).map_err(|e| {
            format!(
                "Failed to archive directory '{}': {}",
                relative.display(),
                e
            )
        })?;
        return Ok(());
    }
    if metadata.file_type().is_symlink() {
        let target = std::fs::read_link(&full).map_err(|e| {
            format!(
                "Failed to read symlink target for layer entry '{}': {}",
                full.display(),
                e
            )
        })?;
        let mut header = Header::new_gnu();
        header.set_entry_type(EntryType::Symlink);
        header.set_mode(metadata.permissions().mode());
        header.set_size(0);
        header.set_cksum();
        builder
            .append_link(&mut header, relative, target)
            .map_err(|e| format!("Failed to archive symlink '{}': {}", relative.display(), e))?;
        return Ok(());
    }
    builder
        .append_path_with_name(&full, relative)
        .map_err(|e| format!("Failed to archive path '{}': {}", relative.display(), e))
}

fn append_whiteout(builder: &mut TarBuilder<&mut Vec<u8>>, relative: &Path) -> Result<(), String> {
    let parent = relative.parent().unwrap_or_else(|| Path::new(""));
    let name = relative
        .file_name()
        .ok_or_else(|| format!("Failed to create whiteout for '{}'", relative.display()))?;
    let whiteout = parent.join(format!(".wh.{}", name.to_string_lossy()));
    let mut header = Header::new_gnu();
    header.set_entry_type(EntryType::Regular);
    header.set_mode(0o000);
    header.set_size(0);
    header.set_cksum();
    builder
        .append_data(&mut header, whiteout, Cursor::new(Vec::<u8>::new()))
        .map_err(|e| {
            format!(
                "Failed to append whiteout for '{}': {}",
                relative.display(),
                e
            )
        })
}

fn snapshot_rootfs(rootfs: &Path, stage_root: &Path, label: &str) -> Result<PathBuf, String> {
    let snapshot = stage_root.join(format!("{}-{}", label, uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&snapshot).map_err(|e| {
        format!(
            "Failed to create build snapshot '{}': {}",
            snapshot.display(),
            e
        )
    })?;
    copy_tree(rootfs, &snapshot)?;
    Ok(snapshot)
}

fn copy_tree(src: &Path, dst: &Path) -> Result<(), String> {
    for entry in walkdir::WalkDir::new(src)
        .into_iter()
        .filter_map(Result::ok)
    {
        let path = entry.path();
        if path == src {
            continue;
        }
        let relative = path
            .strip_prefix(src)
            .map_err(|e| format!("Failed to normalize path '{}': {}", path.display(), e))?;
        if is_ephemeral_build_support_path(relative) {
            continue;
        }
        let target = dst.join(relative);
        let metadata = std::fs::symlink_metadata(path)
            .map_err(|e| format!("Failed to read metadata for '{}': {}", path.display(), e))?;
        if metadata.is_dir() {
            std::fs::create_dir_all(&target)
                .map_err(|e| format!("Failed to create directory '{}': {}", target.display(), e))?;
            std::fs::set_permissions(&target, metadata.permissions()).map_err(|e| {
                format!("Failed to set permissions on '{}': {}", target.display(), e)
            })?;
        } else if metadata.file_type().is_symlink() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    format!("Failed to create directory '{}': {}", parent.display(), e)
                })?;
            }
            let link_target = std::fs::read_link(path)
                .map_err(|e| format!("Failed to read symlink '{}': {}", path.display(), e))?;
            std::os::unix::fs::symlink(&link_target, &target)
                .map_err(|e| format!("Failed to create symlink '{}': {}", target.display(), e))?;
        } else {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    format!("Failed to create directory '{}': {}", parent.display(), e)
                })?;
            }
            std::fs::copy(path, &target).map_err(|e| {
                format!(
                    "Failed to copy '{}' to '{}': {}",
                    path.display(),
                    target.display(),
                    e
                )
            })?;
            std::fs::set_permissions(&target, metadata.permissions()).map_err(|e| {
                format!("Failed to set permissions on '{}': {}", target.display(), e)
            })?;
        }
    }
    Ok(())
}

fn copy_tree_or_file(src: &Path, dst: &Path) -> Result<(), String> {
    if src.is_dir() {
        std::fs::create_dir_all(dst)
            .map_err(|e| format!("Failed to create directory '{}': {}", dst.display(), e))?;
        copy_tree(src, dst)
    } else if std::fs::symlink_metadata(src)
        .map_err(|e| format!("Failed to read metadata for '{}': {}", src.display(), e))?
        .file_type()
        .is_symlink()
    {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create directory '{}': {}", parent.display(), e))?;
        }
        let link_target = std::fs::read_link(src)
            .map_err(|e| format!("Failed to read symlink '{}': {}", src.display(), e))?;
        let _ = std::fs::remove_file(dst);
        std::os::unix::fs::symlink(&link_target, dst)
            .map_err(|e| format!("Failed to create symlink '{}': {}", dst.display(), e))?;
        Ok(())
    } else {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create directory '{}': {}", parent.display(), e))?;
        }
        std::fs::copy(src, dst).map_err(|e| {
            format!(
                "Failed to copy '{}' to '{}': {}",
                src.display(),
                dst.display(),
                e
            )
        })?;
        let metadata = std::fs::symlink_metadata(src)
            .map_err(|e| format!("Failed to read metadata for '{}': {}", src.display(), e))?;
        std::fs::set_permissions(dst, metadata.permissions())
            .map_err(|e| format!("Failed to set permissions on '{}': {}", dst.display(), e))?;
        Ok(())
    }
}

fn resolve_user(rootfs: &Path, user_spec: Option<&str>) -> Result<Option<(u32, u32)>, String> {
    let Some(user_spec) = user_spec.filter(|value| !value.trim().is_empty()) else {
        return Ok(None);
    };
    if let Some((uid, gid)) = parse_uid_gid(user_spec) {
        return Ok(Some((uid, gid)));
    }

    let passwd_path = rootfs.join("etc/passwd");
    let group_path = rootfs.join("etc/group");
    let passwd = std::fs::read_to_string(&passwd_path).unwrap_or_default();
    let group = std::fs::read_to_string(&group_path).unwrap_or_default();
    let (user_part, group_part) = user_spec.split_once(':').unwrap_or((user_spec, user_spec));
    let uid = passwd
        .lines()
        .find_map(|line| {
            let parts = line.split(':').collect::<Vec<_>>();
            if parts.len() >= 4 && parts[0] == user_part {
                parts[2].parse::<u32>().ok()
            } else {
                None
            }
        })
        .ok_or_else(|| format!("Failed to resolve build user '{}' inside image", user_part))?;
    let gid = group
        .lines()
        .find_map(|line| {
            let parts = line.split(':').collect::<Vec<_>>();
            if parts.len() >= 3 && parts[0] == group_part {
                parts[2].parse::<u32>().ok()
            } else {
                None
            }
        })
        .unwrap_or(uid);
    Ok(Some((uid, gid)))
}

fn parse_uid_gid(user_spec: &str) -> Option<(u32, u32)> {
    let mut parts = user_spec.split(':');
    let uid = parts.next()?.parse::<u32>().ok()?;
    let gid = parts
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(uid);
    Some((uid, gid))
}

fn host_path_for_container_path(rootfs: &Path, container_path: &str) -> Result<PathBuf, String> {
    let normalized = normalize_relative_path(container_path.trim_start_matches('/'))
        .ok_or_else(|| format!("Invalid container path '{}'", container_path))?;
    Ok(rootfs.join(normalized))
}

fn resolve_container_destination(current_workdir: &str, destination: &str) -> String {
    if destination.starts_with('/') {
        clean_container_path(destination)
    } else {
        clean_container_path(&format!(
            "{}/{}",
            current_workdir.trim_end_matches('/'),
            destination
        ))
    }
}

fn clean_container_path(path: &str) -> String {
    let mut result = PathBuf::from("/");
    for component in Path::new(path).components() {
        match component {
            std::path::Component::RootDir => {}
            std::path::Component::Normal(part) => result.push(part),
            _ => {}
        }
    }
    if result.as_os_str().is_empty() {
        "/".to_string()
    } else {
        result.to_string_lossy().to_string()
    }
}

fn normalize_relative_path(path: &str) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            std::path::Component::Normal(part) => out.push(part),
            std::path::Component::CurDir => {}
            _ => return None,
        }
    }
    Some(out)
}

fn env_vec_to_map(values: Vec<String>) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for value in values {
        if let Some((key, value)) = value.split_once('=') {
            map.insert(key.to_string(), value.to_string());
        }
    }
    map
}

fn env_map_to_vec(values: &HashMap<String, String>) -> Vec<String> {
    let mut items = values
        .iter()
        .map(|(key, value)| format!("{}={}", key, value))
        .collect::<Vec<_>>();
    items.sort();
    items
}

fn interpolate(
    input: &str,
    env_map: &HashMap<String, String>,
    build_args: &HashMap<String, String>,
) -> String {
    let mut out = String::new();
    let chars = input.chars().collect::<Vec<_>>();
    let mut idx = 0;
    while idx < chars.len() {
        if chars[idx] == '$' {
            if idx + 1 < chars.len() && chars[idx + 1] == '{' {
                let mut end = idx + 2;
                while end < chars.len() && chars[end] != '}' {
                    end += 1;
                }
                if end < chars.len() {
                    let key = chars[idx + 2..end].iter().collect::<String>();
                    out.push_str(
                        build_args
                            .get(&key)
                            .or_else(|| env_map.get(&key))
                            .map(String::as_str)
                            .unwrap_or(""),
                    );
                    idx = end + 1;
                    continue;
                }
            } else {
                let mut end = idx + 1;
                while end < chars.len() && (chars[end].is_ascii_alphanumeric() || chars[end] == '_')
                {
                    end += 1;
                }
                if end > idx + 1 {
                    let key = chars[idx + 1..end].iter().collect::<String>();
                    out.push_str(
                        build_args
                            .get(&key)
                            .or_else(|| env_map.get(&key))
                            .map(String::as_str)
                            .unwrap_or(""),
                    );
                    idx = end;
                    continue;
                }
            }
        }
        out.push(chars[idx]);
        idx += 1;
    }
    out
}

fn parse_exec_or_shell_command(value: &str, shell: &[String]) -> Result<Vec<String>, String> {
    if value.starts_with('[') {
        return serde_json::from_str(value)
            .map_err(|e| format!("Invalid exec-form command '{}': {}", value, e));
    }
    if shell.is_empty() {
        return Err("SHELL for command must not be empty".to_string());
    }
    let mut command = shell.to_vec();
    command.push(value.to_string());
    Ok(command)
}

fn history_entry(_keyword: &str, created_by: &str, empty_layer: bool) -> crate::image::History {
    crate::image::History {
        created: Some(chrono::Utc::now().to_rfc3339()),
        author: Some("qbuild".to_string()),
        created_by: Some(created_by.to_string()),
        comment: None,
        empty_layer: Some(empty_layer),
    }
}

fn is_archive_path(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    name.ends_with(".tar") || name.ends_with(".tar.gz") || name.ends_with(".tgz")
}

fn is_ephemeral_build_support_path(relative: &Path) -> bool {
    matches!(
        relative.to_string_lossy().as_ref(),
        "proc"
            | "dev/null"
            | "dev/zero"
            | "dev/random"
            | "dev/urandom"
            | "dev/tty"
            | "dev/fd"
            | "dev/stdin"
            | "dev/stdout"
            | "dev/stderr"
    )
}

fn extract_archive_to_directory(archive_path: &Path, destination: &Path) -> Result<(), String> {
    let file = File::open(archive_path)
        .map_err(|e| format!("Failed to open archive '{}': {}", archive_path.display(), e))?;
    let mut bytes = Vec::new();
    let mut file_reader = file;
    file_reader
        .read_to_end(&mut bytes)
        .map_err(|e| format!("Failed to read archive '{}': {}", archive_path.display(), e))?;
    let is_gzip = archive_path
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.eq_ignore_ascii_case("gz") || value.eq_ignore_ascii_case("tgz"))
        .unwrap_or(false);
    if is_gzip {
        let decoder = flate2::read::GzDecoder::new(bytes.as_slice());
        let mut archive = tar::Archive::new(decoder);
        archive.unpack(destination).map_err(|e| {
            format!(
                "Failed to unpack archive '{}': {}",
                archive_path.display(),
                e
            )
        })?;
    } else {
        let mut archive = tar::Archive::new(bytes.as_slice());
        archive.unpack(destination).map_err(|e| {
            format!(
                "Failed to unpack archive '{}': {}",
                archive_path.display(),
                e
            )
        })?;
    }
    Ok(())
}

fn hash_file(path: &Path) -> Result<String, String> {
    let mut file = File::open(path)
        .map_err(|e| format!("Failed to open '{}' for hashing: {}", path.display(), e))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8192];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|e| format!("Failed to read '{}' for hashing: {}", path.display(), e))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn prepare_build_rootfs(rootfs: &Path) -> Result<(), String> {
    std::fs::create_dir_all(rootfs.join("dev"))
        .map_err(|e| format!("Failed to create build rootfs /dev: {}", e))?;
    std::fs::create_dir_all(rootfs.join("proc"))
        .map_err(|e| format!("Failed to create build rootfs /proc: {}", e))?;

    for name in ["null", "zero", "random", "urandom", "tty"] {
        ensure_device_node_from_host(
            &rootfs.join("dev").join(name),
            &Path::new("/dev").join(name),
        )?;
    }
    ensure_symlink(rootfs.join("dev/fd"), Path::new("/proc/self/fd"))?;
    ensure_symlink(rootfs.join("dev/stdin"), Path::new("/proc/self/fd/0"))?;
    ensure_symlink(rootfs.join("dev/stdout"), Path::new("/proc/self/fd/1"))?;
    ensure_symlink(rootfs.join("dev/stderr"), Path::new("/proc/self/fd/2"))?;
    Ok(())
}

fn ensure_symlink(path: PathBuf, target: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() => return Ok(()),
        Ok(metadata) if metadata.is_dir() => {
            std::fs::remove_dir_all(&path)
                .map_err(|e| format!("Failed to replace directory '{}': {}", path.display(), e))?;
        }
        Ok(_) => {
            std::fs::remove_file(&path)
                .map_err(|e| format!("Failed to replace file '{}': {}", path.display(), e))?;
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(format!(
                "Failed to inspect symlink path '{}': {}",
                path.display(),
                err
            ));
        }
    }
    std::os::unix::fs::symlink(target, &path)
        .map_err(|e| format!("Failed to create symlink '{}': {}", path.display(), e))
}

fn ensure_device_node_from_host(target: &Path, source: &Path) -> Result<(), String> {
    if target.exists() {
        return Ok(());
    }
    let metadata = std::fs::symlink_metadata(source).map_err(|e| {
        format!(
            "Failed to read device metadata for '{}': {}",
            source.display(),
            e
        )
    })?;
    let c_path = CString::new(target.as_os_str().as_encoded_bytes())
        .map_err(|_| format!("Invalid device node path '{}'", target.display()))?;
    let mode = metadata.mode() as libc::mode_t;
    let dev = metadata.rdev() as libc::dev_t;
    let result = unsafe { libc::mknod(c_path.as_ptr(), mode, dev) };
    if result != 0 {
        return Err(format!(
            "Failed to create device node '{}': {}",
            target.display(),
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

struct BuildMountGuard {
    proc_path: PathBuf,
    proc_mounted: bool,
}

struct BuildAptGuard {
    injected_path: Option<PathBuf>,
}

impl BuildAptGuard {
    fn new(rootfs: &Path) -> Result<Self, String> {
        if !rootfs.join("usr/bin/apt-get").exists()
            && !rootfs.join("bin/apt-get").exists()
            && !rootfs.join("usr/bin/apt").exists()
            && !rootfs.join("bin/apt").exists()
        {
            return Ok(Self {
                injected_path: None,
            });
        }

        let apt_conf_dir = rootfs.join("etc/apt/apt.conf.d");
        if !apt_conf_dir.exists() {
            return Ok(Self {
                injected_path: None,
            });
        }

        let injected_path = apt_conf_dir.join("99qbuild-clock-skew.conf");
        if injected_path.exists() {
            return Ok(Self {
                injected_path: None,
            });
        }

        std::fs::write(
            &injected_path,
            "Acquire::Check-Valid-Until \"false\";\nAcquire::Check-Date \"false\";\n",
        )
        .map_err(|e| {
            format!(
                "Failed to write temporary apt clock-skew override '{}': {}",
                injected_path.display(),
                e
            )
        })?;

        Ok(Self {
            injected_path: Some(injected_path),
        })
    }
}

impl Drop for BuildAptGuard {
    fn drop(&mut self) {
        if let Some(path) = &self.injected_path {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl BuildMountGuard {
    fn new(rootfs: &Path) -> Result<Self, String> {
        let proc_path = rootfs.join("proc");
        let proc_mounted = match mount(
            Some("proc"),
            &proc_path,
            Some("proc"),
            MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
            None::<&str>,
        ) {
            Ok(()) => true,
            Err(nix::Error::EINVAL | nix::Error::EPERM | nix::Error::EBUSY) => false,
            Err(err) => {
                return Err(format!(
                    "Failed to mount build /proc at '{}': {}",
                    proc_path.display(),
                    err
                ));
            }
        };
        Ok(Self {
            proc_path,
            proc_mounted,
        })
    }
}

impl Drop for BuildMountGuard {
    fn drop(&mut self) {
        if self.proc_mounted {
            let _ = umount2(&self.proc_path, MntFlags::MNT_DETACH);
        }
    }
}

struct BuildWorkspaceGuard {
    path: PathBuf,
}

impl BuildWorkspaceGuard {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for BuildWorkspaceGuard {
    fn drop(&mut self) {
        if self.path.exists() {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

fn wrap_shell_run_command(command: &str) -> String {
    if !needs_apt_update_retry(command) {
        return command.to_string();
    }

    format!(
        "attempt=1; while true; do {}; status=$?; if [ $status -eq 0 ]; then break; fi; if [ $attempt -ge 5 ]; then exit $status; fi; sleep 5; attempt=$((attempt+1)); done",
        command
    )
}

fn needs_apt_update_retry(command: &str) -> bool {
    command.contains("apt-get update") || command.contains("apt update")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn parses_multistage_dockerfile() {
        let dockerfile = r#"
            FROM alpine:3.20 AS base
            ARG APP_ENV=prod
            ENV APP_ENV=$APP_ENV
            RUN echo hi

            FROM base AS final
            COPY --from=base /etc /tmp/etc
            CMD ["sh"]
        "#;

        let instructions = parse_dockerfile(dockerfile).unwrap();
        let stages = group_stages(&instructions).unwrap();
        assert_eq!(stages.len(), 2);
        assert_eq!(stages[0].name.as_deref(), Some("base"));
        assert_eq!(stages[1].name.as_deref(), Some("final"));
    }

    #[test]
    fn interpolates_env_and_build_args() {
        let env = HashMap::from([("HOME".to_string(), "/root".to_string())]);
        let args = HashMap::from([("APP".to_string(), "qbuild".to_string())]);
        assert_eq!(
            interpolate("$HOME/${APP}/bin", &env, &args),
            "/root/qbuild/bin"
        );
    }

    #[test]
    fn appends_broken_absolute_symlink_to_tar() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("usr/bin")).unwrap();
        std::os::unix::fs::symlink("/usr/local/bin/example", root.join("usr/bin/example")).unwrap();

        let mut tar_data = Vec::new();
        {
            let mut builder = TarBuilder::new(&mut tar_data);
            append_path_to_tar(&mut builder, root, Path::new("usr/bin/example")).unwrap();
            builder.finish().unwrap();
        }

        let mut archive = tar::Archive::new(tar_data.as_slice());
        let mut entries = archive.entries().unwrap();
        let entry = entries.next().unwrap().unwrap();
        assert_eq!(entry.header().entry_type(), EntryType::Symlink);
        assert_eq!(
            entry.link_name().unwrap().unwrap(),
            Path::new("/usr/local/bin/example")
        );
    }
}
