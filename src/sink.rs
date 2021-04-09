use crate::{
    prelude::*,
    protocol::{SinkAction, SourceAction},
};
use std::fmt::Debug;
use tokio::{
    io::{self, AsyncWrite, AsyncWriteExt},
    sync::mpsc,
};
use tracing::Span;

pub(crate) fn new<W, T, F>(
    writer: W,
    tx: mpsc::Sender<T>,
    from_action: F,
) -> (Sender, Sink<W, T, F>) {
    let (source_tx, rx) = mpsc::channel(1);
    (
        Sender(source_tx),
        Sink {
            writer,
            tx,
            rx,
            from_action,
        },
    )
}

#[derive(Debug, Clone)]
pub(crate) struct Sender(mpsc::Sender<SourceAction>);

impl Sender {
    pub(crate) async fn send(&self, action: SourceAction) -> Result<()> {
        self.0.send(action).await?;
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct Sink<W, T, F> {
    writer: W,
    tx: mpsc::Sender<T>,
    rx: mpsc::Receiver<SourceAction>,
    from_action: F,
}

impl<W, T, F> Sink<W, T, F>
where
    W: AsyncWrite + Send + 'static,
    T: Debug + Send + Sync + 'static,
    F: Fn(SinkAction) -> T + Send + Sync + 'static,
{
    pub(crate) fn spawn(self, span: Span) -> impl Future<Output = Result<()>> {
        tokio::spawn(self.handle().instrument(span))
            .err_into()
            .and_then(future::ready)
    }

    async fn handle(self) -> Result<()> {
        let Self {
            writer,
            tx,
            mut rx,
            from_action,
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
                        tx.send(from_action(SinkAction::Ack)).await?;
                    }
                    do_if_not_closed(&mut is_closed, writer.flush())
                        .instrument(info_span!("flush"))
                        .await?;
                }
                SourceAction::SourceClosed => break,
            }
            if is_closed {
                // send SinkClosed each time when receiving any messages from source if sink has been closed
                tx.send(from_action(SinkAction::SinkClosed)).await?;
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