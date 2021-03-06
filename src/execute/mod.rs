use crate::{
    net::SocketAddrs,
    parse,
    prelude::*,
    protocol::{
        self, ExecuteCommand, ExecuteRequest, ExitStatus, PortId, PtyParam, Request, Response,
        StreamId, WindowSize,
    },
    stdin::Stdin,
    stream::{listener, sink, source, RecvRouter},
    terminal::{self, raw_mode},
};
use clap::{AppSettings, Clap};
use nix::libc;
use std::{env, fmt::Debug, sync::Arc};
use tokio::{
    fs::File,
    net::TcpListener,
    signal::unix::{signal, SignalKind},
    sync::{broadcast, mpsc, oneshot},
};

mod receiver;
mod sender;
mod window_change;

/// Execute command
#[derive(Debug, Clap)]
#[clap(setting = AppSettings::TrailingVarArg)]
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

    /// Specifies that connections to the given TCP port on the local (client) host are
    /// to be forwarded to the given host and port on the remote side.
    ///
    /// Format: [bind_address]:port:host:host_port
    ///
    /// This works by allocating a socket to listen to either a TCP port on the local side,
    /// optionally bound to the specified `bind_address`.
    /// Whenever a connection is made to the local port or socket, the connection is forwarded
    /// over the insecure channel, and a connection is made to either `host` port `host_port` from the remote machine.
    #[clap(short = 'L', number_of_values(1), parse(try_from_str = parse::local_port_forward_specifier))]
    local_port_forward_specifier: Vec<(SocketAddrs, String)>,

    /// A server host and port to connect
    #[clap(name = "addr", parse(try_from_str))]
    addrs: SocketAddrs,

    /// Commands to execute on a remote host
    command: Vec<String>,
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
    local_listeners: Vec<TcpListener>,
}

pub(super) async fn main(mut args: Args) -> Result<i32> {
    trace!(?args);

    let (local_listeners, remote_connect_addrs) = prepare_local_forwarding(&mut args)?;
    let (host, req) = create_request(args, remote_connect_addrs);

    let param = ServeParam {
        allocate_pty: req.pty_param.is_some(),
        handle_stdin: true,
        handle_stdout: true,
        handle_stderr: true,
        local_listeners,
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

    let status = serve(stream, param).await.wrap_err("internal error")?;

    debug!(?status, "command finished");

    let exit_code = match status {
        ExitStatus::Code(code) => code,
        ExitStatus::Signal(num) => 128 + num,
    };
    Ok(exit_code)
}

fn prepare_local_forwarding(args: &mut Args) -> Result<(Vec<TcpListener>, Vec<String>)> {
    let mut local = vec![];
    let mut remote = vec![];
    for (local_addrs, remote_addr) in args.local_port_forward_specifier.drain(..) {
        let listener = local_addrs
            .listen(1024)
            .wrap_err("failed to listen on the local address")?;
        local.push(listener);
        remote.push(remote_addr);
    }
    Ok((local, remote))
}

fn create_request(args: Args, connect_addrs: Vec<String>) -> (SocketAddrs, ExecuteRequest) {
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
        args.addrs,
        ExecuteRequest {
            command,
            envs,
            pty_param,
            connect_addrs,
        },
    )
}

async fn connect(addrs: SocketAddrs, req: ExecuteRequest) -> Result<tokio::net::TcpStream> {
    let mut stream = addrs
        .connect()
        .await
        .wrap_err("failed to connect to the server")?;
    debug!("connected");

    protocol::send_message(&mut stream, &Request::Execute(req))
        .await
        .wrap_err("failed to send request")?;

    let response: Response = protocol::recv_message(&mut stream)
        .await
        .wrap_err("failed to receive response")?;

    let () = Result::<()>::from(response).wrap_err("received error response from the server")?;

    Ok(stream)
}

async fn serve(stream: tokio::net::TcpStream, param: ServeParam) -> Result<ExitStatus> {
    let (reader, writer) = stream.into_split();
    let (send_msg_tx, send_msg_rx) = mpsc::channel(128);
    let (exit_status_tx, exit_status_rx) = oneshot::channel();
    let (error_tx, error_rx) = mpsc::channel(1);
    let (task_end_tx, task_end_rx) = broadcast::channel(1);

    let window_change_stream = param
        .allocate_pty
        .then(|| signal(SignalKind::window_change()))
        .transpose()
        .wrap_err("failed to set signal handler")?;

    let recv_router = Arc::new(RecvRouter::new());
    let mut stdio_handlers = vec![];

    if param.handle_stdin {
        let id = StreamId::Stdin;
        let stdin = Stdin::new().wrap_err("failed to open stdin")?;
        let task = source::Task::new(id, stdin, send_msg_tx.clone(), &recv_router);
        stdio_handlers.push(task.spawn(info_span!("stdin")).boxed());
    }

    if param.handle_stdout {
        let id = StreamId::Stdout;
        let stdout = File::create("/dev/stdout")
            .await
            .wrap_err("failed to open stdout")?;
        let task = sink::Task::new(id, stdout, send_msg_tx.clone(), &recv_router);
        stdio_handlers.push(task.spawn(info_span!("stdout")).boxed());
    }

    if param.handle_stderr {
        let id = StreamId::Stderr;
        let stderr = File::create("/dev/stderr")
            .await
            .wrap_err("failed to open stderr")?;
        let task = sink::Task::new(id, stderr, send_msg_tx.clone(), &recv_router);
        stdio_handlers.push(task.spawn(info_span!("stderr")).boxed());
    }

    if let Some(stream) = window_change_stream {
        let task = window_change::Task::new(stream, send_msg_tx.clone(), task_end_tx.subscribe());
        stdio_handlers.push(task.spawn(info_span!("window_change_signal")).boxed());
    }

    for (port_id, local_listener) in (0..).map(PortId::new).zip(param.local_listeners) {
        let local_addr = local_listener.local_addr()?;
        let task = listener::Task::new(
            port_id,
            local_listener,
            send_msg_tx.clone(),
            task_end_tx.subscribe(),
            recv_router.clone(),
        );
        let _ = task.spawn(info_span!("listener", %local_addr));
    }

    let receiver = receiver::Task::new(reader, recv_router, exit_status_tx, error_rx)
        .spawn(info_span!("receiver"));
    let sender = sender::Task::new(writer, send_msg_rx, error_tx).spawn(info_span!("sender"));
    drop(send_msg_tx);
    drop(task_end_rx);

    let status = exit_status_rx.await??;
    if let Err(err) = task_end_tx.send(()) {
        warn!(?err, "failed to notify task end")
    }
    future::try_join_all(stdio_handlers).await?;
    raw_mode::leave().wrap_err("failed to leave from raw mode")?;
    tokio::try_join!(receiver, sender)?;

    Ok(status)
}
