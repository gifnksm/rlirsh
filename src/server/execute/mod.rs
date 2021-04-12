use crate::{
    prelude::*,
    protocol::{
        self, C2sStreamKind, ExecuteRequest, ExecuteResponse, ExitStatus, S2cStreamKind,
        ServerAction, SinkAction,
    },
    sink, source,
};
use etc_passwd::Passwd;
use std::{
    collections::HashMap, env, ffi::OsString, os::unix::prelude::ExitStatusExt, process::Stdio,
    sync::Arc,
};
use tokio::{
    net::TcpStream,
    process::Command,
    sync::{mpsc, Notify},
};

mod receiver;
mod sender;

pub(super) async fn serve(mut stream: TcpStream, req: ExecuteRequest) -> Result<()> {
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
            protocol::send_message(&mut stream, &ExecuteResponse::Ok)
                .await
                .wrap_err("failed to send response")?;
            child
        }
        Err(err) => {
            protocol::send_message(&mut stream, &ExecuteResponse::Err(err.to_string()))
                .await
                .wrap_err("failed to send response")?;
            bail!(err);
        }
    };

    info!(id = ?child.id(), "spawned");

    let (reader, writer) = stream.into_split();
    let (send_msg_tx, send_msg_rx) = mpsc::channel(128);
    let finish_notify = Arc::new(Notify::new());
    let (send_error_tx, send_error_rx) = mpsc::channel(1);
    let (kill_error_tx, mut kill_error_rx) = mpsc::channel(1);

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
    let receiver = receiver::Task::new(
        reader,
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
        status = child.wait() => {
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
            // does not support such an operation, so send a message to the client to alternate it.
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
            if let Err(err) = child.kill().await {
                warn!(?err, "failed to kill the process");
            }
        }
    };
    drop(send_msg_tx);

    tokio::try_join!(receiver, sender, future::try_join_all(handlers))?;
    info!("serve finished");

    Ok(())
}
