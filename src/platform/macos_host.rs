use crate::cli::{Cli, Commands};
use crate::error::AppError;
use crate::platform::render::{command_to_request, print_event, render_response};
use crate::protocol::{CommandRequest, GuestFrame, GuestResponse};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub async fn run(cli: Cli) -> Result<(), AppError> {
    match cli.command {
        Commands::Guestd(_) | Commands::InternalExec(_) => Err(AppError::Message(
            "macOS host mode cannot run guest-local commands directly".to_string(),
        )),
        other => {
            let request = command_to_request(other)?;
            let endpoint = ensure_running()?;
            let response = send_request(&endpoint, &request)?;
            render_response(&request, response)
        }
    }
}

fn ensure_running() -> Result<PathBuf, AppError> {
    let install = MacosInstall::resolve()?;
    if ping_guest(&install.socket_path)? {
        return Ok(install.socket_path);
    }

    std::fs::create_dir_all(&install.state_dir)?;
    if install.socket_path.exists() {
        let _ = std::fs::remove_file(&install.socket_path);
    }

    spawn_supervisor(&install)?;
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if ping_guest(&install.socket_path)? {
            return Ok(install.socket_path.clone());
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    Err(AppError::Message(format!(
        "qbuild macOS supervisor did not become ready. Expected guest socket at {}",
        install.socket_path.display()
    )))
}

fn send_request(
    socket_path: &std::path::Path,
    request: &CommandRequest,
) -> Result<GuestResponse, AppError> {
    let mut stream = UnixStream::connect(socket_path).map_err(|e| {
        AppError::Message(format!(
            "failed to connect to qbuild guest socket at '{}': {}",
            socket_path.display(),
            e
        ))
    })?;
    let request_line = serde_json::to_vec(request)?;
    stream.write_all(&request_line)?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Err(AppError::Message(
                "guest daemon closed the connection before returning a response".to_string(),
            ));
        }
        let frame: GuestFrame = serde_json::from_str(line.trim_end())?;
        match frame {
            GuestFrame::Event(event) => print_event(event),
            GuestFrame::Response(response) => return Ok(response),
            GuestFrame::Error(message) => return Err(AppError::Message(message)),
        }
    }
}

fn ping_guest(socket_path: &std::path::Path) -> Result<bool, AppError> {
    if !socket_path.exists() {
        return Ok(false);
    }
    match send_request(socket_path, &CommandRequest::Ping) {
        Ok(GuestResponse::Pong) => Ok(true),
        Ok(_) => Err(AppError::Message(
            "guest daemon health check returned an unexpected response".to_string(),
        )),
        Err(AppError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(AppError::Message(message))
            if message.contains("failed to connect")
                || message.contains("closed the connection") =>
        {
            Ok(false)
        }
        Err(err) => Err(err),
    }
}

fn spawn_supervisor(install: &MacosInstall) -> Result<(), AppError> {
    let mut command = Command::new(&install.supervisor_bin);
    command
        .arg("daemon")
        .arg("--state-dir")
        .arg(&install.state_dir)
        .arg("--guest-socket-host")
        .arg(&install.socket_path)
        .arg("--guest-rootfs")
        .arg(&install.guest_rootfs)
        .arg("--init-block")
        .arg(&install.init_block)
        .arg("--kernel")
        .arg(&install.kernel)
        .arg("--home-share")
        .arg(dirs::home_dir().unwrap_or_else(|| PathBuf::from("/Users")))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    command.spawn().map_err(|e| {
        AppError::Message(format!(
            "failed to launch qbuild macOS supervisor '{}': {}",
            install.supervisor_bin.display(),
            e
        ))
    })?;
    Ok(())
}

#[derive(Debug, Clone)]
struct MacosInstall {
    state_dir: PathBuf,
    socket_path: PathBuf,
    supervisor_bin: PathBuf,
    kernel: PathBuf,
    init_block: PathBuf,
    guest_rootfs: PathBuf,
}

impl MacosInstall {
    fn resolve() -> Result<Self, AppError> {
        let exe = std::env::current_exe()?;
        let exe_dir = exe.parent().ok_or_else(|| {
            AppError::Message("failed to resolve qbuild executable directory".to_string())
        })?;
        let repo_root = exe_dir
            .ancestors()
            .find(|path| path.join("MACOS.md").is_file() && path.join("Cargo.toml").is_file())
            .map(PathBuf::from);

        let manifest_dir = repo_root
            .clone()
            .map(|root| root.join("packaging/macos-guest"))
            .unwrap_or_else(|| exe_dir.join("../share/qbuild/macos-guest"));
        let state_dir = crate::default_data_root().join("macos");
        let socket_path = state_dir.join("guestd.sock");
        let supervisor_bin = resolve_supervisor_bin(exe_dir, repo_root.as_deref())?;

        Ok(Self {
            state_dir,
            socket_path,
            supervisor_bin,
            kernel: manifest_dir.join("vmlinux"),
            init_block: manifest_dir.join("init.block"),
            guest_rootfs: manifest_dir.join("guest-rootfs.ext4"),
        })
    }
}

fn resolve_supervisor_bin(
    exe_dir: &std::path::Path,
    repo_root: Option<&std::path::Path>,
) -> Result<PathBuf, AppError> {
    let release_candidate = exe_dir.join("qbuild-macos-supervisor");
    if release_candidate.is_file() {
        return Ok(release_candidate);
    }

    if let Some(root) = repo_root {
        let source_bin = root.join("tools/macos-supervisor/.build/release/qbuild-macos-supervisor");
        if source_bin.is_file() {
            return Ok(source_bin);
        }

        let package_dir = root.join("tools/macos-supervisor");
        if package_dir.join("Package.swift").is_file() {
            let builder = package_dir.join("build.sh");
            let status = if builder.is_file() {
                Command::new(&builder).current_dir(&package_dir).status()?
            } else {
                Command::new("swift")
                    .arg("build")
                    .arg("-c")
                    .arg("release")
                    .current_dir(&package_dir)
                    .status()?
            };
            if status.success() && source_bin.is_file() {
                return Ok(source_bin);
            }
        }
    }

    Err(AppError::Message(
        "qbuild macOS supervisor binary is unavailable. Build tools/macos-supervisor or install a release bundle with qbuild-macos-supervisor and guest assets.".to_string(),
    ))
}
