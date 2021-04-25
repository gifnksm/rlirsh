use crate::{
    prelude::*,
    protocol::{ConnId, ConnecterAction, ListenerAction, PortId, StreamAction},
    stream::RecvRouter,
};
use std::{fmt::Debug, sync::Arc};
use tokio::{net::TcpListener, sync::mpsc};
use tracing::Span;

#[derive(Debug)]
pub(crate) struct Task<T> {
    port_id: PortId,
    listener: TcpListener,
    tx: mpsc::Sender<T>,
    recv_router: Arc<RecvRouter>,
}

impl<T> Task<T>
where
    T: Debug + Send + Sync + From<StreamAction> + 'static,
{
    pub(crate) fn new(
        port_id: PortId,
        listener: TcpListener,
        tx: mpsc::Sender<T>,
        recv_router: Arc<RecvRouter>,
    ) -> Self {
        Self {
            port_id,
            listener,
            tx,
            recv_router,
        }
    }

    pub(crate) fn spawn(self, span: Span) -> impl Future<Output = Result<()>> {
        tokio::spawn(self.handle().instrument(span))
            .err_into()
            .and_then(future::ready)
    }

    async fn handle(self) -> Result<()> {
        let Self {
            port_id,
            listener,
            tx,
            recv_router,
        } = self;

        for conn_id in (0..).map(ConnId::new) {
            let id = (port_id, conn_id);
            let (conn_tx, mut rx) = mpsc::channel(1);
            recv_router.insert_listener_tx(id, conn_tx);
            let (stream, peer_addr) = match listener.accept().await {
                Ok(res) => res,
                Err(err) => {
                    warn!(?conn_id, ?err, "accept failed");
                    continue;
                }
            };
            debug!(%peer_addr, ?conn_id, "connection accepted");
            let req = ListenerAction::Connect(conn_id);
            tx.send(T::from((port_id, req).into()))
                .await
                .wrap_err("failed to send request")?;
            rx.recv()
                .await
                .ok_or_else(|| eyre!("failed to receive response"))
                .and_then(|res| match res {
                    ConnecterAction::ConnectResponse(res) => Result::<()>::from(res),
                })?;
            recv_router.remove_listener_tx(id);
            debug!("connected");
        }

        Ok(())
    }
}
