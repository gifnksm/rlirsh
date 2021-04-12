use crate::{
    prelude::*,
    protocol::{self, C2sStreamKind, ClientAction, S2cStreamKind},
    sink, source,
};
use std::{collections::HashMap, sync::Arc};
use tokio::{
    io::AsyncRead,
    sync::{mpsc::Receiver, Notify},
};
use tracing::Span;

#[derive(Debug)]
pub(super) struct Task<R> {
    reader: R,
    c2s_tx_map: HashMap<C2sStreamKind, sink::Sender>,
    s2c_tx_map: HashMap<S2cStreamKind, source::Sender>,
    finish_notify: Arc<Notify>,
    error_rx: Receiver<Error>,
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
        error_rx: Receiver<Error>,
    ) -> Self {
        Self {
            reader,
            c2s_tx_map,
            s2c_tx_map,
            finish_notify,
            error_rx,
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
            mut error_rx,
        } = self;
        tokio::pin!(reader);

        trace!("started");
        let mut receive_failed = false;
        loop {
            let res = tokio::select! {
                res = protocol::recv_message(&mut reader) => res,
                Some(res) = error_rx.recv() => Err(res), // error reported from sender task
            };
            let message = match res {
                Ok(message) => message,
                Err(err) => {
                    debug!(?err);
                    receive_failed = true;
                    break;
                }
            };
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
        if receive_failed {
            // If error occurred, shutdown all handlers on this process
            for (kind, tx) in &s2c_tx_map {
                if let Err(err) = tx.shutdown().instrument(info_span!("source", ?kind)).await {
                    debug!(?err);
                }
            }
            for (kind, tx) in &c2s_tx_map {
                if let Err(err) = tx.shutdown().instrument(info_span!("client", ?kind)).await {
                    debug!(?err);
                }
            }
        }
        finish_notify.notify_one();
        trace!("finished");

        Ok(())
    }
}
