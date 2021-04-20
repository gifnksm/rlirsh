use crate::{
    prelude::*,
    protocol::{ClientAction, WindowSize},
    terminal,
};
use std::sync::Arc;
use tokio::{
    signal::unix::Signal,
    sync::{mpsc::Sender, Notify},
};
use tracing::Span;

#[derive(Debug)]
pub(super) struct Task {
    stream: Signal,
    send_msg_tx: Sender<ClientAction>,
    exit_notify: Arc<Notify>,
}

impl Task {
    pub(super) fn new(
        stream: Signal,
        send_msg_tx: Sender<ClientAction>,
        exit_notify: Arc<Notify>,
    ) -> Self {
        Self {
            stream,
            send_msg_tx,
            exit_notify,
        }
    }

    pub(super) fn spawn(self, span: Span) -> impl Future<Output = Result<()>> {
        tokio::spawn(self.handle().instrument(span))
            .err_into()
            .and_then(future::ready)
    }

    async fn handle(self) -> Result<()> {
        let Self {
            mut stream,
            send_msg_tx,
            exit_notify,
        } = self;

        trace!("started");
        loop {
            tokio::select! {
                Some(()) = stream.recv() => {}
                () = exit_notify.notified() => break,
            }
            trace!("window change signal received");
            let (width, height) = match terminal::get_window_size(&libc::STDIN_FILENO) {
                Ok(size) => size,
                Err(err) => {
                    warn!(?err, "failed to get window size");
                    continue;
                }
            };
            debug!(?width, ?height, "window size updated");
            send_msg_tx
                .send(ClientAction::WindowSizeChange(WindowSize { width, height }))
                .await?;
        }
        trace!("finished");

        Ok(())
    }
}
