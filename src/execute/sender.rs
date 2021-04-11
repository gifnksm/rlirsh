use crate::{
    prelude::*,
    protocol::{self, ClientAction},
};
use tokio::{io::AsyncWrite, sync::mpsc::Receiver};
use tracing::Span;

pub(super) struct Task<W> {
    writer: W,
    rx: Receiver<ClientAction>,
}

impl<W> Task<W>
where
    W: AsyncWrite + Send + 'static,
{
    pub(super) fn new(writer: W, rx: Receiver<ClientAction>) -> Self {
        Self { writer, rx }
    }

    pub(super) fn spawn(self, span: Span) -> impl Future<Output = Result<()>> {
        tokio::spawn(self.handle().instrument(span))
            .err_into()
            .and_then(future::ready)
    }

    async fn handle(self) -> Result<()> {
        let Self { writer, mut rx } = self;
        tokio::pin!(writer);

        trace!("started");
        while let Some(message) = rx.recv().await {
            trace!(?message);
            protocol::send_message(&mut writer, &message)
                .await
                .wrap_err("failed to send message")?;
        }
        // all stream closed, notify to the server
        protocol::send_message(&mut writer, &ClientAction::Finished)
            .await
            .wrap_err("failed to send message")?;
        trace!("finished");

        Ok(())
    }
}
