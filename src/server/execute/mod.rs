use crate::{
    net::SocketAddrs,
    prelude::*,
    protocol::{
        self, ExecuteCommand, ExecuteRequest, ExitStatus, PortId, Response, ServerAction,
        SinkAction, SourceAction, StreamAction, StreamId,
    },
    stream::{connecter, sink, source, RecvRouter},
    terminal,
};
use etc_passwd::Passwd;
use std::{
    env, ffi::OsString, os::unix::prelude::ExitStatusExt, os::unix::process::CommandExt,
    process::Command as StdCommand, process::Stdio, sync::Arc,
};
use tokio::{
    io::{self, AsyncRead, AsyncWrite},
    net::TcpStream,
    process::{Child, Command},
    sync::{mpsc, Notify},
};
use tokio_pty_command::{CommandExt as _, PtyMaster};

mod receiver;
mod sender;

type BoxedWriter = Box<dyn AsyncWrite + Send + Unpin>;
type BoxedReader = Box<dyn AsyncRead + Send + Unpin>;

struct ServeParam {
    child: Child,
    pty_master: Option<PtyMaster>,
    stdin: Option<BoxedWriter>,
    stdout: Option<BoxedReader>,
    stderr: Option<BoxedReader>,
    connect_addrs: Vec<SocketAddrs>,
}

pub(super) async fn main(mut stream: TcpStream, req: ExecuteRequest) -> Result<()> {
    info!(?req.command, "serve request");

    let res = handle_request(req);
    let resp = Response::new(&res);
    protocol::send_message(&mut stream, &resp)
        .await
        .wrap_err("failed to send response")?;
    let param = res?;

    info!(id = ?param.child.id(), "spawned");

    serve(stream, param).await?;

    info!("serve finished");

    Ok(())
}

fn handle_request(mut req: ExecuteRequest) -> Result<ServeParam> {
    let connect_addrs = prepare_local_forwarding(req.connect_addrs.drain(..))
        .wrap_err("failed to prepare for local port forwarding")?;
    let (mut child, pty_master) = spawn_process(req).wrap_err("failed to start process")?;
    let (stdin, stdout, stderr) =
        bind_stdio(&mut child, &pty_master).wrap_err("failed to start process")?;
    let param = ServeParam {
        child,
        pty_master,
        stdin,
        stdout,
        stderr,
        connect_addrs,
    };
    Ok(param)
}

fn prepare_local_forwarding(
    connect_addrs: impl IntoIterator<Item = String>,
) -> Result<Vec<SocketAddrs>> {
    connect_addrs.into_iter().map(|addr| addr.parse()).collect()
}

fn spawn_process(req: ExecuteRequest) -> Result<(Child, Option<PtyMaster>)> {
    let shell = if let Some(passwd) = Passwd::current_user().ok().flatten() {
        OsString::from(passwd.shell.to_str()?)
    } else if let Some(shell) = env::var_os("SHELL") {
        shell
    } else {
        bail!("cannot get login shell for the user")
    };

    let mut arg0 = OsString::from("-");
    arg0.push(&shell);

    let mut builder = StdCommand::new(&shell);
    builder.arg0(arg0);
    match req.command {
        ExecuteCommand::LoginShell => {}
        ExecuteCommand::Program { command } => {
            builder.arg("-c");
            builder.arg(command.join(" "));
        }
    }

    builder.env_clear();
    builder.envs(req.envs.iter().map(|(a, b)| (a, b)));

    let mut builder = Command::from(builder);
    builder.kill_on_drop(true);

    let child;
    let pty_master;
    if let Some(param) = req.pty_param {
        let pty = PtyMaster::open().wrap_err("failed to open pty master")?;
        child = builder
            .spawn_with_pty(&pty)
            .wrap_err("failed to spawn process")?;
        let ws = &param.window_size;
        terminal::set_window_size(&pty, ws.width, ws.height)
            .wrap_err("failed to set window size")?;
        pty_master = Some(pty);
    } else {
        builder
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        child = builder.spawn().wrap_err("failed to spawn process")?;
        pty_master = None;
    }

    Ok((child, pty_master))
}

