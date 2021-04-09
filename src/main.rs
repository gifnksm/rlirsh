use crate::{prelude::*, protocol::*, stdin::Stdin};
use argh::FromArgs;
use etc_passwd::Passwd;
use std::{
    collections::HashMap,
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
    net::{TcpSocket, TcpStream},
    process::Command,
    sync::{mpsc, oneshot, Notify},
};

mod prelude;
mod protocol;
mod sink;
mod source;
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
    let (send_msg_tx, send_msg_rx) = mpsc::channel(128);
    let finish_notify = Arc::new(Notify::new());
    let finish_notify2 = finish_notify.clone();

    let mut c2s_tx_map = HashMap::new();
    let mut s2c_tx_map = HashMap::new();
    let mut handlers = vec![];

    if let Some(stdin) = child.stdin.take() {
        let kind = C2sStreamKind::Stdin;
        let (tx, task) = sink::new(stdin, send_msg_tx.clone(), move |action| {
            ServerAction::SinkAction(kind, action)
        });
        c2s_tx_map.insert(kind, tx);
        handlers.push(task.spawn(info_span!("stdin")).boxed());
    }
    if let Some(stdout) = child.stdout.take() {
        let kind = S2cStreamKind::Stdout;
        let (tx, task) = source::new(stdout, send_msg_tx.clone(), move |action| {
            ServerAction::SourceAction(kind, action)
        });
        s2c_tx_map.insert(kind, tx);
        handlers.push(task.spawn(info_span!("stdout")).boxed());
    }
    if let Some(stderr) = child.stderr.take() {
        let kind = S2cStreamKind::Stderr;
        let (tx, task) = source::new(stderr, send_msg_tx.clone(), move |action| {
            ServerAction::SourceAction(kind, action)
        });
        s2c_tx_map.insert(kind, tx);
        handlers.push(task.spawn(info_span!("stderr")).boxed());
    }

    let c2s_kinds = c2s_tx_map.keys().copied().collect::<Vec<_>>();

    let receiver = tokio::spawn(
        async move {
            let mut reader = reader;
            trace!("started");
            loop {
                let message = recv_message(&mut reader).await?;
                trace!(?message);
                match message {
                    ClientAction::SourceAction(kind, action) => {
                        let tx = c2s_tx_map
                            .get(&kind)
                            .ok_or_else(|| eyre!("tx not found: {:?}", kind))?;
                        tx.send(action)
                            .instrument(info_span!("source", ?kind))
                            .await?
                    }
                    ClientAction::SinkAction(kind, action) => {
                        let tx = s2c_tx_map
                            .get(&kind)
                            .ok_or_else(|| eyre!("tx not found: {:?}", kind))?;
                        tx.send(action)
                            .instrument(info_span!("sink", ?kind))
                            .await?
                    }
                    ClientAction::Finished => break,
                }
            }
            finish_notify2.notify_one();
            trace!("finished");
            Ok::<(), Error>(())
        }
        .instrument(info_span!("receiver")),
    )
    .err_into()
    .and_then(future::ready);

    let sender = tokio::spawn(
        async move {
            let mut writer = writer;
            let mut send_msg_rx = send_msg_rx;
            trace!("started");
            // send messages to server until all stream closed
            while let Some(message) = send_msg_rx.recv().await {
                trace!(?message);
                send_message(&mut writer, &message)
                    .await
                    .wrap_err("failed to send message")?;
            }
            // all stream closed, waiting for receiver task finished
            finish_notify.notified().await;
            // receiver closed, notify to the client
            send_message(&mut writer, &ServerAction::Finished)
                .await
                .wrap_err("failed to send message")?;
            trace!("finished");
            Ok::<(), Error>(())
        }
        .instrument(info_span!("sender")),
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
    info!(?status, "process finished");
    // Notifies the client that the standard input pipe connected to the child process is closed.
    // It should be triggered by the HUP of the pipe, but the current version of tokio (1.4)
    // does not support such an operation, so send a message to the client to alternate it.
    for kind in c2s_kinds {
        send_msg_tx
            .send(ServerAction::SinkAction(kind, SinkAction::SinkClosed))
            .await
            .wrap_err("failed to send message")?;
    }
    send_msg_tx.send(ServerAction::Exit(status)).await?;
    drop(send_msg_tx);

    tokio::try_join!(receiver, sender, future::try_join_all(handlers))?;
    info!("serve finished");

    Ok(())
}

