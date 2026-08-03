use crate::model::{IpcRequest, IpcResponse, Snapshot};
use anyhow::{Context, Result, bail};
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

pub async fn snapshot(socket: &Path, local_only: bool) -> Result<Snapshot> {
    let stream = UnixStream::connect(socket)
        .await
        .with_context(|| format!("connect to daemon {}", socket.display()))?;
    let (reader, mut writer) = stream.into_split();
    write_request(&mut writer, &IpcRequest::Snapshot { local_only }).await?;
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .await
        .context("read daemon response")?;
    parse_response(&line)
}

pub async fn watch_jsonl(socket: &Path, local_only: bool) -> Result<()> {
    let stream = UnixStream::connect(socket)
        .await
        .with_context(|| format!("connect to daemon {}", socket.display()))?;
    let (reader, mut writer) = stream.into_split();
    write_request(&mut writer, &IpcRequest::Watch { local_only }).await?;
    let mut lines = BufReader::new(reader).lines();
    let mut stdout = tokio::io::stdout();
    while let Some(line) = lines.next_line().await.context("read daemon stream")? {
        let snapshot = parse_response(&line)?;
        let encoded = serde_json::to_vec(&snapshot).context("serialize snapshot")?;
        if let Err(error) = stdout.write_all(&encoded).await {
            if error.kind() == std::io::ErrorKind::BrokenPipe {
                return Ok(());
            }
            return Err(error.into());
        }
        if let Err(error) = stdout.write_all(b"\n").await {
            if error.kind() == std::io::ErrorKind::BrokenPipe {
                return Ok(());
            }
            return Err(error.into());
        }
        if let Err(error) = stdout.flush().await {
            if error.kind() == std::io::ErrorKind::BrokenPipe {
                return Ok(());
            }
            return Err(error.into());
        }
    }
    Ok(())
}

pub async fn shutdown(socket: &Path) -> Result<()> {
    let stream = UnixStream::connect(socket)
        .await
        .with_context(|| format!("connect to daemon {}", socket.display()))?;
    let (reader, mut writer) = stream.into_split();
    write_request(&mut writer, &IpcRequest::Shutdown).await?;
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    match serde_json::from_str(&line).context("parse shutdown response")? {
        IpcResponse::Ack => {}
        IpcResponse::Error { message } => bail!("daemon error: {message}"),
        IpcResponse::Snapshot { .. } => bail!("unexpected snapshot response to shutdown"),
    }
    for _ in 0..60 {
        if !socket.exists() || UnixStream::connect(socket).await.is_err() {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    bail!("daemon acknowledged shutdown but socket is still accepting connections")
}

pub async fn acknowledge(socket: &Path, target: &str) -> Result<()> {
    let stream = UnixStream::connect(socket)
        .await
        .with_context(|| format!("connect to daemon {}", socket.display()))?;
    let (reader, mut writer) = stream.into_split();
    write_request(
        &mut writer,
        &IpcRequest::Acknowledge {
            target: target.to_string(),
        },
    )
    .await?;
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    match serde_json::from_str(&line).context("parse acknowledge response")? {
        IpcResponse::Ack => Ok(()),
        IpcResponse::Error { message } => bail!("daemon error: {message}"),
        IpcResponse::Snapshot { .. } => bail!("unexpected snapshot response to acknowledge"),
    }
}

async fn write_request(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    request: &IpcRequest,
) -> Result<()> {
    let mut encoded = serde_json::to_vec(request).context("serialize daemon request")?;
    encoded.push(b'\n');
    writer
        .write_all(&encoded)
        .await
        .context("write daemon request")
}

fn parse_response(line: &str) -> Result<Snapshot> {
    match serde_json::from_str(line).context("parse daemon response")? {
        IpcResponse::Snapshot { snapshot } => Ok(snapshot),
        IpcResponse::Ack => bail!("unexpected daemon acknowledgement"),
        IpcResponse::Error { message } => bail!("daemon error: {message}"),
    }
}
