use crate::cli::GuestdCommand;
use crate::error::AppError;
use crate::protocol::{CommandRequest, GuestFrame};
use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream, UnixListener, UnixStream};

pub async fn serve(config: &GuestdCommand) -> Result<(), AppError> {
    if let Some(path) = &config.listen_unix {
        return serve_unix(path).await;
    }
    serve_tcp(&config.listen).await
}

async fn serve_tcp(listen: &str) -> Result<(), AppError> {
    let listener = TcpListener::bind(listen).await.map_err(|e| {
        AppError::Message(format!("failed to bind guest daemon on {}: {}", listen, e))
    })?;
    eprintln!("qbuild guest daemon listening on {}", listen);

    loop {
        let (stream, _) = listener
            .accept()
            .await
            .map_err(|e| AppError::Message(format!("failed to accept guest connection: {}", e)))?;
        tokio::spawn(async move {
            if let Err(err) = handle_tcp_client(stream).await {
                eprintln!("guest daemon connection error: {}", err);
            }
        });
    }
}

async fn serve_unix(path: &Path) -> Result<(), AppError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if path.exists() {
        let _ = std::fs::remove_file(path);
    }
    let listener = UnixListener::bind(path).map_err(|e| {
        AppError::Message(format!(
            "failed to bind guest daemon unix socket at '{}': {}",
            path.display(),
            e
        ))
    })?;
    eprintln!("qbuild guest daemon listening on {}", path.display());

    loop {
        let (stream, _) = listener.accept().await.map_err(|e| {
            AppError::Message(format!(
                "failed to accept guest unix connection on '{}': {}",
                path.display(),
                e
            ))
        })?;
        tokio::spawn(async move {
            if let Err(err) = handle_unix_client(stream).await {
                eprintln!("guest daemon connection error: {}", err);
            }
        });
    }
}

async fn handle_tcp_client(stream: TcpStream) -> Result<(), AppError> {
    let (read_half, write_half) = stream.into_split();
    handle_stream(read_half, write_half).await
}

async fn handle_unix_client(stream: UnixStream) -> Result<(), AppError> {
    let (read_half, write_half) = stream.into_split();
    handle_stream(read_half, write_half).await
}

async fn handle_stream<R, W>(read_half: R, write_half: W) -> Result<(), AppError>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    if reader.read_line(&mut line).await? == 0 {
        return Ok(());
    }
    let request: CommandRequest = serde_json::from_str(line.trim_end())?;
    let writer = Arc::new(tokio::sync::Mutex::new(write_half));
    let event_writer = Arc::clone(&writer);
    let emit = Arc::new(move |event| {
        let event_writer = Arc::clone(&event_writer);
        tokio::spawn(async move {
            let frame = GuestFrame::Event(event);
            if let Ok(line) = serde_json::to_vec(&frame) {
                let mut guard = event_writer.lock().await;
                let _ = guard.write_all(&line).await;
                let _ = guard.write_all(b"\n").await;
            }
        });
    });

    match crate::services::execute(request, emit).await {
        Ok(response) => {
            let frame = GuestFrame::Response(response);
            let line = serde_json::to_vec(&frame)?;
            let mut guard = writer.lock().await;
            guard.write_all(&line).await?;
            guard.write_all(b"\n").await?;
            guard.flush().await?;
        }
        Err(err) => {
            let frame = GuestFrame::Error(err.to_string());
            let line = serde_json::to_vec(&frame)?;
            let mut guard = writer.lock().await;
            guard.write_all(&line).await?;
            guard.write_all(b"\n").await?;
            guard.flush().await?;
        }
    }

    Ok(())
}
