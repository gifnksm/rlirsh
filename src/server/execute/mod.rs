use crate::{
    prelude::*,
    protocol::{
        self, C2sStreamKind, ExecuteCommand, ExecuteRequest, ExecuteResponse, ExitStatus,
        S2cStreamKind, ServerAction, SinkAction, SourceAction,
    },
    sink, source, terminal,
};
use etc_passwd::Passwd;
use std::{
    collections::HashMap, env, ffi::OsString, os::unix::prelude::ExitStatusExt,
    os::unix::process::CommandExt, process::Command as StdCommand, process::Stdio, sync::Arc,
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

struct ServeParam {
    child: Child,
    pty_master: Option<PtyMaster>,
    stdin: Option<Box<dyn AsyncWrite + Send + Unpin>>,
    stdout: Option<Box<dyn AsyncRead + Send + Unpin>>,
    stderr: Option<Box<dyn AsyncRead + Send + Unpin>>,
}

pub(super) async fn main(mut stream: TcpStream, req: ExecuteRequest) -> Result<()> {
    info!(?req.command, "serve request");

    let res = spawn_process(req).wrap_err("failed to start process");
    let param = match res {
        Ok(res) => {
            protocol::send_message(&mut stream, &ExecuteResponse::Ok)
                .await
                .wrap_err("failed to send response")?;
            res
        }
        Err(err) => {
            protocol::send_message(&mut stream, &ExecuteResponse::Err(err.to_string()))
                .await
                .wrap_err("failed to send response")?;
            bail!(err);
        }
    };

    info!(id = ?param.child.id(), "spawned");

    serve(stream, param).await?;

    info!("serve finished");

    Ok(())
}

fn spawn_process(req: ExecuteRequest) -> Result<ServeParam> {
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

    let mut child;
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

    let stdin;
    let stdout;
    let stderr;
    if let Some(pty_master) = &pty_master {
        let pty_master = pty_master
            .try_clone()
            .wrap_err("failed to duplicate PTY master fd")?;
        let (read, write) = io::split(pty_master);
        stdin = Some(Box::new(write) as _);
        stdout = Some(Box::new(read) as _);
        stderr = None;
    } else {
        stdin = child.stdin.take().map(|x| Box::new(x) as _);
        stdout = child.stdout.take().map(|x| Box::new(x) as _);
        stderr = child.stderr.take().map(|x| Box::new(x) as _);
    }

    let param = ServeParam {
        child,
        pty_master,
        stdin,
        stdout,
        stderr,
    };

    Ok(param)
}

async fn serve(stream: TcpStream, mut param: ServeParam) -> Result<()> {
    let (reader, writer) = stream.into_split();
    let (send_msg_tx, send_msg_rx) = mpsc::channel(128);
    let finish_notify = Arc::new(Notify::new());
    let (send_error_tx, send_error_rx) = mpsc::channel(1);
    let (kill_error_tx, mut kill_error_rx) = mpsc::channel(1);

    let mut c2s_tx_map = HashMap::new();
    let mut s2c_tx_map = HashMap::new();
    let mut handlers = vec![];

    if let Some(stdin) = param.stdin {
        let kind = C2sStreamKind::Stdin;
        let (tx, task) = sink::new(stdin, send_msg_tx.clone(), move |action| {
            ServerAction::SinkAction(kind, action)
        });
        c2s_tx_map.insert(kind, tx);
        handlers.push(task.spawn(info_span!("stdin")).boxed());
    } else {
        send_msg_tx
            .send(ServerAction::SinkAction(
                C2sStreamKind::Stdin,
                SinkAction::SinkClosed,
            ))
            .await?;
    }

    if let Some(stdout) = param.stdout {
        let kind = S2cStreamKind::Stdout;
        let (tx, task) = source::new(stdout, send_msg_tx.clone(), move |action| {
            ServerAction::SourceAction(kind, action)
        });
        s2c_tx_map.insert(kind, tx);
        handlers.push(task.spawn(info_span!("stdout")).boxed());
    } else {
        send_msg_tx
            .send(ServerAction::SourceAction(
                S2cStreamKind::Stdout,
                SourceAction::SourceClosed,
            ))
            .await?;
    }

    if let Some(stderr) = param.stderr {
        let kind = S2cStreamKind::Stderr;
        let (tx, task) = source::new(stderr, send_msg_tx.clone(), move |action| {
            ServerAction::SourceAction(kind, action)
        });
        s2c_tx_map.insert(kind, tx);
        handlers.push(task.spawn(info_span!("stderr")).boxed());
    } else {
        send_msg_tx
            .send(ServerAction::SourceAction(
                S2cStreamKind::Stderr,
                SourceAction::SourceClosed,
            ))
            .await?;
    }

    let c2s_kinds = c2s_tx_map.keys().copied().collect::<Vec<_>>();
    let receiver = receiver::Task::new(
        reader,
        param.pty_master,
        c2s_tx_map,
        s2c_tx_map,
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
            for kind in c2s_kinds {
                send_msg_tx
                    .send(ServerAction::SinkAction(kind, SinkAction::SinkClosed))
                    .await
                    .wrap_err("failed to send message")?;
            }
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

    tokio::try_join!(receiver, sender, future::try_join_all(handlers))?;

    Ok(())
}