fn bind_stdio(
    child: &mut Child,
    pty_master: &Option<PtyMaster>,
) -> Result<(
    Option<BoxedWriter>,
    Option<BoxedReader>,
    Option<BoxedReader>,
)> {
    if let Some(pty_master) = &pty_master {
        let pty_master = pty_master
            .try_clone()
            .wrap_err("failed to duplicate PTY master fd")?;
        let (read, write) = io::split(pty_master);
        Ok((Some(Box::new(write) as _), Some(Box::new(read) as _), None))
    } else {
        Ok((
            child.stdin.take().map(|x| Box::new(x) as _),
            child.stdout.take().map(|x| Box::new(x) as _),
            child.stderr.take().map(|x| Box::new(x) as _),
        ))
    }
}

async fn serve(stream: TcpStream, mut param: ServeParam) -> Result<()> {
    let (reader, writer) = stream.into_split();
    let (send_msg_tx, send_msg_rx) = mpsc::channel::<ServerAction>(128);
    let finish_notify = Arc::new(Notify::new());
    let (send_error_tx, send_error_rx) = mpsc::channel(1);
    let (kill_error_tx, mut kill_error_rx) = mpsc::channel(1);

    let recv_router = Arc::new(RecvRouter::new());
    let mut stdio_handlers = vec![];

    {
        let id = StreamId::Stdin;
        if let Some(stdin) = param.stdin {
            let task = sink::Task::new(id, stdin, send_msg_tx.clone(), &recv_router);
            stdio_handlers.push(task.spawn(info_span!("stdin")).boxed());
        } else {
            send_msg_tx
                .send(StreamAction::from((id, SinkAction::SinkClosed)).into())
                .await?;
        }
    }
    {
        let id = StreamId::Stdout;
        if let Some(stdout) = param.stdout {
            let task = source::Task::new(id, stdout, send_msg_tx.clone(), &recv_router);
            stdio_handlers.push(task.spawn(info_span!("stdout")).boxed());
        } else {
            send_msg_tx
                .send(StreamAction::from((id, SourceAction::SourceClosed)).into())
                .await?;
        }
    }
    {
        let id = StreamId::Stderr;
        if let Some(stderr) = param.stderr {
            let task = source::Task::new(id, stderr, send_msg_tx.clone(), &recv_router);
            stdio_handlers.push(task.spawn(info_span!("stderr")).boxed());
        } else {
            send_msg_tx
                .send(StreamAction::from((id, SourceAction::SourceClosed)).into())
                .await?;
        }
    }

    for (port_id, connect_addr) in (0..).map(PortId::new).zip(param.connect_addrs) {
        let send_msg_tx = send_msg_tx.clone();
        let span = info_span!("connecter", %connect_addr);
        let task = connecter::Task::new(
            port_id,
            connect_addr,
            send_msg_tx.clone(),
            recv_router.clone(),
        );
        let _ = task.spawn(span);
    }

    let receiver = receiver::Task::new(
        reader,
        param.pty_master,
        recv_router.clone(),
        finish_notify.clone(),
        send_error_rx,
        kill_error_tx,
    )
    .spawn(info_span!("receiver"));
    let sender = sender::Task::new(writer, send_msg_rx, finish_notify, send_error_tx)
        .spawn(info_span!("sender"));

    tokio::select! {
        status = param.child.wait() => {
            let status = status?;
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
            // does not support such an operation [1], so send a message to the client to alternate it.
            // [1]: https://github.com/tokio-rs/tokio/issues/3467
            send_msg_tx
                .send(StreamAction::from((StreamId::Stdin, SinkAction::SinkClosed)).into())
                .await
                .wrap_err("failed to send message")?;
            send_msg_tx.send(ServerAction::Exit(status)).await?;
        }
        Some(err) = kill_error_rx.recv() => {
            warn!(?err);
            if let Err(err) = param.child.kill().await {
                warn!(?err, "failed to kill the process");
            }
        }
    };
    drop(send_msg_tx);

    tokio::try_join!(receiver, sender, future::try_join_all(stdio_handlers))?;

    Ok(())
}
