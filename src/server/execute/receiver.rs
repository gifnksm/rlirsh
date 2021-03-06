use crate::{
    prelude::*,
    protocol::{self, ClientAction},
    stream::RecvRouter,
    terminal,
};
use std::sync::Arc;
use tokio::{
    io::AsyncRead,
    sync::{
        mpsc::{Receiver, Sender},
        Notify,
    },
};
use tokio_pty_command::PtyMaster;
use tracing::Span;

#[derive(Debug)]
pub(super) struct Task<R> {
    reader: R,
    pty_master: Option<PtyMaster>,
    recv_router: Arc<RecvRouter>,
    finish_notify: Arc<Notify>,
    send_error_rx: Receiver<Error>,
    kill_error_tx: Sender<Error>,
}

impl<R> Task<R>
where
    R: AsyncRead + Send + 'static,
{
    pub(super) fn new(
        reader: R,
        pty_master: Option<PtyMaster>,
        recv_router: Arc<RecvRouter>,
        finish_notify: Arc<Notify>,
        send_error_rx: Receiver<Error>,
        kill_error_tx: Sender<Error>,
    ) -> Self {
        Self {
            reader,
            pty_master,
            recv_router,
            finish_notify,
            send_error_rx,
            kill_error_tx,
        }
    }

    pub(super) fn spawn(self, span: Span) -> impl Future<Output = Result<()>> {
        tokio::spawn(self.handle().instrument(span))
            .err_into()
            .and_then(future::ready)
    }

    async fn handle(self) -> Result<()> {
        let Self {
            reader,
            pty_master,
            recv_router,
            finish_notify,
            mut send_error_rx,
            kill_error_tx,
        } = self;
        tokio::pin!(reader);

        trace!("started");
        let mut receive_failed = None;
        loop {
            let res = tokio::select! {
                res = protocol::recv_message(&mut reader) => res,
                Some(res) = send_error_rx.recv() => Err(res), // error reported from sender task
            };
            let message = match res {
                Ok(message) => message,
                Err(err) => {
                    debug!(?err);
                    receive_failed = Some(err);
                    break;
                }
            };
            trace!(?message);
            match message {
                ClientAction::Stream(action) => recv_router.send_stream_action(action).await?,
                ClientAction::WindowSizeChange(ws) => {
                    if let Some(pty_master) = &pty_master {
                        if let Err(err) = terminal::set_window_size(pty_master, ws.width, ws.height)
                        {
                            warn!(?err, "failed to set window size");
                        }
                    } else {
                        warn!("PTY is not allocated");
                    }
                }
                ClientAction::Finished => break,
            }
        }
        if let Some(err) = receive_failed {
            // If error occurred, shutdown all handlers on this process
            recv_router.shutdown().await;
            if let Err(err) = kill_error_tx
                .send(err.wrap_err("connection disconnected unexpectedly"))
                .await
            {
                debug!(?err);
            }
        }
        finish_notify.notify_one();
        trace!("finished");

        Ok(())
    }
}
