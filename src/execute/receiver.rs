use crate::{
    prelude::*,
    protocol::{self, ConnectorAction, ExitStatus, PortId, ServerAction},
    stream::RecvRouter,
};
use std::{collections::HashMap, sync::Arc};
use tokio::{
    io::AsyncRead,
    sync::{mpsc, oneshot},
};
use tracing::Span;

#[derive(Debug)]
pub(super) struct Task<R> {
    reader: R,
    recv_router: Arc<RecvRouter>,
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
        recv_router: Arc<RecvRouter>,
        listener_tx_map: HashMap<PortId, mpsc::Sender<ConnectorAction>>,
        exit_status_tx: oneshot::Sender<Result<ExitStatus>>,
        error_rx: mpsc::Receiver<Error>,
    ) -> Self {
        Self {
            reader,
            recv_router,
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
            recv_router,
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
                ServerAction::StreamAction(id, action) => {
                    recv_router.send_stream_action(id, action).await?
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
            recv_router.shutdown().await;
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
