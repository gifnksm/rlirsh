use crate::{
    prelude::*,
    protocol::{
        self, C2sStreamKind, ClientAction, ExecuteRequest, ExecuteResponse, ExitStatus, Request,
        S2cStreamKind,
    },
    sink, source,
    stdin::Stdin,
};
use clap::Clap;
use std::{
    collections::HashMap,
    fmt::Debug,
    net::{SocketAddr, ToSocketAddrs},
};
use tokio::{
    fs::File,
    net::TcpSocket,
    sync::{mpsc, oneshot},
};

mod receiver;
mod sender;

/// Execute command
#[derive(Debug, Clap)]
pub(super) struct Args {
    /// server host and port to connect
    #[clap(parse(try_from_str = parse_addr))]
    host: SocketAddr,
    /// command to execute on remote host
    cmd: String,
    /// command arguments
    args: Vec<String>,
}

fn parse_addr(s: &str) -> Result<SocketAddr, String> {
    s.to_socket_addrs()
        .map_err(|e| e.to_string())?
        .next()
        .ok_or_else(|| "failed to lookup address information".into())
}

pub(super) async fn main(args: Args) -> Result<()> {
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
    protocol::send_message(&mut stream, &req)
        .await
        .wrap_err("failed to send request")?;
    let response = protocol::recv_message(&mut stream)
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
    let (exit_status_tx, exit_status_rx) = oneshot::channel();
    let (error_tx, error_rx) = mpsc::channel(1);

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
    let receiver = receiver::Task::new(reader, c2s_tx_map, s2c_tx_map, exit_status_tx, error_rx)
        .spawn(info_span!("receiver"));
    let sender = sender::Task::new(writer, send_msg_rx, error_tx).spawn(info_span!("sender"));
    drop(send_msg_tx);

    let status = exit_status_rx.await?;
    tokio::try_join!(receiver, sender, future::try_join_all(handlers))?;
    debug!("finished");

    let exit_code = match status {
        Ok(status) => {
            debug!(?status);
            match status {
                ExitStatus::Code(code) => code,
                ExitStatus::Signal(num) => 128 + num,
            }
        }
        Err(err) => {
            warn!(?err);
            255
        }
    };
    std::process::exit(exit_code);
}
