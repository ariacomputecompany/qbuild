use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Parser, Debug, Clone)]
#[command(
    name = "qbuild",
    version,
    about = "Standalone OCI image builder for Quilt-compatible artifacts"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug, Clone)]
pub enum Commands {
    /// Build an OCI image from a local Docker build context
    Build(BuildCommand),
    /// Pull an OCI image into the local qbuild store
    Pull(PullCommand),
    /// Push a locally stored OCI image reference to a registry
    Push(PushCommand),
    /// Run a locally stored OCI image standalone
    Run(RunCommand),
    /// Create a persistent local container definition
    Create(CreateCommand),
    /// Start a persistent local container
    Start(StartCommand),
    /// Stop a running local container
    Stop(StopCommand),
    /// Remove a stopped local container
    Rm(RemoveCommand),
    /// List local containers
    Ps(PsCommand),
    /// Print container logs
    Logs(LogsCommand),
    /// Inspect a locally stored OCI image reference
    Inspect(InspectCommand),
    /// List locally stored image references
    List(ListCommand),
    /// Run the Linux guest daemon
    Guestd(GuestdCommand),
    #[command(hide = true, name = "internal-exec")]
    InternalExec(InternalExecCommand),
}

#[derive(Args, Debug, Clone, Serialize, Deserialize)]
pub struct BuildCommand {
    #[arg(default_value = ".")]
    pub context: String,
    #[arg(long, default_value = "Dockerfile")]
    pub dockerfile: String,
    #[arg(long)]
    pub image: String,
    #[arg(long = "build-arg")]
    pub build_arg: Vec<String>,
    #[arg(long)]
    pub target: Option<String>,
    #[arg(long)]
    pub store_dir: Option<PathBuf>,
    #[arg(long)]
    pub work_dir: Option<PathBuf>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug, Clone, Serialize, Deserialize)]
pub struct PullCommand {
    pub reference: String,
    #[arg(long)]
    pub store_dir: Option<PathBuf>,
    #[arg(long)]
    pub username: Option<String>,
    #[arg(long)]
    pub password: Option<String>,
    #[arg(long)]
    pub force: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug, Clone, Serialize, Deserialize)]
pub struct PushCommand {
    pub reference: String,
    #[arg(long)]
    pub store_dir: Option<PathBuf>,
    #[arg(long)]
    pub username: Option<String>,
    #[arg(long)]
    pub password: Option<String>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug, Clone, Serialize, Deserialize)]
pub struct RunCommand {
    pub reference: String,
    #[arg(long)]
    pub store_dir: Option<PathBuf>,
    #[arg(short = 'e', long = "env")]
    pub env: Vec<String>,
    #[arg(long)]
    pub workdir: Option<String>,
    #[arg(long)]
    pub memory_mb: Option<u64>,
    #[arg(long)]
    pub cpu_percent: Option<f64>,
    #[arg(long)]
    pub pids_limit: Option<u64>,
    #[arg(long)]
    pub clear_image_env: bool,
    #[arg(long)]
    pub no_mount_namespace: bool,
    #[arg(long)]
    pub uts_namespace: bool,
    #[arg(short = 'v', long = "volume", alias = "mount")]
    pub mounts: Vec<String>,
    #[arg(long)]
    pub name: Option<String>,
    #[arg(long)]
    pub ipc_namespace: bool,
    #[arg(long)]
    pub network_namespace: bool,
    #[arg(last = true)]
    pub command: Vec<String>,
}

#[derive(Args, Debug, Clone, Serialize, Deserialize)]
pub struct CreateCommand {
    pub reference: String,
    #[arg(long)]
    pub store_dir: Option<PathBuf>,
    #[arg(long)]
    pub data_root: Option<PathBuf>,
    #[arg(short = 'e', long = "env")]
    pub env: Vec<String>,
    #[arg(long)]
    pub workdir: Option<String>,
    #[arg(long)]
    pub memory_mb: Option<u64>,
    #[arg(long)]
    pub cpu_percent: Option<f64>,
    #[arg(long)]
    pub pids_limit: Option<u64>,
    #[arg(long)]
    pub clear_image_env: bool,
    #[arg(long)]
    pub no_mount_namespace: bool,
    #[arg(long)]
    pub uts_namespace: bool,
    #[arg(short = 'v', long = "volume", alias = "mount")]
    pub mounts: Vec<String>,
    #[arg(long)]
    pub name: Option<String>,
    #[arg(long)]
    pub ipc_namespace: bool,
    #[arg(long)]
    pub network_namespace: bool,
    #[arg(last = true)]
    pub command: Vec<String>,
}

#[derive(Args, Debug, Clone, Serialize, Deserialize)]
pub struct StartCommand {
    pub id: String,
    #[arg(long)]
    pub store_dir: Option<PathBuf>,
    #[arg(long)]
    pub data_root: Option<PathBuf>,
}

#[derive(Args, Debug, Clone, Serialize, Deserialize)]
pub struct StopCommand {
    pub id: String,
    #[arg(long)]
    pub store_dir: Option<PathBuf>,
    #[arg(long)]
    pub data_root: Option<PathBuf>,
    #[arg(long, default_value = "term")]
    pub signal: String,
}

#[derive(Args, Debug, Clone, Serialize, Deserialize)]
pub struct RemoveCommand {
    pub id: String,
    #[arg(long)]
    pub store_dir: Option<PathBuf>,
    #[arg(long)]
    pub data_root: Option<PathBuf>,
}

#[derive(Args, Debug, Clone, Serialize, Deserialize)]
pub struct PsCommand {
    #[arg(long)]
    pub store_dir: Option<PathBuf>,
    #[arg(long)]
    pub data_root: Option<PathBuf>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug, Clone, Serialize, Deserialize)]
pub struct LogsCommand {
    pub id: String,
    #[arg(long)]
    pub data_root: Option<PathBuf>,
    #[arg(long)]
    pub store_dir: Option<PathBuf>,
    #[arg(long)]
    pub stderr: bool,
}

#[derive(Args, Debug, Clone, Serialize, Deserialize)]
pub struct InspectCommand {
    pub reference: String,
    #[arg(long)]
    pub store_dir: Option<PathBuf>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug, Clone, Serialize, Deserialize)]
pub struct ListCommand {
    #[arg(long)]
    pub store_dir: Option<PathBuf>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug, Clone)]
pub struct GuestdCommand {
    #[arg(long, default_value = "127.0.0.1:42141")]
    pub listen: String,
    #[arg(long)]
    pub listen_unix: Option<PathBuf>,
}

#[derive(Args, Debug, Clone)]
pub struct InternalExecCommand {
    #[arg(long)]
    pub data_root: PathBuf,
    #[arg(long)]
    pub store_dir: PathBuf,
    #[arg(long)]
    pub container_id: String,
}
