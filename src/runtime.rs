use crate::image::{ImageManager, ImageReference, OciImage};
use crate::protocol::{BindMountSpec, NamespaceConfig, ResourceLimits};
use nix::mount::{MsFlags, mount};
use nix::sched::CloneFlags;
use nix::unistd::{Gid, Pid, Uid, chdir, chroot, setpgid};
use serde::Serialize;
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitStatus};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RunRequest {
    pub image_reference: String,
    pub command: Vec<String>,
    pub environment: HashMap<String, String>,
    pub working_directory: Option<String>,
    pub store_dir: PathBuf,
    pub namespace_config: NamespaceConfig,
    pub resource_limits: Option<ResourceLimits>,
    pub clear_image_env: bool,
    pub mounts: Vec<BindMountSpec>,
    pub container_id: Option<String>,
    pub status_file: Option<PathBuf>,
    pub started_at: Option<i64>,
}

pub struct RunResult {
    pub exit_status: ExitStatus,
}

pub struct RunService;

impl RunService {
    pub fn new() -> Self {
        Self
    }

    pub async fn run(&self, request: RunRequest) -> Result<RunResult, String> {
        ensure_runtime_prerequisites()?;

        let image_manager = ImageManager::new(&request.store_dir)
            .map_err(|e| format!("Failed to initialize image manager: {}", e))?;
        let reference = ImageReference::parse(&request.image_reference).map_err(|e| {
            format!(
                "Invalid image reference '{}': {}",
                request.image_reference, e
            )
        })?;
        let store = crate::image::ImageStore::new(&request.store_dir)
            .map_err(|e| format!("Failed to initialize image store: {}", e))?;
        let manifest_digest = store
            .resolve_image_ref(&reference)
            .map_err(|e| format!("Failed to resolve image reference: {}", e))?
            .ok_or_else(|| format!("Image '{}' not found locally", request.image_reference))?;
        let manifest_bytes = store
            .get_blob(&manifest_digest)
            .map_err(|e| format!("Failed to load manifest blob: {}", e))?;
        let manifest: crate::image::OciManifest = serde_json::from_slice(&manifest_bytes)
            .map_err(|e| format!("Failed to decode manifest: {}", e))?;
        let image = image_manager
            .load_image(&reference, &manifest_digest, &manifest.config.digest)
            .await
            .map_err(|e| format!("Failed to load image '{}': {}", request.image_reference, e))?;

        let container_id = request
            .container_id
            .clone()
            .unwrap_or_else(|| format!("run-{}", uuid::Uuid::new_v4()));
        let rootfs = image_manager
            .prepare_rootfs(&image, &container_id)
            .await
            .map_err(|e| format!("Failed to prepare rootfs: {}", e))?;

        let cgroups = request
            .resource_limits
            .as_ref()
            .map(|limits| CgroupManager::new(container_id.clone(), limits.clone()));
        if let Some(manager) = &cgroups {
            manager.create()?;
        }

        let result = self.run_with_rootfs(&rootfs, &image, &request, cgroups.as_ref());

        let cleanup_result = image_manager.remove_rootfs(&container_id);
        if let Some(manager) = &cgroups {
            let _ = manager.destroy();
        }
        if let Err(err) = cleanup_result {
            eprintln!(
                "warning: failed to cleanup rootfs {}: {}",
                rootfs.display(),
                err
            );
        }

        result
    }

