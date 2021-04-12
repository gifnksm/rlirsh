use crate::{
    prelude::*,
    protocol::{self, C2sStreamKind, ClientAction, S2cStreamKind},
    sink, source,
};
use std::{collections::HashMap, sync::Arc};
use tokio::{io::AsyncRead, sync::Notify};
use tracing::Span;

#[derive(Debug)]
pub(super) struct Task<R> {
    reader: R,
    c2s_tx_map: HashMap<C2sStreamKind, sink::Sender>,
    s2c_tx_map: HashMap<S2cStreamKind, source::Sender>,
    finish_notify: Arc<Notify>,
}

impl<R> Task<R>
where
    R: AsyncRead + Send + 'static,
{
    pub(super) fn new(
        reader: R,
        c2s_tx_map: HashMap<C2sStreamKind, sink::Sender>,
        s2c_tx_map: HashMap<S2cStreamKind, source::Sender>,
        finish_notify: Arc<Notify>,
    ) -> Self {
        Self {
            reader,
            c2s_tx_map,
            s2c_tx_map,
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
            reader,
            c2s_tx_map,
            s2c_tx_map,
            finish_notify,
        } = self;
        tokio::pin!(reader);

        trace!("started");
        loop {
            let message = protocol::recv_message(&mut reader).await?;
            trace!(?message);
            match message {
                ClientAction::SourceAction(kind, action) => {
                    let tx = c2s_tx_map
                        .get(&kind)
                        .ok_or_else(|| eyre!("tx not found: {:?}", kind))?;
                    tx.send(action)
                        .instrument(info_span!("source", ?kind))
                        .await?
                }
                ClientAction::SinkAction(kind, action) => {
                    let tx = s2c_tx_map
                        .get(&kind)
                        .ok_or_else(|| eyre!("tx not found: {:?}", kind))?;
                    tx.send(action)
                        .instrument(info_span!("sink", ?kind))
                        .await?
                }
                ClientAction::Finished => break,
            }
        }
        finish_notify.notify_one();
        trace!("finished");

        Ok(())
    }
}
