use crate::prelude::*;
use argh::FromArgs;
use etc_passwd::Passwd;
use serde::{Deserialize, Serialize};
use std::{
    convert::TryFrom,
    env,
    ffi::OsString,
    fmt::Debug,
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    os::unix::prelude::ExitStatusExt,
    process::Stdio,
    sync::Arc,
};
use tokio::{
    fs::File,
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpSocket, TcpStream},
    process::Command,
    sync::{
        mpsc::{self, Receiver, Sender},
        oneshot, Notify,
    },
};

mod prelude;

/// Rootless insecure remote shell
#[derive(Debug, FromArgs)]
struct Args {
    #[argh(subcommand)]
    command: SubCommand,
}

#[derive(Debug, FromArgs)]
#[argh(subcommand)]
enum SubCommand {
    Server(ServerArgs),
    Execute(ExecuteArgs),
}

/// Launch rlirsh server
#[derive(Debug, FromArgs)]
#[argh(subcommand, name = "server")]
struct ServerArgs {
    /// alternative bind address [default: 0.0.0.0]
    #[argh(option, default = "std::net::Ipv4Addr::new(0, 0, 0, 0).into()")]
    bind: IpAddr,
    /// port number to bind
    #[argh(positional)]
    port: u16,
}

/// Execute command
#[derive(Debug, FromArgs)]
#[argh(subcommand, name = "exec")]
struct ExecuteArgs {
    /// server host and port to connect
    #[argh(positional, from_str_fn(parse_addr))]
    host: SocketAddr,
    /// command to execute on remote host
    #[argh(positional)]
    cmd: String,
    /// command arguments
    #[argh(positional)]
    args: Vec<String>,
}

fn parse_addr(s: &str) -> Result<SocketAddr, String> {
    s.to_socket_addrs()
        .map_err(|e| e.to_string())?
        .next()
        .ok_or_else(|| "failed to lookup address information".into())
}

fn init_tracing() {
    use tracing::Level;
    use tracing_error::ErrorLayer;
    use tracing_subscriber::{fmt, EnvFilter};
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env().add_directive(Level::INFO.into()))
        .with(ErrorLayer::default())
        .init();
}

fn main() -> Result<()> {
    color_eyre::install()?;
    init_tracing();

    let args = argh::from_env::<Args>();

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    runtime.block_on(async {
        match args.command {
            SubCommand::Server(server_args) => server_main(server_args).await.unwrap(),
            SubCommand::Execute(client_args) => execute_main(client_args).await.unwrap(),
        }
    });

    // Workaround for problems that do not terminate the program.
    // Reading standard input may be blocking the termination of the program.
    runtime.shutdown_background();

    Ok(())
}

async fn server_main(args: ServerArgs) -> Result<()> {
    let addr = SocketAddr::new(args.bind, args.port);
    let socket = if addr.is_ipv4() {
        TcpSocket::new_v4()
    } else {
        TcpSocket::new_v6()
    }?;
    socket.set_reuseaddr(true)?;
    socket.bind(addr)?;
    let listener = socket.listen(1024)?;
    info!("start listening on {:?}", addr);
    loop {
        match listener.accept().await {
            Ok((stream, peer_addr)) => {
                info!(%peer_addr, "connected");
                tokio::spawn(
                    async move {
                        if let Err(err) = serve(stream).await {
                            warn!(?err, "serve failed");
                        }
                    }
                    .instrument(info_span!("serve", %peer_addr)),
                );
            }
            Err(err) => {
                warn!(?err, "accept failed");
                continue;
            }
        };
    }
}

async fn serve(mut stream: TcpStream) -> Result<()> {
    let request = recv_message(&mut stream)
        .await
        .wrap_err("failed to receive request")?;
    match request {
        Request::Execute(req) => serve_execute(stream, req).await?,
    }
    Ok(())
}

