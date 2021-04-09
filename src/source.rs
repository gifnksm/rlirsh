use crate::{
    prelude::*,
    protocol::{SinkAction, SourceAction, MAX_STREAM_PACKET_SIZE},
};
use std::fmt::Debug;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    sync::mpsc,
};
use tracing::Span;

pub(crate) fn new<R, T, F>(
    reader: R,
    tx: mpsc::Sender<T>,
    from_action: F,
) -> (Sender, Source<R, T, F>) {
    let (sink_tx, rx) = mpsc::channel(1);
    (
        Sender(sink_tx),
        Source {
            reader,
            tx,
            rx,
            from_action,
        },
    )
}

#[derive(Debug, Clone)]
pub(crate) struct Sender(mpsc::Sender<SinkAction>);

impl Sender {
    pub(crate) async fn send(&self, action: SinkAction) -> Result<()> {
        if let Err(err) = self.0.send(action).await {
            // SinkClosed may be sent after the source closed, so ignore it.
            debug!(?err);
            if !matches!(err.0, SinkAction::SinkClosed) {
                bail!(err)
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct Source<R, T, F> {
    reader: R,
    tx: mpsc::Sender<T>,
    rx: mpsc::Receiver<SinkAction>,
    from_action: F,
}

impl<R, T, F> Source<R, T, F>
where
    R: AsyncRead + Send + 'static,
    T: Debug + Send + Sync + 'static,
    F: Fn(SourceAction) -> T + Send + Sync + 'static,
{
    pub(crate) fn spawn(self, span: Span) -> impl Future<Output = Result<()>> {
        tokio::spawn(self.handle().instrument(span))
            .err_into()
            .and_then(future::ready)
    }

    async fn handle(self) -> Result<()> {
        let Self {
            reader,
            tx,
            mut rx,
            from_action,
        } = self;
        tokio::pin!(reader);

        let mut buf = vec![0; MAX_STREAM_PACKET_SIZE];
        trace!("started");
        loop {
            tokio::select! {
                size = reader.read(&mut buf) => {
                    let size = size.wrap_err("failed to receive message")?;
                    if size == 0 {
                        break;
                    }
                    trace!(%size, "bytes read");

                    let message = SourceAction::Data(buf[..size].into());
                    tx
                        .send(from_action(message))
                        .await
                        .wrap_err("failed to send message")?;
                    let message = rx.recv().await.unwrap();
                    trace!(?message);
                    match message {
                        SinkAction::Ack => {}
                        SinkAction::SinkClosed => break,
                    }
                }
                message = rx.recv() => {
                    let message = message.unwrap();
                    trace!(?message);
                    match message {
                        SinkAction::Ack => panic!("invalid message received"),
                        SinkAction::SinkClosed => break,
                    }
                }
            };
        }
        tx.send(from_action(SourceAction::SourceClosed))
            .await
            .wrap_err("failed to send message")?;
        trace!("finished");
        Ok(())
    }
}