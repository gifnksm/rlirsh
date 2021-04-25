use crate::prelude::*;
use serde::{Deserialize, Serialize};
use std::{convert::TryFrom, fmt};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct SerialError(Vec<String>);

impl SerialError {
    pub(crate) fn new(err: &Error) -> Self {
        Self(err.chain().map(|err| err.to_string()).collect())
    }
}

impl From<SerialError> for Error {
    fn from(from: SerialError) -> Self {
        let mut err: Option<Error> = None;
        for msg in from.0.into_iter().rev() {
            err = Some(match err {
                Some(e) => e.wrap_err(msg),
                None => eyre!(msg),
            });
        }
        err.unwrap_or_else(|| eyre!("failed to deserialize an error information"))
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) enum Response {
    Ok,
    Err(SerialError),
}

impl From<Response> for Result<()> {
    fn from(res: Response) -> Self {
        match res {
            Response::Ok => Ok(()),
            Response::Err(err) => Err(err.into()),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) enum Request {
    Execute(ExecuteRequest),
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct ExecuteRequest {
    pub(crate) command: ExecuteCommand,
    pub(crate) envs: Vec<(String, String)>,
    pub(crate) pty_param: Option<PtyParam>,
    pub(crate) connect_addrs: Vec<String>,
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

#[derive(Debug, Copy, Clone, Deserialize, Serialize)]
pub(crate) enum ExitStatus {
    Code(i32),
    Signal(i32),
}

#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) enum StreamId {
    Stdin,
    Stdout,
    Stderr,
}

#[derive(Debug, Deserialize, Serialize, From)]
pub(crate) enum StreamAction {
    SourceAction(SourceAction),
    SinkAction(SinkAction),
}

#[derive(Debug, Deserialize, Serialize, From)]
pub(crate) enum ServerAction {
    StreamAction(StreamId, StreamAction),
    ConnecterAction(PortId, ConnecterAction),
    Exit(ExitStatus),
    Finished,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub(crate) struct PortId(u32);

impl PortId {
    pub(crate) fn new(id: u32) -> Self {
        Self(id)
    }
}

#[derive(Debug, Deserialize, Serialize, From)]
pub(crate) enum ClientAction {
    StreamAction(StreamId, StreamAction),
    WindowSizeChange(WindowSize),
    ListenerAction(PortId, ListenerAction),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub(crate) struct ConnId(u32);

impl ConnId {
    pub(crate) fn new(id: u32) -> Self {
        Self(id)
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) enum ListenerAction {
    Connect(ConnId),
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) enum ConnecterAction {
    ConnectResponse(ConnId, Response),
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
