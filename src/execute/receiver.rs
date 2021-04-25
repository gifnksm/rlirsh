use crate::{
    prelude::*,
    protocol::{
        self, C2sStreamKind, ConnectorAction, ExitStatus, PortId, S2cStreamKind, ServerAction,
    },
    sink, source,
};
use std::collections::HashMap;
use tokio::{
    io::AsyncRead,
    sync::{mpsc, oneshot},
};
use tracing::Span;

#[derive(Debug)]
pub(super) struct Task<R> {
    reader: R,
    c2s_tx_map: HashMap<C2sStreamKind, source::Sender>,
    s2c_tx_map: HashMap<S2cStreamKind, sink::Sender>,
    listener_tx_map: HashMap<PortId, mpsc::Sender<ConnectorAction>>,
    exit_status_tx: oneshot::Sender<Result<ExitStatus>>,
    error_rx: mpsc::Receiver<Error>,
}

impl<R> Task<R>
where
    R: AsyncRead + Send + 'static,
{
    pub(super) fn new(
        reader: R,
        c2s_tx_map: HashMap<C2sStreamKind, source::Sender>,
        s2c_tx_map: HashMap<S2cStreamKind, sink::Sender>,
        listener_tx_map: HashMap<PortId, mpsc::Sender<ConnectorAction>>,
        exit_status_tx: oneshot::Sender<Result<ExitStatus>>,
        error_rx: mpsc::Receiver<Error>,
    ) -> Self {
        Self {
            reader,
            c2s_tx_map,
            s2c_tx_map,
            listener_tx_map,
            exit_status_tx,
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
            listener_tx_map,
            exit_status_tx,
            mut error_rx,
        } = self;
        tokio::pin!(reader);
        let mut exit_status_tx = Some(exit_status_tx);
        trace!("started");
        let mut receive_failed = None;
        loop {
            let res = tokio::select!(
                res = protocol::recv_message(&mut reader) => res.wrap_err("failed to receive message from server"),
                Some(res) = error_rx.recv() => Err(res), // error reported from sender task
            );
            let message = match res {
                Ok(message) => message,
                Err(err) => {
                    debug!(?err);
                    receive_failed = Some(err);
                    break;
                }
            };
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
                ServerAction::ConnectorAction(port_id, action) => {
                    debug!(?port_id, ?action);
                    let tx = listener_tx_map
                        .get(&port_id)
                        .ok_or_else(|| eyre!("tx not found: {:?}", port_id))?;
                    tx.send(action)
                        .instrument(info_span!("connector", ?port_id))
                        .await?;
                }
                ServerAction::Exit(status) => exit_status_tx
                    .take()
                    .ok_or_else(|| eyre!("received exit status multiple times"))?
                    .send(Ok(status))
                    .map_err(|e| eyre!("failed to send exit status: {:?}", e))?,
                ServerAction::Finished => break,
            }
        }

        if let Some(err) = receive_failed {
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
            if let Some(tx) = exit_status_tx {
                if let Err(err) = tx.send(Err(err.wrap_err("connection disconnected unexpectedly")))
                {
                    debug!(?err);
                }
            }
        }

        trace!("finished");

        Ok(())
    }
}
