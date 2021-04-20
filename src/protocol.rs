use crate::prelude::*;
use serde::{Deserialize, Serialize};
use std::{convert::TryFrom, fmt};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Debug, Deserialize, Serialize)]
pub(crate) enum Request {
    Execute(ExecuteRequest),
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct ExecuteRequest {
    pub(crate) command: ExecuteCommand,
    pub(crate) envs: Vec<(String, String)>,
    pub(crate) pty_param: Option<PtyParam>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) enum ExecuteCommand {
    LoginShell,
    Program { command: Vec<String> },
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct PtyParam {
    pub(crate) window_size: WindowSize,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct WindowSize {
    pub(crate) width: u16,
    pub(crate) height: u16,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) enum ExecuteResponse {
    Ok,
    Err(String),
}

#[derive(Debug, Copy, Clone, Deserialize, Serialize)]
pub(crate) enum ExitStatus {
    Code(i32),
    Signal(i32),
}

#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) enum C2sStreamKind {
    Stdin,
}

#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) enum S2cStreamKind {
    Stdout,
    Stderr,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) enum ServerAction {
    SourceAction(S2cStreamKind, SourceAction),
    SinkAction(C2sStreamKind, SinkAction),
    Exit(ExitStatus),
    Finished,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) enum ClientAction {
    SourceAction(C2sStreamKind, SourceAction),
    SinkAction(S2cStreamKind, SinkAction),
    WindowSizeChange(WindowSize),
    Finished,
}

#[derive(Deserialize, Serialize)]
pub(crate) enum SourceAction {
    Data(Vec<u8>),
    SourceClosed,
}

impl fmt::Debug for SourceAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        struct Bytes<'a>(&'a [u8]);
        impl fmt::Debug for Bytes<'_> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                const LENGTH: usize = 8;
                for (idx, byte) in self.0.iter().take(LENGTH).enumerate() {
                    if idx != 0 {
                        write!(f, " ")?;
                    }
                    write!(f, "{:02x}", byte)?;
                }
                if self.0.len() > LENGTH {
                    write!(f, " ..")?;
                }
                Ok(())
            }
        }

        match self {
            SourceAction::Data(bytes) => f
                .debug_struct("Data")
                .field("len", &bytes.len())
                .field("bytes", &Bytes(&bytes))
                .finish(),
            SourceAction::SourceClosed => f.debug_tuple("SourceClosed").finish(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) enum SinkAction {
    Ack,
    SinkClosed,
}

pub(crate) const MAX_STREAM_PACKET_SIZE: usize = 4096;

const MESSAGE_SIZE_LIMIT: u32 = 1024 * 1024; // 1MiB

pub(crate) async fn recv_message<T>(stream: impl AsyncRead) -> Result<T>
where
    T: for<'a> Deserialize<'a> + 'static,
{
    tokio::pin!(stream);

    let size = stream
        .read_u32()
        .await
        .wrap_err("failed to receive message size")?;
    ensure!(
        size <= MESSAGE_SIZE_LIMIT,
        "message size is too large, size={}",
        size
    );
    // TODO: reuse buffer
    let mut bytes = vec![0; size as usize];
    stream
        .read_exact(&mut bytes)
        .await
        .wrap_err("failed to receive message")?;
    let data = bincode::deserialize(&bytes).wrap_err("failed to deserialize message")?;
    Ok(data)
}

pub(crate) async fn send_message<T>(stream: impl AsyncWrite, data: &T) -> Result<()>
where
    T: Serialize,
{
    tokio::pin!(stream);

    let size = bincode::serialized_size(&data)?;
    ensure!(
        size <= u64::from(MESSAGE_SIZE_LIMIT),
        "serialized size is too large, size={}",
        size
    );

    let size = u32::try_from(size).unwrap();
    stream
        .write_u32(size)
        .await
        .wrap_err("failed to send message size")?;

    // TODO: use reusable buffer
    let bytes = bincode::serialize(data).wrap_err("failed to serialize message")?;
    stream
        .write_all(&bytes)
        .await
        .wrap_err("failed to write message")?;

    // TCP stream's flush is a no-op, but flush here for consistency
    stream.flush().await.wrap_err("failed to flush stream")?;

    Ok(())
}
