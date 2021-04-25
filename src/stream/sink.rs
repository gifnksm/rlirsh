use crate::{
    prelude::*,
    protocol::{SinkAction, SourceAction, StreamAction, StreamId},
};
use std::fmt::Debug;
use tokio::{
    io::{self, AsyncWrite, AsyncWriteExt},
    sync::mpsc,
};
use tracing::Span;

#[derive(Debug, Clone)]
pub(crate) struct Sender(mpsc::Sender<SourceAction>);

impl Sender {
    pub(crate) async fn send(&self, action: SourceAction) -> Result<()> {
        self.0.send(action).await?;
        Ok(())
    }

    pub(crate) async fn shutdown(&self) -> Result<()> {
        self.send(SourceAction::SourceClosed)
            .await
            .wrap_err("failed to shutdown")
    }
}

#[derive(Debug)]
pub(crate) struct Task<W, T> {
    id: StreamId,
    writer: W,
    tx: mpsc::Sender<T>,
    rx: mpsc::Receiver<SourceAction>,
}

impl<W, T> Task<W, T>
where
    W: AsyncWrite + Send + 'static,
    T: Debug + Send + Sync + From<(StreamId, StreamAction)> + 'static,
{
    pub(crate) fn new(id: StreamId, writer: W, tx: mpsc::Sender<T>) -> (Sender, Self) {
        let (source_tx, rx) = mpsc::channel(1);
        (Sender(source_tx), Self { id, writer, tx, rx })
    }

    pub(crate) fn spawn(self, span: Span) -> impl Future<Output = Result<()>> {
        tokio::spawn(self.handle().instrument(span))
            .err_into()
            .and_then(future::ready)
    }

    async fn handle(self) -> Result<()> {
        let Self {
            id,
            writer,
            tx,
            mut rx,
        } = self;
        tokio::pin!(writer);

        trace!("started");
        let mut is_closed = false;
        while let Some(message) = rx.recv().await {
            match message {
                SourceAction::Data(bytes) => {
                    trace!(len = bytes.len(), is_closed, "received");
                    do_if_not_closed(&mut is_closed, writer.write_all(&bytes))
                        .instrument(info_span!("write"))
                        .await?;
                    if !is_closed {
                        let msg = (id, SinkAction::Ack.into()).into();
                        tx.send(msg).await?;
                    }
                    do_if_not_closed(&mut is_closed, writer.flush())
                        .instrument(info_span!("flush"))
                        .await?;
                }
                SourceAction::SourceClosed => break,
            }
            if is_closed {
                // send SinkClosed each time when receiving any messages from source if sink has been closed
                let msg = (id, SinkAction::SinkClosed.into()).into();
                tx.send(msg).await?;
            }
        }
        if let Err(err) = writer.shutdown().await {
            debug!(?err, "failed to shutdown");
        }
        trace!("finished");
        Ok(())
    }
}

async fn do_if_not_closed(
    is_closed: &mut bool,
    act: impl Future<Output = io::Result<()>>,
) -> Result<()> {
    if *is_closed {
        trace!("already closed");
        return Ok(());
    }

    match act.await {
        Ok(()) => Ok(()),
        Err(err) => match err.kind() {
            io::ErrorKind::BrokenPipe => {
                debug!("connection closed");
                *is_closed = true;
                Ok(())
            }
            _ => {
                warn!(?err, "unexpected error occurred");
                Err(err.into())
            }
        },
    }
}
