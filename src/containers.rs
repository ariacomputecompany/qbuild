use crate::protocol::{ContainerState, NamespaceConfig, ResourceLimits};
use crate::runtime::{RunRequest, RunResult, RunService};
use chrono::Utc;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerRecord {
    pub id: String,
    pub image_reference: String,
    pub command: Vec<String>,
    pub environment: HashMap<String, String>,
    pub working_directory: Option<String>,
    pub namespace_config: NamespaceConfig,
    pub resource_limits: Option<ResourceLimits>,
    pub clear_image_env: bool,
    pub state: ContainerState,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContainerSummary {
    pub id: String,
    pub image_reference: String,
    pub state: ContainerState,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct CreateContainerRequest {
    pub image_reference: String,
    pub command: Vec<String>,
    pub environment: HashMap<String, String>,
    pub working_directory: Option<String>,
    pub namespace_config: NamespaceConfig,
    pub resource_limits: Option<ResourceLimits>,
    pub clear_image_env: bool,
}

pub struct ContainerStore {
    root: PathBuf,
    store_dir: PathBuf,
}

impl ContainerStore {
    pub fn new(data_root: impl AsRef<Path>, store_dir: impl AsRef<Path>) -> Result<Self, String> {
        let root = data_root.as_ref().join("containers");
        std::fs::create_dir_all(&root).map_err(|e| {
            format!(
                "Failed to create container store '{}': {}",
                root.display(),
                e
            )
        })?;
        Ok(Self {
            root,
            store_dir: store_dir.as_ref().to_path_buf(),
        })
    }

    pub fn create(&self, request: CreateContainerRequest) -> Result<ContainerRecord, String> {
        let id = format!("ctr-{}", uuid::Uuid::new_v4().simple());
        let record = ContainerRecord {
            id: id.clone(),
            image_reference: request.image_reference,
            command: request.command,
            environment: request.environment,
            working_directory: request.working_directory,
            namespace_config: request.namespace_config,
            resource_limits: request.resource_limits,
            clear_image_env: request.clear_image_env,
            state: ContainerState::Created,
            pid: None,
            exit_code: None,
            created_at: Utc::now().timestamp(),
            started_at: None,
            finished_at: None,
        };
        let dir = self.container_dir(&id);
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("Failed to create container dir '{}': {}", dir.display(), e))?;
        self.write_record(&record)?;
        Ok(record)
    }

    pub fn load(&self, id: &str) -> Result<ContainerRecord, String> {
        let path = self.record_path(id);
        let bytes = std::fs::read(&path).map_err(|e| {
            format!(
                "Failed to read container record '{}': {}",
                path.display(),
                e
            )
        })?;
        serde_json::from_slice(&bytes).map_err(|e| {
            format!(
                "Failed to decode container record '{}': {}",
                path.display(),
                e
            )
        })
    }

    pub fn list(&self) -> Result<Vec<ContainerSummary>, String> {
        let mut items = Vec::new();
        for entry in std::fs::read_dir(&self.root).map_err(|e| {
            format!(
                "Failed to list container store '{}': {}",
                self.root.display(),
                e
            )
        })? {
            let entry = entry.map_err(|e| format!("Failed to read container dir entry: {}", e))?;
            if !entry
                .file_type()
                .map_err(|e| format!("Failed to inspect container dir entry: {}", e))?
                .is_dir()
            {
                continue;
            }
            let id = entry.file_name().to_string_lossy().to_string();
            let mut record = self.load(&id)?;
            self.refresh_state(&mut record)?;
            items.push(ContainerSummary {
                id: record.id,
                image_reference: record.image_reference,
                state: record.state,
                pid: record.pid,
                exit_code: record.exit_code,
                created_at: record.created_at,
                started_at: record.started_at,
                finished_at: record.finished_at,
            });
        }
        items.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(items)
    }

    pub fn start(&self, id: &str) -> Result<ContainerRecord, String> {
        let mut record = self.load(id)?;
        self.refresh_state(&mut record)?;
        if matches!(
            record.state,
            ContainerState::Starting | ContainerState::Running
        ) {
            return Err(format!("Container '{}' is already {:?}", id, record.state));
        }

        let dir = self.container_dir(id);
        let stdout_path = dir.join("stdout.log");
        let stderr_path = dir.join("stderr.log");
        let stdout = File::options()
            .create(true)
            .append(true)
            .open(&stdout_path)
            .map_err(|e| format!("Failed to open log file '{}': {}", stdout_path.display(), e))?;
        let stderr = File::options()
            .create(true)
            .append(true)
            .open(&stderr_path)
            .map_err(|e| format!("Failed to open log file '{}': {}", stderr_path.display(), e))?;

        record.state = ContainerState::Starting;
        record.exit_code = None;
        record.finished_at = None;
        self.write_record(&record)?;

        let exe = std::env::current_exe()
            .map_err(|e| format!("Failed to resolve qbuild executable path: {}", e))?;
        let mut command = std::process::Command::new(exe);
        command
            .arg("internal-exec")
            .arg("--data-root")
            .arg(self.data_root())
            .arg("--store-dir")
            .arg(&self.store_dir)
            .arg("--container-id")
            .arg(id)
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));
        unsafe {
            command.pre_exec(|| {
                nix::unistd::setsid()
                    .map(|_| ())
                    .map_err(|e| std::io::Error::other(e.to_string()))
            });
        }
        let child = command
            .spawn()
            .map_err(|e| format!("Failed to spawn container '{}': {}", id, e))?;

        record.pid = Some(child.id());
        record.started_at = Some(Utc::now().timestamp());
        self.write_record(&record)?;
        Ok(record)
    }

    pub fn stop(&self, id: &str, signal: Signal) -> Result<ContainerRecord, String> {
        let mut record = self.load(id)?;
        self.refresh_state(&mut record)?;
        let pid = record
            .pid
            .ok_or_else(|| format!("Container '{}' does not have a running pid", id))?;
        kill(Pid::from_raw(-(pid as i32)), signal)
            .or_else(|_| kill(Pid::from_raw(pid as i32), signal))
            .map_err(|e| format!("Failed to signal container '{}': {}", id, e))?;
        Ok(record)
    }

    pub fn remove(&self, id: &str) -> Result<(), String> {
        let mut record = self.load(id)?;
        self.refresh_state(&mut record)?;
        if matches!(
            record.state,
            ContainerState::Starting | ContainerState::Running
        ) {
            return Err(format!("Container '{}' is still {:?}", id, record.state));
        }
        std::fs::remove_dir_all(self.container_dir(id)).map_err(|e| {
            format!(
                "Failed to remove container dir '{}': {}",
                self.container_dir(id).display(),
                e
            )
        })
    }

    pub fn logs(&self, id: &str, stderr: bool) -> Result<String, String> {
        let path = if stderr {
            self.container_dir(id).join("stderr.log")
        } else {
            self.container_dir(id).join("stdout.log")
        };
        let mut file = File::open(&path)
            .map_err(|e| format!("Failed to open log file '{}': {}", path.display(), e))?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)
            .map_err(|e| format!("Failed to read log file '{}': {}", path.display(), e))?;
        Ok(contents)
    }

    pub async fn run_managed(&self, id: &str) -> Result<RunResult, String> {
        let mut record = self.load(id)?;
        record.state = ContainerState::Running;
        record.started_at = Some(Utc::now().timestamp());
        record.pid = Some(std::process::id());
        self.write_status(&ExecStatusFile::running_from(&record))?;
        self.write_record(&record)?;

        let service = RunService::new();
        let result = service
            .run(RunRequest {
                image_reference: record.image_reference.clone(),
                command: record.command.clone(),
                environment: record.environment.clone(),
                working_directory: record.working_directory.clone(),
                store_dir: self.store_dir.clone(),
                namespace_config: record.namespace_config.clone(),
                resource_limits: record.resource_limits.clone(),
                clear_image_env: record.clear_image_env,
                container_id: Some(record.id.clone()),
                status_file: Some(self.status_path(&record.id)),
                started_at: record.started_at,
            })
            .await;

        match result {
            Ok(result) => {
                let exit_code = result.exit_status.code().unwrap_or(1);
                self.write_status(&ExecStatusFile::exited(&record, exit_code))?;
                Ok(result)
            }
            Err(err) => {
                self.write_status(&ExecStatusFile::failed(&record, 1))?;
                Err(err)
            }
        }
    }

    fn refresh_state(&self, record: &mut ContainerRecord) -> Result<(), String> {
        let status_path = self.status_path(&record.id);
        if status_path.exists() {
            let bytes = std::fs::read(&status_path).map_err(|e| {
                format!(
                    "Failed to read container status '{}': {}",
                    status_path.display(),
                    e
                )
            })?;
            let status: ExecStatusFile = serde_json::from_slice(&bytes).map_err(|e| {
                format!(
                    "Failed to decode container status '{}': {}",
                    status_path.display(),
                    e
                )
            })?;
            record.state = status.state;
            record.exit_code = status.exit_code;
            record.pid = status.pid;
            record.started_at = status.started_at;
            record.finished_at = status.finished_at;
            self.write_record(record)?;
            return Ok(());
        }

        if matches!(
            record.state,
            ContainerState::Starting | ContainerState::Running
        ) && let Some(pid) = record.pid
            && !process_exists(pid)
        {
            record.state = ContainerState::Exited;
            record.finished_at = Some(Utc::now().timestamp());
            self.write_record(record)?;
        }
        Ok(())
    }

    fn data_root(&self) -> &Path {
        self.root
            .parent()
            .expect("container store root should have a parent")
    }

    fn write_record(&self, record: &ContainerRecord) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(record)
            .map_err(|e| format!("Failed to encode container record '{}': {}", record.id, e))?;
        let path = self.record_path(&record.id);
        std::fs::write(&path, bytes).map_err(|e| {
            format!(
                "Failed to write container record '{}': {}",
                path.display(),
                e
            )
        })
    }

    fn write_status(&self, status: &ExecStatusFile) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(status)
            .map_err(|e| format!("Failed to encode container status '{}': {}", status.id, e))?;
        let path = self.status_path(&status.id);
        std::fs::write(&path, bytes).map_err(|e| {
            format!(
                "Failed to write container status '{}': {}",
                path.display(),
                e
            )
        })
    }

    fn container_dir(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }

    fn record_path(&self, id: &str) -> PathBuf {
        self.container_dir(id).join("container.json")
    }

    fn status_path(&self, id: &str) -> PathBuf {
        self.container_dir(id).join("status.json")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExecStatusFile {
    id: String,
    state: ContainerState,
    pid: Option<u32>,
    exit_code: Option<i32>,
    started_at: Option<i64>,
    finished_at: Option<i64>,
}

impl ExecStatusFile {
    fn running_from(record: &ContainerRecord) -> Self {
        Self {
            id: record.id.clone(),
            state: ContainerState::Running,
            pid: Some(std::process::id()),
            exit_code: None,
            started_at: record.started_at,
            finished_at: None,
        }
    }

    fn exited(record: &ContainerRecord, exit_code: i32) -> Self {
        Self {
            id: record.id.clone(),
            state: ContainerState::Exited,
            pid: Some(std::process::id()),
            exit_code: Some(exit_code),
            started_at: record.started_at,
            finished_at: Some(Utc::now().timestamp()),
        }
    }

    fn failed(record: &ContainerRecord, exit_code: i32) -> Self {
        Self::exited(record, exit_code)
    }
}

fn process_exists(pid: u32) -> bool {
    kill(Pid::from_raw(pid as i32), None).is_ok()
}
