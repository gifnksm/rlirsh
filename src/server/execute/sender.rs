use crate::{
    prelude::*,
    protocol::{self, ServerAction},
};
use std::sync::Arc;
use tokio::{
    io::AsyncWrite,
    sync::{mpsc::Receiver, Notify},
};
use tracing::Span;

pub(super) struct Task<W> {
    writer: W,
    rx: Receiver<ServerAction>,
    finish_notify: Arc<Notify>,
}

impl<W> Task<W>
where
    W: AsyncWrite + Send + 'static,
{
    pub(super) fn new(writer: W, rx: Receiver<ServerAction>, finish_notify: Arc<Notify>) -> Self {
        Self {
            writer,
            rx,
            finish_notify,
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
            finish_notify,
        } = self;
        tokio::pin!(writer);

        trace!("started");
        // send messages to server until all stream closed
        while let Some(message) = rx.recv().await {
            trace!(?message);
            protocol::send_message(&mut writer, &message)
                .await
                .wrap_err("failed to send message")?;
        }
        // all stream closed, waiting for receiver task finished
        finish_notify.notified().await;
        // receiver closed, notify to the client
        protocol::send_message(&mut writer, &ServerAction::Finished)
            .await
            .wrap_err("failed to send message")?;
        trace!("finished");

        Ok(())
    }
}
