use crate::{prelude::*, protocol::*, stdin::Stdin};
use argh::FromArgs;
use etc_passwd::Passwd;
use std::{
    env,
    ffi::OsString,
    fmt::Debug,
    future::Future,
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    os::unix::prelude::ExitStatusExt,
    process::Stdio,
    sync::Arc,
};
use tokio::{
    fs::File,
    io::{self, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpSocket, TcpStream},
    process::Command,
    sync::{
        mpsc::{self, Receiver, Sender},
        oneshot, Notify,
    },
};

mod prelude;
mod protocol;
mod stdin;

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
        .with(fmt::layer().with_writer(std::io::stderr))
        .with(EnvFilter::from_default_env().add_directive(Level::INFO.into()))
        .with(ErrorLayer::default())
        .init();
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    color_eyre::install()?;
    init_tracing();

    let args = argh::from_env::<Args>();
    match args.command {
        SubCommand::Server(server_args) => server_main(server_args).await?,
        SubCommand::Execute(client_args) => execute_main(client_args).await?,
    }

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

    let (stdin_msg_tx, mut stdin_msg_rx) = mpsc::channel::<SinkAction>(1);
    let stdin_msg_tx2 = stdin_msg_tx.clone();
    let (stdout_msg_tx, mut stdout_msg_rx) = mpsc::channel::<SourceAction>(1);
    let (stderr_msg_tx, mut stderr_msg_rx) = mpsc::channel::<SourceAction>(1);
    let (exit_msg_tx, mut exit_msg_rx) = mpsc::channel::<ExitStatus>(1);

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
                let message = recv_message(&mut reader).await?;
                trace!(?message);
                match message {
                    ClientAction::Stdin(action) => {
                        send_source_action(&stdin_action_tx, action)
                            .instrument(info_span!("stdin_action_tx"))
                            .await?
                    }
                    ClientAction::Stdout(action) => {
                        send_sink_action(&stdout_action_tx, action)
                            .instrument(info_span!("stdout_action_tx"))
                            .await?
                    }
                    ClientAction::Stderr(action) => {
                        send_sink_action(&stderr_action_tx, action)
                            .instrument(info_span!("stderr_action_tx"))
                            .await?
                    }
                    ClientAction::Finished => break,
                }
            }
            finish_notify2.notify_one();
            debug!("finished");
            Ok::<(), Error>(())
        }
        .instrument(info_span!("receiver")),
    )
    .err_into()
    .and_then(future::ready);

    let sender = tokio::spawn(
        async move {
            let mut writer = writer;
            debug!("started");
            loop {
                let message = tokio::select! {
                    Some(message) = stdin_msg_rx.recv() => ServerAction::Stdin(message),
                    Some(message) = stdout_msg_rx.recv() => ServerAction::Stdout(message),
                    Some(message) = stderr_msg_rx.recv() => ServerAction::Stderr(message),
                    Some(message) = exit_msg_rx.recv() => ServerAction::Exit(message),
                    else => break
                };
                trace!(?message);
                send_message(&mut writer, &message)
                    .await
                    .wrap_err("failed to send message")?;
            }
            debug!("waiting for finished");
            finish_notify.notified().await;
            send_message(&mut writer, &ServerAction::Finished)
                .await
                .wrap_err("failed to send message")?;
            debug!("finished");
            Ok::<(), Error>(())
        }
        .instrument(info_span!("sender")),
    )
    .err_into()
    .and_then(future::ready);

    let stdin_handler = tokio::spawn(
        handle_sink(stdin, stdin_msg_tx, stdin_action_rx).instrument(info_span!("stdin")),
    )
    .err_into()
    .and_then(future::ready);

    let stdout_handler = tokio::spawn(
        handle_source(stdout, stdout_msg_tx, stdout_action_rx).instrument(info_span!("stdout")),
    )
    .err_into()
    .and_then(future::ready);

    let stderr_handler = tokio::spawn(
        handle_source(stderr, stderr_msg_tx, stderr_action_rx).instrument(info_span!("stderr")),
    )
    .err_into()
    .and_then(future::ready);

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
    stdin_msg_tx2.send(SinkAction::SinkClosed).await?;
    drop(stdin_msg_tx2);
    exit_msg_tx.send(status).await?;
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
    let stdin = Stdin::new().wrap_err("failed to open stdin")?;
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

    let (stdin_msg_tx, mut stdin_msg_rx) = mpsc::channel::<SourceAction>(1);
    let (stdout_msg_tx, mut stdout_msg_rx) = mpsc::channel::<SinkAction>(1);
    let (stderr_msg_tx, mut stderr_msg_rx) = mpsc::channel::<SinkAction>(1);
    let (finish_msg_tx, mut finish_msg_rx) = mpsc::channel::<()>(1);

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
                let message = recv_message(&mut reader).await?;
                trace!(?message);
                match message {
                    ServerAction::Stdin(action) => {
                        send_sink_action(&stdin_action_tx, action)
                            .instrument(info_span!("stdin_action_tx"))
                            .await?
                    }
                    ServerAction::Stdout(action) => {
                        send_source_action(&stdout_action_tx, action)
                            .instrument(info_span!("stdout_action_tx"))
                            .await?
                    }
                    ServerAction::Stderr(action) => {
                        send_source_action(&stderr_action_tx, action)
                            .instrument(info_span!("stderr_action_tx"))
                            .await?
                    }
                    ServerAction::Exit(status) => exit_status_tx
                        .take()
                        .unwrap()
                        .send(status)
                        .map_err(|e| eyre!("failed to send exit status: {:?}", e))?,
                    ServerAction::Finished => break,
                }
            }
            debug!("finished");
            Ok::<(), Error>(())
        }
        .instrument(info_span!("receiver")),
    )
    .err_into()
    .and_then(future::ready);

    let sender = tokio::spawn(
        async move {
            let mut writer = writer;
            debug!("started");
            loop {
                let message = tokio::select! {
                    Some(message) = stdin_msg_rx.recv() => ClientAction::Stdin(message),
                    Some(message) = stdout_msg_rx.recv() => ClientAction::Stdout(message),
                    Some(message) = stderr_msg_rx.recv() => ClientAction::Stderr(message),
                    Some(()) = finish_msg_rx.recv() =>  ClientAction::Finished,
                    else => break,
                };
                trace!(?message);
                send_message(&mut writer, &message)
                    .await
                    .wrap_err("failed to send message")?;
            }
            debug!("finished");
            Ok::<(), Error>(())
        }
        .instrument(info_span!("sender")),
    )
    .err_into()
    .and_then(future::ready);

    let stdin_handler = tokio::spawn(
        handle_source(stdin, stdin_msg_tx, stdin_action_rx).instrument(info_span!("stdin")),
    )
    .err_into()
    .and_then(future::ready);

    let stdout_handler = tokio::spawn(
        handle_sink(stdout, stdout_msg_tx, stdout_action_rx).instrument(info_span!("stdout")),
    )
    .err_into()
    .and_then(future::ready);

    let stderr_handler = tokio::spawn(
        handle_sink(stderr, stderr_msg_tx, stderr_action_rx).instrument(info_span!("stderr")),
    )
    .err_into()
    .and_then(future::ready);

    let status = exit_status_rx.await?;
    debug!(?status, "exit");
    tokio::try_join!(stdin_handler, stdout_handler, stderr_handler)?;
    debug!("handle completed");
    finish_msg_tx.send(()).await?;
    drop(finish_msg_tx);

    tokio::try_join!(receiver, sender)?;

    debug!("finished");

    Ok(())
}

