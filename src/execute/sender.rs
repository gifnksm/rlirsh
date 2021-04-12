use crate::{
    prelude::*,
    protocol::{self, ClientAction},
};
use tokio::{
    io::AsyncWrite,
    sync::mpsc::{Receiver, Sender},
};
use tracing::Span;

#[derive(Debug)]
pub(super) struct Task<W> {
    writer: W,
    rx: Receiver<ClientAction>,
    error_tx: Sender<Error>,
}

impl<W> Task<W>
where
    W: AsyncWrite + Send + 'static,
{
    pub(super) fn new(writer: W, rx: Receiver<ClientAction>, error_tx: Sender<Error>) -> Self {
        Self {
            writer,
            rx,
            error_tx,
        }
    }

    pub(super) fn spawn(self, span: Span) -> impl Future<Output = Result<()>> {
        tokio::spawn(self.handle().instrument(span))
            .err_into()
            .and_then(future::ready)
    }

    async fn handle(self) -> Result<()> {
        let Self {
            writer,
            mut rx,
            error_tx,
        } = self;
        tokio::pin!(writer);

        // When error occurred during sending message, stop sending the messages and waiting for
        // all handler being closed for clean shutdown
        let mut send_failed = false;

        trace!("started");
        while let Some(message) = rx.recv().await {
            trace!(?message);
            do_if_not_failed(
                &mut send_failed,
                &error_tx,
                protocol::send_message(&mut writer, &message)
                    .map_err(|err| err.wrap_err("failed to send message to server")),
            )
            .await;
        }
        // all stream closed, notify to the server
        do_if_not_failed(
            &mut send_failed,
            &error_tx,
            protocol::send_message(&mut writer, &ClientAction::Finished)
                .map_err(|err| err.wrap_err("failed to send message to server")),
        )
        .await;
        trace!("finished");

        Ok(())
    }
}

async fn do_if_not_failed(
    is_failed: &mut bool,
    error_tx: &Sender<Error>,
    act: impl Future<Output = Result<()>>,
) {
    if *is_failed {
        return;
    }

    if let Err(err) = act.await {
        debug!(?err);
        if let Err(err) = error_tx.send(err).await.wrap_err("failed to report error") {
            debug!(?err);
        }
        *is_failed = true;
    }
}
