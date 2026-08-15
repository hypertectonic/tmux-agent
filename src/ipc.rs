use crate::model::{IpcRequest, IpcResponse, Snapshot};
use anyhow::{Context, Result, bail};
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

pub struct SnapshotWatch {
    lines: Lines<BufReader<OwnedReadHalf>>,
    _writer: OwnedWriteHalf,
}

impl SnapshotWatch {
    pub async fn connect(socket: &Path, local_only: bool) -> Result<Self> {
        let stream = UnixStream::connect(socket)
            .await
            .with_context(|| format!("connect to daemon {}", socket.display()))?;
        let (reader, mut writer) = stream.into_split();
        write_request(&mut writer, &IpcRequest::Watch { local_only }).await?;
        Ok(Self {
            lines: BufReader::new(reader).lines(),
            _writer: writer,
        })
    }

    pub async fn next_snapshot(&mut self) -> Result<Option<Snapshot>> {
        self.lines
            .next_line()
            .await
            .context("read daemon stream")?
            .map(|line| parse_response(&line))
            .transpose()
    }
}

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
    let mut watch = SnapshotWatch::connect(socket, local_only).await?;
    let mut stdout = tokio::io::stdout();
    while let Some(snapshot) = watch.next_snapshot().await? {
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
    request_ack(
        socket,
        IpcRequest::Acknowledge {
            target: target.to_string(),
        },
        "acknowledge",
    )
    .await
}

pub async fn mark_used(socket: &Path, target: &str) -> Result<()> {
    request_ack(
        socket,
        IpcRequest::MarkUsed {
            target: target.to_string(),
        },
        "mark used",
    )
    .await
}

async fn request_ack(socket: &Path, request: IpcRequest, operation: &str) -> Result<()> {
    let stream = UnixStream::connect(socket)
        .await
        .with_context(|| format!("connect to daemon {}", socket.display()))?;
    let (reader, mut writer) = stream.into_split();
    write_request(&mut writer, &request).await?;
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    match serde_json::from_str(&line).with_context(|| format!("parse {operation} response"))? {
        IpcResponse::Ack => Ok(()),
        IpcResponse::Error { message } => bail!("daemon error: {message}"),
        IpcResponse::Snapshot { .. } => bail!("unexpected snapshot response to {operation}"),
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use tokio::net::UnixListener;

    #[tokio::test]
    async fn snapshot_watch_delivers_initial_and_changed_snapshots_on_one_connection() {
        let directory = tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut request = String::new();
            reader.read_line(&mut request).await.unwrap();
            assert!(matches!(
                serde_json::from_str(&request).unwrap(),
                IpcRequest::Watch { local_only: false }
            ));

            for revision in [1, 2] {
                let response = IpcResponse::Snapshot {
                    snapshot: Snapshot {
                        revision,
                        ..Snapshot::default()
                    },
                };
                let mut encoded = serde_json::to_vec(&response).unwrap();
                encoded.push(b'\n');
                writer.write_all(&encoded).await.unwrap();
            }
        });

        let mut watch = SnapshotWatch::connect(&socket, false).await.unwrap();

        assert_eq!(watch.next_snapshot().await.unwrap().unwrap().revision, 1);
        assert_eq!(watch.next_snapshot().await.unwrap().unwrap().revision, 2);
        server.await.unwrap();
    }
}
