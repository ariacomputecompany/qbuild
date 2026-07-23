#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use crate::cli::{
    BuildCommand, Commands, CreateCommand, InspectCommand, ListCommand, LogsCommand, PsCommand,
    PullCommand, PushCommand, RemoveCommand, RunCommand, StartCommand, StopCommand,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NamespaceConfig {
    pub mount: bool,
    pub uts: bool,
    pub ipc: bool,
    pub network: bool,
}

impl Default for NamespaceConfig {
    fn default() -> Self {
        Self {
            mount: true,
            uts: false,
            ipc: false,
            network: false,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GpuRequest {
    pub count: u32,
    pub device_ids: Vec<String>,
}

impl GpuRequest {
    pub fn is_enabled(&self) -> bool {
        self.count > 0
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.count == 0 && !self.device_ids.is_empty() {
            return Err("gpu_ids require gpu_count > 0".to_string());
        }
        if !self.device_ids.is_empty() && self.count as usize != self.device_ids.len() {
            return Err("gpu_count must exactly match gpu_ids length".to_string());
        }
        let mut seen = std::collections::HashSet::new();
        for id in &self.device_ids {
            if id.trim().is_empty() {
                return Err("gpu_ids cannot contain empty values".to_string());
            }
            if !seen.insert(id) {
                return Err(format!("duplicate gpu_id '{}'", id));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResourceLimits {
    pub memory_limit_bytes: Option<u64>,
    pub cpu_quota: Option<i64>,
    pub cpu_period: Option<u64>,
    pub pids_limit: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BindMountSpec {
    pub source: String,
    pub target: String,
    pub readonly: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContainerState {
    Created,
    Starting,
    Running,
    Exited,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CommandRequest {
    Ping,
    Build(BuildCommand),
    Pull(PullCommand),
    Push(PushCommand),
    Run(RunCommand),
    Create(CreateCommand),
    Start(StartCommand),
    Stop(StopCommand),
    Rm(RemoveCommand),
    Ps(PsCommand),
    Logs(LogsCommand),
    Inspect(InspectCommand),
    List(ListCommand),
}

impl TryFrom<Commands> for CommandRequest {
    type Error = String;

    fn try_from(value: Commands) -> Result<Self, Self::Error> {
        match value {
            Commands::Build(cmd) => Ok(Self::Build(cmd)),
            Commands::Pull(cmd) => Ok(Self::Pull(cmd)),
            Commands::Push(cmd) => Ok(Self::Push(cmd)),
            Commands::Run(cmd) => Ok(Self::Run(cmd)),
            Commands::Create(cmd) => Ok(Self::Create(cmd)),
            Commands::Start(cmd) => Ok(Self::Start(cmd)),
            Commands::Stop(cmd) => Ok(Self::Stop(cmd)),
            Commands::Rm(cmd) => Ok(Self::Rm(cmd)),
            Commands::Ps(cmd) => Ok(Self::Ps(cmd)),
            Commands::Logs(cmd) => Ok(Self::Logs(cmd)),
            Commands::Inspect(cmd) => Ok(Self::Inspect(cmd)),
            Commands::List(cmd) => Ok(Self::List(cmd)),
            Commands::Guestd(_) | Commands::InternalExec(_) => {
                Err("command is not part of the guest RPC surface".to_string())
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GuestFrame {
    Event(GuestEvent),
    Response(GuestResponse),
    Error(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GuestEvent {
    Status(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GuestResponse {
    Pong,
    Build(BuildOutput),
    Pull(PullOutput),
    Push(PushOutput),
    Run(RunOutput),
    Create(CreateOutput),
    Start(StartOutput),
    Stop(StopOutput),
    Removed(RemoveOutput),
    Ps(PsOutput),
    Logs(LogsOutput),
    Inspect(InspectOutput),
    List(ListOutput),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildOutput {
    pub image_reference: String,
    pub manifest_digest: String,
    pub config_digest: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullOutput {
    pub reference: String,
    pub manifest_digest: String,
    pub config_digest: String,
    pub size_bytes: u64,
    pub layers: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushOutput {
    pub reference: String,
    pub manifest_digest: String,
    pub registry: String,
    pub repository: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunOutput {
    pub exit_code: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateOutput {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartOutput {
    pub id: String,
    pub pid: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StopOutput {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoveOutput {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PsOutput {
    pub containers: Vec<ContainerSummaryOutput>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerSummaryOutput {
    pub id: String,
    pub image_reference: String,
    pub name: Option<String>,
    pub state: ContainerState,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogsOutput {
    pub contents: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InspectOutput {
    pub reference: String,
    pub manifest_digest: String,
    pub config_digest: String,
    pub architecture: String,
    pub os: String,
    pub size_bytes: u64,
    pub layers: usize,
    pub env: Vec<String>,
    pub cmd: Vec<String>,
    pub entrypoint: Vec<String>,
    pub working_dir: Option<String>,
    pub user: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListOutput {
    pub images: Vec<ImageReferenceOutput>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageReferenceOutput {
    pub reference: String,
    pub manifest_digest: String,
}