    fn run_with_rootfs(
        &self,
        rootfs: &Path,
        image: &OciImage,
        request: &RunRequest,
        cgroups: Option<&CgroupManager>,
    ) -> Result<RunResult, String> {
        let command = resolve_runtime_command(image, &request.command, rootfs)?;
        let env = resolve_runtime_env(image, &request.environment, request.clear_image_env);
        let workdir = request
            .working_directory
            .clone()
            .or_else(|| image.working_dir())
            .unwrap_or_else(|| "/".to_string());
        let user = image.user();
        let namespace_flags = build_clone_flags(&request.namespace_config);
        let mount_namespace = request.namespace_config.mount;
        if !request.mounts.is_empty() && !mount_namespace {
            return Err(
                "Bind mounts require qbuild's mount namespace; remove --no-mount-namespace"
                    .to_string(),
            );
        }
        let mounts = request.mounts.clone();

        let rootfs = rootfs.to_path_buf();
        let workdir_clone = workdir.clone();

        let mut child = Command::new(&command[0]);
        child.args(&command[1..]);
        child.stdin(std::process::Stdio::inherit());
        child.stdout(std::process::Stdio::inherit());
        child.stderr(std::process::Stdio::inherit());
        child.env_clear();
        for (key, value) in &env {
            child.env(key, value);
        }

        unsafe {
            child.pre_exec(move || {
                if !namespace_flags.is_empty() {
                    nix::sched::unshare(namespace_flags)
                        .map_err(|e| std::io::Error::other(e.to_string()))?;
                }

                setpgid(Pid::from_raw(0), Pid::from_raw(0))
                    .map_err(|e| std::io::Error::other(e.to_string()))?;

                prepare_runtime_rootfs(&rootfs).map_err(std::io::Error::other)?;
                apply_bind_mounts(&rootfs, &mounts).map_err(std::io::Error::other)?;
                chroot(&rootfs).map_err(|e| std::io::Error::other(e.to_string()))?;
                chdir("/").map_err(|e| std::io::Error::other(e.to_string()))?;

                if mount_namespace {
                    mount_proc_inside_rootfs().map_err(std::io::Error::other)?;
                }

                chdir(workdir_clone.as_str()).map_err(|e| std::io::Error::other(e.to_string()))?;

                if let Some(user_spec) = user.as_deref()
                    && let Some((uid, gid)) =
                        resolve_user(&rootfs, user_spec).map_err(std::io::Error::other)?
                {
                    nix::unistd::setgid(Gid::from_raw(gid))
                        .map_err(|e| std::io::Error::other(e.to_string()))?;
                    nix::unistd::setuid(Uid::from_raw(uid))
                        .map_err(|e| std::io::Error::other(e.to_string()))?;
                }
                Ok(())
            });
        }

        let mut child = child
            .spawn()
            .map_err(|e| format!("Failed to spawn container process: {}", e))?;

        if let Some(manager) = cgroups {
            manager.add_process(child.id() as i32)?;
        }

        if let Some(status_file) = &request.status_file {
            write_runtime_status(status_file, child.id(), request.started_at)?;
        }

        let status = child
            .wait()
            .map_err(|e| format!("Failed to wait for container process: {}", e))?;
        Ok(RunResult {
            exit_status: status,
        })
    }
}

#[derive(Serialize)]
struct RuntimeStatus<'a> {
    id: &'a str,
    state: &'a str,
    pid: u32,
    exit_code: Option<i32>,
    started_at: Option<i64>,
    finished_at: Option<i64>,
}

fn write_runtime_status(path: &Path, pid: u32, started_at: Option<i64>) -> Result<(), String> {
    let id = path
        .parent()
        .and_then(|parent| parent.file_name())
        .and_then(|value| value.to_str())
        .ok_or_else(|| format!("Invalid runtime status path '{}'", path.display()))?;
    let payload = RuntimeStatus {
        id,
        state: "running",
        pid,
        exit_code: None,
        started_at,
        finished_at: None,
    };
    let bytes = serde_json::to_vec_pretty(&payload).map_err(|e| {
        format!(
            "Failed to encode runtime status '{}': {}",
            path.display(),
            e
        )
    })?;
    std::fs::write(path, bytes)
        .map_err(|e| format!("Failed to write runtime status '{}': {}", path.display(), e))
}

fn ensure_runtime_prerequisites() -> Result<(), String> {
    if !Uid::effective().is_root() {
        return Err(
            "Standalone container run currently requires root privileges in qbuild's runtime model"
                .to_string(),
        );
    }
    Ok(())
}