async fn serve_execute(mut stream: TcpStream, req: ExecuteRequest) -> Result<()> {
    info!(?req.cmd, ?req.args, "serve request");
    let shell = if let Some(passwd) = Passwd::current_user()? {
        OsString::from(passwd.shell.to_str()?)
    } else if let Some(shell) = env::var_os("SHELL") {
        shell
    } else {
        bail!("cannot get login shell for the user")
    };
    let cmd = if req.args.is_empty() {
        req.cmd.clone()
    } else {
        req.cmd.clone() + " " + &req.args.join(" ")
    };
    let child = Command::new(&shell)
        .arg("-c")
        .arg(cmd)
        .env_clear()
        .envs(req.envs.iter().map(|(a, b)| (a, b)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn();

    let mut child = match child {
        Ok(child) => {
            send_message(&mut stream, &ExecuteResponse::Ok)
                .await
                .wrap_err("failed to send response")?;
            child
        }
        Err(err) => {
            send_message(&mut stream, &ExecuteResponse::Err(err.to_string()))
                .await
                .wrap_err("failed to send response")?;
            bail!(err);
        }
    };

    info!(id = ?child.id(), "spawned");

    let (reader, writer) = stream.into_split();

    let (msg_tx, mut msg_rx) = mpsc::channel::<ServerAction>(128);
    let stdin_msg_tx = msg_tx.clone();
    let stdout_msg_tx = msg_tx.clone();
    let stderr_msg_tx = msg_tx.clone();
    let exit_msg_tx = msg_tx;

    let (stdin_action_tx, stdin_action_rx) = mpsc::channel::<SourceAction>(1);
    let (stdout_action_tx, stdout_action_rx) = mpsc::channel::<SinkAction>(1);
    let (stderr_action_tx, stderr_action_rx) = mpsc::channel::<SinkAction>(1);

    let finish_notify = Arc::new(Notify::new());
    let finish_notify2 = finish_notify.clone();

    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    let receiver = tokio::spawn(
        async move {
            let mut reader = reader;
            debug!("started");
            loop {
                let message = recv_message(&mut reader).await.unwrap();
                trace!(?message);
                match message {
                    ClientAction::Stdin(action) => stdin_action_tx.send(action).await.unwrap(),
                    ClientAction::Stdout(action) => stdout_action_tx.send(action).await.unwrap(),
                    ClientAction::Stderr(action) => stderr_action_tx.send(action).await.unwrap(),
                    ClientAction::Finished => break,
                }
            }
            finish_notify2.notify_one();
            debug!("finished");
        }
        .instrument(info_span!("receiver")),
    );

    let sender = tokio::spawn(
        async move {
            let mut writer = writer;
            debug!("started");
            while let Some(message) = msg_rx.recv().await {
                trace!(?message);
                send_message(&mut writer, &message)
                    .await
                    .wrap_err("failed to send message")
                    .unwrap();
            }
            debug!("waiting for finished");
            finish_notify.notified().await;
            send_message(&mut writer, &ServerAction::Finished)
                .await
                .wrap_err("failed to send message")
                .unwrap();
            debug!("finished");
        }
        .instrument(info_span!("sender")),
    );

    let stdin_handler = tokio::spawn(
        async move {
            handle_sink(stdin, stdin_msg_tx, stdin_action_rx, ServerAction::Stdin)
                .await
                .unwrap();
        }
        .instrument(info_span!("stdin")),
    );

    let stdout_handler = tokio::spawn(
        async move {
            handle_source(
                stdout,
                stdout_msg_tx,
                stdout_action_rx,
                ServerAction::Stdout,
            )
            .await
            .unwrap();
        }
        .instrument(info_span!("stdout")),
    );

    let stderr_handler = tokio::spawn(
        async move {
            handle_source(
                stderr,
                stderr_msg_tx,
                stderr_action_rx,
                ServerAction::Stderr,
            )
            .await
            .unwrap();
        }
        .instrument(info_span!("stderr")),
    );

    let status = child.wait().await?;
    let status = if let Some(code) = status.code() {
        ExitStatus::Code(code)
    } else if let Some(signal) = status.signal() {
        ExitStatus::Signal(signal)
    } else {
        unreachable!()
    };
    // Notifies the client that the standard input pipe connected to the child process is closed.
    // It should be triggered by the HUP of the pipe, but the current version of tokio (1.4)
    // does not support such an operation, so send a message to the client to alternate it.
    exit_msg_tx
        .send(ServerAction::Stdin(SinkAction::SinkClosed))
        .await?;
    exit_msg_tx.send(ServerAction::Exit(status)).await?;
    drop(exit_msg_tx);

    tokio::try_join!(
        receiver,
        sender,
        stdin_handler,
        stdout_handler,
        stderr_handler,
    )?;

    debug!("finished");

    Ok(())
}

async fn execute_main(args: ExecuteArgs) -> Result<()> {
    let stdin = File::open("/dev/stdin")
        .await
        .wrap_err("failed to open stdin")?;
    let stdout = File::create("/dev/stdout")
        .await
        .wrap_err("failed to open stdout")?;
    let stderr = File::create("/dev/stderr")
        .await
        .wrap_err("failed to open stderr")?;

    let socket = if args.host.is_ipv4() {
        TcpSocket::new_v4()
    } else {
        TcpSocket::new_v6()
    }?;

    debug!(%args.host, "connect");

    let mut stream = socket.connect(args.host).await?;
    let req = Request::Execute(ExecuteRequest {
        cmd: args.cmd,
        args: args.args,
        envs: vec![],
    });
    send_message(&mut stream, &req)
        .await
        .wrap_err("failed to send request")?;
    let response = recv_message(&mut stream)
        .await
        .wrap_err("failed to receive response")?;
    if let ExecuteResponse::Err(message) = response {
        bail!(message);
    }
    debug!("command started");

    let (reader, writer) = stream.into_split();

    let (msg_tx, mut msg_rx) = mpsc::channel::<ClientAction>(128);
    let stdin_msg_tx = msg_tx.clone();
    let stdout_msg_tx = msg_tx.clone();
    let stderr_msg_tx = msg_tx.clone();
    let finish_msg_tx = msg_tx;

    let (stdin_action_tx, stdin_action_rx) = mpsc::channel::<SinkAction>(1);
    let (stdout_action_tx, stdout_action_rx) = mpsc::channel::<SourceAction>(1);
    let (stderr_action_tx, stderr_action_rx) = mpsc::channel::<SourceAction>(1);

    let (exit_status_tx, exit_status_rx) = oneshot::channel::<ExitStatus>();

    let receiver = tokio::spawn(
        async move {
            let mut reader = reader;
            let mut exit_status_tx = Some(exit_status_tx);
            debug!("started");
            loop {
                let message = recv_message(&mut reader).await.unwrap();
                trace!(?message);
                match message {
                    ServerAction::Stdin(action) => stdin_action_tx.send(action).await.unwrap(),
                    ServerAction::Stdout(action) => stdout_action_tx.send(action).await.unwrap(),
                    ServerAction::Stderr(action) => stderr_action_tx.send(action).await.unwrap(),
                    ServerAction::Exit(status) => {
                        exit_status_tx.take().unwrap().send(status).unwrap()
                    }
                    ServerAction::Finished => break,
                }
            }
            debug!("finished");
        }
        .instrument(info_span!("receiver")),
    );

    let sender = tokio::spawn(
        async move {
            let mut writer = writer;
            debug!("started");
            while let Some(message) = msg_rx.recv().await {
                trace!(?message);
                send_message(&mut writer, &message)
                    .await
                    .wrap_err("failed to send message")
                    .unwrap();
            }
            debug!("finished");
        }
        .instrument(info_span!("sender")),
    );

    let stdin_handler = tokio::spawn(
        async move {
            handle_source(stdin, stdin_msg_tx, stdin_action_rx, ClientAction::Stdin)
                .await
                .unwrap()
        }
        .instrument(info_span!("stdin")),
    );

    let stdout_handler = tokio::spawn(
        async move {
            handle_sink(
                stdout,
                stdout_msg_tx,
                stdout_action_rx,
                ClientAction::Stdout,
            )
            .await
            .unwrap();
        }
        .instrument(info_span!("stdout")),
    );

    let stderr_handler = tokio::spawn(
        async move {
            handle_sink(
                stderr,
                stderr_msg_tx,
                stderr_action_rx,
                ClientAction::Stderr,
            )
            .await
            .unwrap();
        }
        .instrument(info_span!("stderr")),
    );

    let status = exit_status_rx.await?;
    debug!(?status, "exit");
    tokio::try_join!(stdin_handler, stdout_handler, stderr_handler)?;
    debug!("handle completed");
    finish_msg_tx.send(ClientAction::Finished).await?;
    drop(finish_msg_tx);

    tokio::try_join!(receiver, sender)?;

    debug!("finished");

    Ok(())
}

const MESSAGE_SIZE_LIMIT: u32 = 1024 * 1024; // 1MiB

async fn recv_message<T>(stream: &mut (impl AsyncRead + Unpin)) -> Result<T>
where
    T: for<'a> Deserialize<'a> + 'static,
{
    let size = stream
        .read_u32()
        .await
        .wrap_err("failed to receive message size")?;
    ensure!(
        size <= MESSAGE_SIZE_LIMIT,
        "message size is too large, size={}",
        size
    );
    let mut bytes = vec![0; size as usize];
    stream
        .read_exact(&mut bytes)
        .await
        .wrap_err("failed to receive message")?;
    let data = bincode::deserialize(&bytes).wrap_err("failed to deserialize message")?;
    Ok(data)
}

async fn send_message<T>(stream: &mut (impl AsyncWrite + Unpin), data: &T) -> Result<()>
where
    T: Serialize,
{
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

    let bytes = bincode::serialize(data).wrap_err("failed to serialize message")?;
    stream
        .write_all(&bytes)
        .await
        .wrap_err("failed to write message")?;
    Ok(())
}

async fn handle_sink<T>(
    mut writer: impl AsyncWrite + Unpin,
    msg_tx: Sender<T>,
    mut action_rx: Receiver<SourceAction>,
    from_action: impl Fn(SinkAction) -> T,
) -> Result<()>
where
    T: Send + Sync + Debug + 'static,
{
    debug!("started");
    while let Some(message) = action_rx.recv().await {
        match message {
            SourceAction::Data(bytes) => {
                use std::io::ErrorKind;
                trace!(len = bytes.len(), "received");
                let msg = match writer.write_all(&bytes).await {
                    Ok(()) => SinkAction::Ack,
                    Err(e) => match e.kind() {
                        ErrorKind::BrokenPipe => {
                            debug!("pipe closed");
                            SinkAction::SinkClosed
                        }
                        _ => bail!(e),
                    },
                };
                msg_tx.send(from_action(msg)).await?;
            }
            SourceAction::SourceClosed => break,
        }
    }
    debug!("finished");
    Ok(())
}

async fn handle_source<T>(
    mut reader: impl AsyncRead + Unpin,
    msg_tx: Sender<T>,
    mut action_rx: Receiver<SinkAction>,
    from_action: impl Fn(SourceAction) -> T,
) -> Result<()>
where
    T: Send + Sync + Debug + 'static,
{
    let mut buf = vec![0; BUFFER_SIZE];
    debug!("started");
    loop {
        tokio::select! {
            size = reader.read(&mut buf) => {
                let size = size.wrap_err("failed to receive message")?;
                if size == 0 {
                    break;
                }
                trace!(%size, "bytes read");

                let message = from_action(SourceAction::Data(buf[..size].into()));
                msg_tx
                    .send(message)
                    .await
                    .wrap_err("failed to send message")?;
                let message = action_rx.recv().await.unwrap();
                trace!(?message);
                match message {
                    SinkAction::Ack => {}
                    SinkAction::SinkClosed => break,
                }
            }
            message = action_rx.recv() => {
                let message = message.unwrap();
                trace!(?message);
                match message {
                    SinkAction::Ack => panic!("invalid message received"),
                    SinkAction::SinkClosed => break,
                }
            }
        };
    }
    msg_tx
        .send(from_action(SourceAction::SourceClosed))
        .await
        .wrap_err("failed to send message")?;
    debug!("finished");
    Ok(())
}

#[derive(Debug, Deserialize, Serialize)]
enum Request {
    Execute(ExecuteRequest),
}

#[derive(Debug, Deserialize, Serialize)]
struct ExecuteRequest {
    cmd: String,
    args: Vec<String>,
    envs: Vec<(String, String)>,
}

#[derive(Debug, Deserialize, Serialize)]
enum ExecuteResponse {
    Ok,
    Err(String),
}

const BUFFER_SIZE: usize = 4096;

#[derive(Debug, Copy, Clone, Deserialize, Serialize)]
enum ExitStatus {
    Code(i32),
    Signal(i32),
}

#[derive(Debug, Deserialize, Serialize)]
enum ServerAction {
    Stdin(SinkAction),
    Stdout(SourceAction),
    Stderr(SourceAction),
    Exit(ExitStatus),
    Finished,
}

#[derive(Debug, Deserialize, Serialize)]
enum ClientAction {
    Stdin(SourceAction),
    Stdout(SinkAction),
    Stderr(SinkAction),
    Finished,
}

#[derive(Debug, Deserialize, Serialize)]
enum SourceAction {
    Data(Vec<u8>),
    SourceClosed,
}

#[derive(Debug, Deserialize, Serialize)]
enum SinkAction {
    Ack,
    SinkClosed,
}
