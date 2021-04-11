use crate::{
    prelude::*,
    protocol::{self, C2sStreamKind, ExitStatus, S2cStreamKind, ServerAction},
    sink, source,
};
use std::collections::HashMap;
use tokio::{io::AsyncRead, sync::oneshot};
use tracing::Span;

pub(super) struct Task<R> {
    reader: R,
    c2s_tx_map: HashMap<C2sStreamKind, source::Sender>,
    s2c_tx_map: HashMap<S2cStreamKind, sink::Sender>,
    exit_status_tx: oneshot::Sender<ExitStatus>,
}

impl<R> Task<R>
where
    R: AsyncRead + Send + 'static,
{
    pub(super) fn new(
        reader: R,
        c2s_tx_map: HashMap<C2sStreamKind, source::Sender>,
        s2c_tx_map: HashMap<S2cStreamKind, sink::Sender>,
        exit_status_tx: oneshot::Sender<ExitStatus>,
    ) -> Self {
        Self {
            reader,
            c2s_tx_map,
            s2c_tx_map,
            exit_status_tx,
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
            c2s_tx_map,
            s2c_tx_map,
            exit_status_tx,
        } = self;
        tokio::pin!(reader);
        let mut exit_status_tx = Some(exit_status_tx);

        trace!("started");
        loop {
            let message = protocol::recv_message(&mut reader).await?;
            trace!(?message);
            match message {
                ServerAction::SourceAction(kind, action) => {
                    let tx = s2c_tx_map
                        .get(&kind)
                        .ok_or_else(|| eyre!("tx not found: {:?}", kind))?;
                    tx.send(action)
                        .instrument(info_span!("source", ?kind))
                        .await?
                }
                ServerAction::SinkAction(kind, action) => {
                    let tx = c2s_tx_map
                        .get(&kind)
                        .ok_or_else(|| eyre!("tx not found: {:?}", kind))?;
                    tx.send(action)
                        .instrument(info_span!("sink", ?kind))
                        .await?
                }
                ServerAction::Exit(status) => exit_status_tx
                    .take()
                    .ok_or_else(|| eyre!("received exit status multiple times"))?
                    .send(status)
                    .map_err(|e| eyre!("failed to send exit status: {:?}", e))?,
                ServerAction::Finished => break,
            }
        }
        trace!("finished");

        Ok(())
    }
}