fn resolve_runtime_command(
    image: &OciImage,
    override_command: &[String],
    rootfs: &Path,
) -> Result<Vec<String>, String> {
    if !override_command.is_empty() {
        return Ok(override_command.to_vec());
    }

    let entrypoint = image.entrypoint().unwrap_or_default();
    let cmd = image.default_cmd().unwrap_or_default();
    if !entrypoint.is_empty() {
        let mut full = entrypoint;
        full.extend(cmd);
        return Ok(full);
    }
    if !cmd.is_empty() {
        return Ok(cmd);
    }

    if rootfs.join("bin/sh").exists() || rootfs.join("usr/bin/sh").exists() {
        return Ok(vec!["/bin/sh".to_string()]);
    }

    Err("Image does not define Entrypoint/Cmd and /bin/sh is unavailable".to_string())
}

fn resolve_runtime_env(
    image: &OciImage,
    overrides: &HashMap<String, String>,
    clear_image_env: bool,
) -> HashMap<String, String> {
    let mut env = HashMap::new();
    if !clear_image_env {
        for entry in image.env() {
            if let Some((key, value)) = entry.split_once('=') {
                env.insert(key.to_string(), value.to_string());
            }
        }
    }
    for (key, value) in overrides {
        env.insert(key.clone(), value.clone());
    }
    env.entry("PATH".to_string()).or_insert_with(|| {
        "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string()
    });
    env
}

fn build_clone_flags(config: &NamespaceConfig) -> CloneFlags {
    let mut flags = CloneFlags::empty();
    if config.mount {
        flags |= CloneFlags::CLONE_NEWNS;
    }
    if config.uts {
        flags |= CloneFlags::CLONE_NEWUTS;
    }
    if config.ipc {
        flags |= CloneFlags::CLONE_NEWIPC;
    }
    if config.network {
        flags |= CloneFlags::CLONE_NEWNET;
    }
    flags
}

fn prepare_runtime_rootfs(rootfs: &Path) -> Result<(), String> {
    std::fs::create_dir_all(rootfs.join("dev"))
        .map_err(|e| format!("Failed to create /dev in runtime rootfs: {}", e))?;
    std::fs::create_dir_all(rootfs.join("proc"))
        .map_err(|e| format!("Failed to create /proc in runtime rootfs: {}", e))?;

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

fn mount_proc_inside_rootfs() -> Result<(), String> {
    let target = Path::new("/proc");
    mount(
        Some("proc"),
        target,
        Some("proc"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
        None::<&str>,
    )
    .map_err(|e| format!("Failed to mount /proc in runtime: {}", e))
}

fn apply_bind_mounts(rootfs: &Path, mounts: &[BindMountSpec]) -> Result<(), String> {
    for spec in mounts {
        let source = std::fs::canonicalize(&spec.source).map_err(|e| {
            format!(
                "Failed to resolve bind mount source '{}': {}",
                spec.source, e
            )
        })?;
        let metadata = std::fs::metadata(&source).map_err(|e| {
            format!(
                "Failed to read bind mount source '{}': {}",
                source.display(),
                e
            )
        })?;
        let target = normalize_container_mount_target(&spec.target)?;
        let host_target = rootfs.join(
            target
                .strip_prefix("/")
                .map_err(|_| format!("Bind mount target '{}' must be absolute", spec.target))?,
        );

        if metadata.is_dir() {
            std::fs::create_dir_all(&host_target).map_err(|e| {
                format!(
                    "Failed to create bind mount target '{}': {}",
                    host_target.display(),
                    e
                )
            })?;
        } else if metadata.is_file() {
            if let Some(parent) = host_target.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    format!(
                        "Failed to create bind mount target parent '{}': {}",
                        parent.display(),
                        e
                    )
                })?;
            }
            OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(false)
                .open(&host_target)
                .map_err(|e| {
                    format!(
                        "Failed to create bind mount target '{}': {}",
                        host_target.display(),
                        e
                    )
                })?;
        } else {
            return Err(format!(
                "Bind mount source '{}' must be a regular file or directory",
                source.display()
            ));
        }

        mount(
            Some(source.as_path()),
            host_target.as_path(),
            None::<&str>,
            MsFlags::MS_BIND | MsFlags::MS_REC,
            None::<&str>,
        )
        .map_err(|e| {
            format!(
                "Failed to bind mount '{}' to '{}': {}",
                source.display(),
                target.display(),
                e
            )
        })?;

        if spec.readonly {
            mount(
                Some(source.as_path()),
                host_target.as_path(),
                None::<&str>,
                MsFlags::MS_BIND | MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY | MsFlags::MS_REC,
                None::<&str>,
            )
            .map_err(|e| format!("Failed to remount '{}' readonly: {}", target.display(), e))?;
        }
    }
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
    let c_path = std::ffi::CString::new(target.as_os_str().as_encoded_bytes())
        .map_err(|_| format!("Invalid device node path '{}'", target.display()))?;
    let mode = std::os::unix::fs::MetadataExt::mode(&metadata) as libc::mode_t;
    let dev = std::os::unix::fs::MetadataExt::rdev(&metadata) as libc::dev_t;
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