const BUFFER_SIZE: usize = 4096;

async fn handle_sink(
    mut writer: impl AsyncWrite + Unpin,
    msg_tx: Sender<SinkAction>,
    mut action_rx: Receiver<SourceAction>,
) -> Result<()> {
    async fn do_if_not_closed(
        is_closed: &mut bool,
        act: impl Future<Output = io::Result<()>>,
    ) -> Result<()> {
        if *is_closed {
            return Ok(());
        }

        match act.await {
            Ok(()) => Ok(()),
            Err(err) => match err.kind() {
                io::ErrorKind::BrokenPipe => {
                    *is_closed = true;
                    Ok(())
                }
                _ => Err(err.into()),
            },
        }
    }

    debug!("started");
    let mut is_closed = false;
    while let Some(message) = action_rx.recv().await {
        match message {
            SourceAction::Data(bytes) => {
                trace!(len = bytes.len(), is_closed, "received");
                do_if_not_closed(&mut is_closed, writer.write_all(&bytes)).await?;
                if !is_closed {
                    msg_tx.send(SinkAction::Ack).await?;
                }
                do_if_not_closed(&mut is_closed, writer.flush()).await?;
            }
            SourceAction::SourceClosed => break,
        }
        if is_closed {
            // send SinkClosed each time when receiving any messages from source if sink has been closed
            msg_tx.send(SinkAction::SinkClosed).await?;
        }
    }
    if let Err(err) = writer.shutdown().await {
        debug!(?err, "failed to shutdown");
    }
    debug!("finished");
    Ok(())
}

async fn handle_source(
    mut reader: impl AsyncRead + Unpin,
    msg_tx: Sender<SourceAction>,
    mut action_rx: Receiver<SinkAction>,
) -> Result<()> {
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

                let message = SourceAction::Data(buf[..size].into());
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
        .send(SourceAction::SourceClosed)
        .await
        .wrap_err("failed to send message")?;
    debug!("finished");
    Ok(())
}

async fn send_source_action(tx: &Sender<SourceAction>, action: SourceAction) -> Result<()> {
    tx.send(action).await?;
    Ok(())
}

async fn send_sink_action(tx: &Sender<SinkAction>, action: SinkAction) -> Result<()> {
    if let Err(err) = tx.send(action).await {
        // SinkClosed may be sent after the source closed, so ignore it.
        debug!(?err);
        if !matches!(err.0, SinkAction::SinkClosed) {
            bail!(err)
        }
    }
    Ok(())
}
