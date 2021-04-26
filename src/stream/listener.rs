use crate::{
    prelude::*,
    protocol::{ConnId, ConnecterAction, ListenerAction, PortId, StreamAction, StreamId},
    stream::{sink, source, RecvRouter},
};
use std::{fmt::Debug, sync::Arc};
use tokio::{
    net::TcpListener,
    sync::{broadcast, mpsc},
};
use tracing::Span;

#[derive(Debug)]
pub(crate) struct Task<T> {
    port_id: PortId,
    listener: TcpListener,
    send_msg_tx: mpsc::Sender<T>,
    task_end_rx: broadcast::Receiver<()>,
    recv_router: Arc<RecvRouter>,
}

impl<T> Task<T>
where
    T: Debug + Send + Sync + From<StreamAction> + 'static,
{
    pub(crate) fn new(
        port_id: PortId,
        listener: TcpListener,
        send_msg_tx: mpsc::Sender<T>,
        task_end_rx: broadcast::Receiver<()>,
        recv_router: Arc<RecvRouter>,
    ) -> Self {
        Self {
            port_id,
            listener,
            send_msg_tx,
            task_end_rx,
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
            send_msg_tx,
            mut task_end_rx,
            recv_router,
        } = self;

        debug!("start");

        for conn_id in (0..).map(ConnId::new) {
            let id = StreamId::Forward(port_id, conn_id);
            let (conn_tx, mut rx) = mpsc::channel(1);
            recv_router.insert_listener_tx(id, conn_tx);
            let res = tokio::select! {
                res = listener.accept() => res,
                res = task_end_rx.recv() => {
                    if let Err(err) = res {
                        warn!(?err, "failed to receive task end signal");
                    }
                    break;
                },
            };
            let (stream, peer_addr) = match res {
                Ok(res) => res,
                Err(err) => {
                    warn!(?conn_id, ?err, "accept failed");
                    continue;
                }
            };
            debug!(%peer_addr, ?conn_id, "connection accepted");
            let req = ListenerAction::Connect(conn_id);
            send_msg_tx
                .send(T::from((port_id, req).into()))
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

            let (reader, writer) = stream.into_split();
            let _ = source::Task::new(id, reader, send_msg_tx.clone(), &recv_router)
                .spawn(info_span!("forward_source", ?id));
            let _ = sink::Task::new(id, writer, send_msg_tx.clone(), &recv_router)
                .spawn(info_span!("forward_sink", ?id));
        }

        let req = ListenerAction::ListenerClosed;
        send_msg_tx
            .send(T::from((port_id, req).into()))
            .await
            .wrap_err("failed to send request")?;

        debug!("finish");

        Ok(())
    }
}