fn resolve_user(rootfs: &Path, user_spec: &str) -> Result<Option<(u32, u32)>, String> {
    if user_spec.trim().is_empty() {
        return Ok(None);
    }
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
        .ok_or_else(|| {
            format!(
                "Failed to resolve runtime user '{}' inside image",
                user_part
            )
        })?;
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

fn normalize_container_mount_target(target: &str) -> Result<PathBuf, String> {
    let raw = Path::new(target);
    if !raw.is_absolute() {
        return Err(format!("Bind mount target '{}' must be absolute", target));
    }
    let mut normalized = PathBuf::from("/");
    for component in raw.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(part) => normalized.push(part),
            Component::ParentDir => {
                return Err(format!(
                    "Bind mount target '{}' cannot contain parent traversal",
                    target
                ));
            }
            Component::Prefix(_) => {
                return Err(format!(
                    "Bind mount target '{}' is not a Linux path",
                    target
                ));
            }
        }
    }
    if normalized == Path::new("/") {
        return Err("Bind mount target cannot be '/'".to_string());
    }
    Ok(normalized)
}

struct CgroupManager {
    id: String,
    limits: ResourceLimits,
    root: PathBuf,
}

impl CgroupManager {
    fn new(id: String, limits: ResourceLimits) -> Self {
        Self {
            id,
            limits,
            root: PathBuf::from("/sys/fs/cgroup"),
        }
    }

    fn create(&self) -> Result<(), String> {
        if self.root.join("cgroup.controllers").exists() {
            self.create_v2()
        } else {
            Ok(())
        }
    }

    fn add_process(&self, pid: i32) -> Result<(), String> {
        if self.root.join("cgroup.controllers").exists() {
            let path = self.root.join("qbuild").join(&self.id).join("cgroup.procs");
            std::fs::write(&path, pid.to_string())
                .map_err(|e| format!("Failed to add process {} to cgroup: {}", pid, e))
        } else {
            Ok(())
        }
    }

    fn destroy(&self) -> Result<(), String> {
        if self.root.join("cgroup.controllers").exists() {
            let dir = self.root.join("qbuild").join(&self.id);
            let _ = std::fs::write(dir.join("cgroup.kill"), "1");
            std::fs::remove_dir(&dir)
                .map_err(|e| format!("Failed to remove cgroup '{}': {}", dir.display(), e))
        } else {
            Ok(())
        }
    }

    fn create_v2(&self) -> Result<(), String> {
        let parent = self.root.join("qbuild");
        std::fs::create_dir_all(&parent).map_err(|e| {
            format!(
                "Failed to create cgroup parent '{}': {}",
                parent.display(),
                e
            )
        })?;
        let _ = std::fs::write(parent.join("cgroup.subtree_control"), "+memory +cpu +pids");
        let dir = parent.join(&self.id);
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("Failed to create cgroup '{}': {}", dir.display(), e))?;
        if let Some(memory) = self.limits.memory_limit_bytes {
            let _ = std::fs::write(dir.join("memory.max"), memory.to_string());
        }
        if let Some(cpu_quota) = self.limits.cpu_quota {
            let cpu_period = self.limits.cpu_period.unwrap_or(100000);
            let _ = std::fs::write(dir.join("cpu.max"), format!("{} {}", cpu_quota, cpu_period));
        }
        if let Some(pids) = self.limits.pids_limit {
            let _ = std::fs::write(dir.join("pids.max"), pids.to_string());
        }
        Ok(())
    }
}
