use crate::{
    prelude::*,
    protocol::{
        self, C2sStreamKind, ClientAction, ExecuteCommand, ExecuteRequest, ExecuteResponse,
        ExitStatus, PtyParam, Request, S2cStreamKind, WindowSize,
    },
    sink, source,
    stdin::Stdin,
    terminal::{self, raw_mode},
};
use clap::Clap;
use nix::libc;
use std::{
    collections::HashMap,
    env,
    fmt::Debug,
    net::{SocketAddr, ToSocketAddrs},
    sync::Arc,
};
use tokio::{
    fs::File,
    net::TcpSocket,
    signal::unix::{signal, SignalKind},
    sync::{mpsc, oneshot, Notify},
};

mod receiver;
mod sender;
mod window_change;

/// Execute command
#[derive(Debug, Clap)]
pub(super) struct Args {
    /// Disable pseudo-terminal allocation.
    #[clap(name = "disable-pty", short = 'T', overrides_with = "force-enable-pty")]
    disable_pty: bool,

    /// Force pseudo-terminal allocation.
    ///
    /// This can be used to execute arbitrary screen-based programs on a remote machine,
    /// which can be very useful, e.g. when implementing menu services.
    ///
    /// Multiple `-t` options force tty allocation, even if `rlirsh` has no local tty.
    #[clap(
        name = "force-enable-pty",
        short = 't',
        overrides_with = "disable-pty",
        parse(from_occurrences)
    )]
    force_enable_pty: u32,

    /// A server host and port to connect
    #[clap(parse(try_from_str = parse_addr))]
    host: SocketAddr,

    /// Commands to execute on a remote host
    command: Vec<String>,
}

fn parse_addr(s: &str) -> Result<SocketAddr, String> {
    s.to_socket_addrs()
        .map_err(|e| e.to_string())?
        .next()
        .ok_or_else(|| "failed to lookup address information".into())
}

#[derive(Debug)]
enum PtyMode {
    Auto,
    Disable,
    Enable,
}

struct ServeParam {
    allocate_pty: bool,
    handle_stdin: bool,
    handle_stdout: bool,
    handle_stderr: bool,
}

pub(super) async fn main(args: Args) -> Result<i32> {
    trace!(?args);
    let (host, req) = create_request(args);

    let param = ServeParam {
        allocate_pty: req.pty_param.is_some(),
        handle_stdin: true,
        handle_stdout: true,
        handle_stderr: true,
    };

    let stream = connect(host, req).await?;

    debug!("command started");

    raw_mode::leave_on_panic();
    let _raw_mode = param
        .allocate_pty
        .then(raw_mode::enter_scoped)
        .transpose()
        .unwrap_or_else(|err| {
            warn!(?err, "failed to enter raw mote");
            None
        });

    let status = serve(stream, &param).await.wrap_err("internal error")?;

    debug!(?status, "command finished");

    let exit_code = match status {
        ExitStatus::Code(code) => code,
        ExitStatus::Signal(num) => 128 + num,
    };
    Ok(exit_code)
}

fn create_request(args: Args) -> (SocketAddr, ExecuteRequest) {
    let pty_mode = if args.disable_pty {
        assert_eq!(args.force_enable_pty, 0);
        PtyMode::Disable
    } else if args.force_enable_pty > 0 {
        PtyMode::Enable
    } else {
        PtyMode::Auto
    };

    let mut allocate_pty = match pty_mode {
        PtyMode::Auto => args.command.is_empty(),
        PtyMode::Disable => false,
        PtyMode::Enable => true,
    };

    let command = if args.command.is_empty() {
        ExecuteCommand::LoginShell
    } else {
        ExecuteCommand::Program {
            command: args.command,
        }
    };

    let has_local_tty = terminal::has_tty();
    if args.force_enable_pty < 2 && allocate_pty && !has_local_tty {
        warn!("Pseudo-terminal will not be allocated because stdin is not a terminal.");
        allocate_pty = false;
    }

    let pty_param;
    let mut envs = vec![];
    if allocate_pty {
        let (width, height) =
            terminal::get_window_size(&libc::STDIN_FILENO).unwrap_or_else(|err| {
                warn!(?err, "failed to get window size");
                (80, 60)
            });
        pty_param = Some(PtyParam {
            window_size: WindowSize { width, height },
        });
        if let Ok(term) = env::var("TERM") {
            envs.push(("TERM".into(), term));
        }
    } else {
        pty_param = None;
    };

    (
        args.host,
        ExecuteRequest {
            command,
            envs,
            pty_param,
        },
    )
}

async fn connect(host: SocketAddr, req: ExecuteRequest) -> Result<tokio::net::TcpStream> {
    let socket = if host.is_ipv4() {
        TcpSocket::new_v4()
    } else {
        TcpSocket::new_v6()
    }?;

    let mut stream = socket
        .connect(host)
        .await
        .wrap_err("failed to connect to the server")?;

    protocol::send_message(&mut stream, &Request::Execute(req))
        .await
        .wrap_err("failed to send request")?;

    let response = protocol::recv_message(&mut stream)
        .await
        .wrap_err("failed to receive response")?;

    if let ExecuteResponse::Err(message) = response {
        bail!(eyre!(message).wrap_err("failed to execute the program"));
    }

    Ok(stream)
}

async fn serve(stream: tokio::net::TcpStream, param: &ServeParam) -> Result<ExitStatus> {
    let (reader, writer) = stream.into_split();
    let (send_msg_tx, send_msg_rx) = mpsc::channel(128);
    let (exit_status_tx, exit_status_rx) = oneshot::channel();
    let (error_tx, error_rx) = mpsc::channel(1);
    let exit_notify = Arc::new(Notify::new());

    let window_change_stream = param
        .allocate_pty
        .then(|| signal(SignalKind::window_change()))
        .transpose()
        .wrap_err("failed to set signal handler")?;

    let mut c2s_tx_map = HashMap::new();
    let mut s2c_tx_map = HashMap::new();
    let mut handlers = vec![];

    if param.handle_stdin {
        let kind = C2sStreamKind::Stdin;
        let stdin = Stdin::new().wrap_err("failed to open stdin")?;
        let (tx, task) = source::new(stdin, send_msg_tx.clone(), move |action| {
            ClientAction::SourceAction(kind, action)
        });
        c2s_tx_map.insert(kind, tx);
        handlers.push(task.spawn(info_span!("stdin")).boxed());
    }

    if param.handle_stdout {
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

    if param.handle_stderr {
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

    if let Some(stream) = window_change_stream {
        let task = window_change::Task::new(stream, send_msg_tx.clone(), exit_notify.clone());
        handlers.push(task.spawn(info_span!("window_change_signal")).boxed());
    }

    let receiver = receiver::Task::new(reader, c2s_tx_map, s2c_tx_map, exit_status_tx, error_rx)
        .spawn(info_span!("receiver"));
    let sender = sender::Task::new(writer, send_msg_rx, error_tx).spawn(info_span!("sender"));
    drop(send_msg_tx);

    let status = exit_status_rx.await??;
    exit_notify.notify_waiters();
    tokio::try_join!(receiver, sender, future::try_join_all(handlers))?;

    Ok(status)
}
