use crate::{
    prelude::*,
    protocol::{ClientAction, WindowSize},
    terminal,
};
use tokio::{
    signal::unix::Signal,
    sync::{broadcast, mpsc},
};
use tracing::Span;

#[derive(Debug)]
pub(super) struct Task {
    stream: Signal,
    send_msg_tx: mpsc::Sender<ClientAction>,
    task_end_rx: broadcast::Receiver<()>,
}

impl Task {
    pub(super) fn new(
        stream: Signal,
        send_msg_tx: mpsc::Sender<ClientAction>,
        task_end_rx: broadcast::Receiver<()>,
    ) -> Self {
        Self {
            stream,
            send_msg_tx,
            task_end_rx,
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
            mut task_end_rx,
        } = self;

        trace!("started");
        loop {
            tokio::select! {
                Some(()) = stream.recv() => {}
                res = task_end_rx.recv() => {
                    if let Err(err) = res {
                        warn!(?err, "failed to receive task end signal");
                    }
                    break;
                },
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
