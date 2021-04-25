use crate::{
    prelude::*,
    protocol::{SinkAction, SourceAction, StreamAction, StreamId, MAX_STREAM_PACKET_SIZE},
    stream::RecvRouter,
};
use std::fmt::Debug;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    sync::mpsc,
};
use tracing::Span;

#[derive(Debug, Clone)]
pub(crate) struct Sender(mpsc::Sender<SinkAction>);

impl Sender {
    pub(crate) async fn send(&self, action: SinkAction) -> Result<()> {
        if let Err(err) = self.0.send(action).await {
            // SinkAction::{Ack, SinkClosed} may be sent after the source closed, so ignore it.
            debug!(?err);
        }
        Ok(())
    }

    pub(crate) async fn shutdown(&self) -> Result<()> {
        self.send(SinkAction::SinkClosed)
            .await
            .wrap_err("failed to shutdown")
    }
}

#[derive(Debug)]
pub(crate) struct Task<R, T> {
    id: StreamId,
    reader: R,
    tx: mpsc::Sender<T>,
    rx: mpsc::Receiver<SinkAction>,
}

impl<R, T> Task<R, T>
where
    R: AsyncRead + Send + 'static,
    T: Debug + Send + Sync + From<StreamAction> + 'static,
{
    pub(crate) fn new(
        id: StreamId,
        reader: R,
        tx: mpsc::Sender<T>,
        recv_router: &RecvRouter,
    ) -> Self {
        let (sink_tx, rx) = mpsc::channel(1);
        recv_router.insert_source_tx(id, Sender(sink_tx));
        Self { id, reader, tx, rx }
    }

    pub(crate) fn spawn(self, span: Span) -> impl Future<Output = Result<()>> {
        tokio::spawn(self.handle().instrument(span))
            .err_into()
            .and_then(future::ready)
    }

    async fn handle(self) -> Result<()> {
        let Self {
            id,
            reader,
            tx,
            mut rx,
        } = self;
        tokio::pin!(reader);

        let mut buf = vec![0; MAX_STREAM_PACKET_SIZE];
        trace!("started");
        loop {
            tokio::select! {
                size = reader.read(&mut buf) => {
                    let size = match size {
                        Ok(0) => {
                            debug!("pipe closed");
                            break
                        },
                        Ok(n) => n,
                        Err(err) => {
                            debug!(?err, "error occurred while reading the pipe");
                            break;
                        }
                    };
                    trace!(%size, "bytes read");

                    let msg = T::from((id, SourceAction::Data(buf[..size].into())).into());
                    tx
                        .send(msg)
                        .await
                        .wrap_err("failed to send message")?;
                    let message = rx.recv().await.ok_or_else(|| eyre!("failed to receive message"))?;
                    trace!(?message);
                    match message {
                        SinkAction::Ack => {}
                        SinkAction::SinkClosed => break,
                    }
                }
                message = rx.recv() => {
                    let message = message.ok_or_else(|| eyre!("failed to receive message"))?;
                    trace!(?message);
                    match message {
                        SinkAction::Ack => panic!("invalid message received"),
                        SinkAction::SinkClosed => break,
                    }
                }
            };
        }
        let msg = T::from((id, SourceAction::SourceClosed).into());
        tx.send(msg).await.wrap_err("failed to send message")?;
        trace!("finished");
        Ok(())
    }
}