async fn execute_main(args: ExecuteArgs) -> Result<()> {
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

    let send_stdin = true;
    let send_stdout = true;
    let send_stderr = true;

    let (reader, writer) = stream.into_split();
    let (send_msg_tx, send_msg_rx) = mpsc::channel(128);

    let mut c2s_tx_map = HashMap::new();
    let mut s2c_tx_map = HashMap::new();
    let mut handlers = vec![];

    if send_stdin {
        let kind = C2sStreamKind::Stdin;
        let stdin = Stdin::new().wrap_err("failed to open stdin")?;
        let (tx, task) = source::new(stdin, send_msg_tx.clone(), move |action| {
            ClientAction::SourceAction(kind, action)
        });
        c2s_tx_map.insert(kind, tx);
        handlers.push(task.spawn(info_span!("stdin")).boxed());
    }
    if send_stdout {
        let kind = S2cStreamKind::Stdout;
        let stdout = File::create("/dev/stdout")
            .await
            .wrap_err("failed to open stdout")?;
        let (tx, task) = sink::new(stdout, send_msg_tx.clone(), move |action| {
            ClientAction::SinkAction(kind, action)
        });
        s2c_tx_map.insert(kind, tx);
        handlers.push(task.spawn(info_span!("stdout")).boxed());
    }
    if send_stderr {
        let kind = S2cStreamKind::Stderr;
        let stderr = File::create("/dev/stderr")
            .await
            .wrap_err("failed to open stderr")?;
        let (tx, task) = sink::new(stderr, send_msg_tx.clone(), move |action| {
            ClientAction::SinkAction(kind, action)
        });
        s2c_tx_map.insert(kind, tx);
        handlers.push(task.spawn(info_span!("stderr")).boxed());
    }
    let (exit_status_tx, exit_status_rx) = oneshot::channel::<ExitStatus>();
    drop(send_msg_tx);

    let receiver = tokio::spawn(
        async move {
            let mut reader = reader;
            let mut exit_status_tx = Some(exit_status_tx);
            trace!("started");
            loop {
                let message = recv_message(&mut reader).await?;
                trace!(?message);
                match message {
                    ServerAction::SourceAction(kind, action) => {
                        let tx = s2c_tx_map
                            .get(&kind)
                            .ok_or_else(|| eyre!("tx not found: {:?}", kind))?;
                        tx.send(action)
                            .instrument(info_span!("source", ?kind))
                            .await?
                    }
                    ServerAction::SinkAction(kind, action) => {
                        let tx = c2s_tx_map
                            .get(&kind)
                            .ok_or_else(|| eyre!("tx not found: {:?}", kind))?;
                        tx.send(action)
                            .instrument(info_span!("sink", ?kind))
                            .await?
                    }
                    ServerAction::Exit(status) => exit_status_tx
                        .take()
                        .ok_or_else(|| eyre!("received exit status multiple times"))?
                        .send(status)
                        .map_err(|e| eyre!("failed to send exit status: {:?}", e))?,
                    ServerAction::Finished => break,
                }
            }
            trace!("finished");
            Ok::<(), Error>(())
        }
        .instrument(info_span!("receiver")),
    )
    .err_into()
    .and_then(future::ready);

    let sender = tokio::spawn(
        async move {
            let mut writer = writer;
            let mut send_msg_rx = send_msg_rx;
            trace!("started");
            while let Some(message) = send_msg_rx.recv().await {
                trace!(?message);
                send_message(&mut writer, &message)
                    .await
                    .wrap_err("failed to send message")?;
            }
            // all stream closed, notify to the server
            send_message(&mut writer, &ClientAction::Finished)
                .await
                .wrap_err("failed to send message")?;
            trace!("finished");
            Ok::<(), Error>(())
        }
        .instrument(info_span!("sender")),
    )
    .err_into()
    .and_then(future::ready);

    let status = exit_status_rx.await?;
    debug!(?status, "exit");

    tokio::try_join!(receiver, sender, future::try_join_all(handlers))?;
    debug!("finished");

    Ok(())
}
