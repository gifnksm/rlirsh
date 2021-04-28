use crate::{
    net::SocketAddrs,
    prelude::*,
    protocol::{ConnecterAction, ListenerAction, PortId, Response, StreamAction, StreamId},
    stream::{sink, source, RecvRouter},
};
use std::{fmt::Debug, sync::Arc};
use tokio::sync::mpsc;
use tracing::Span;

#[derive(Debug, Clone)]
pub(crate) struct Sender(mpsc::Sender<ListenerAction>);

impl Sender {
    pub(crate) async fn send(&self, action: ListenerAction) -> Result<()> {
        self.0.send(action).await?;
        Ok(())
    }

    pub(crate) async fn shutdown(&self) -> Result<()> {
        // TODO: add appropriate shutdown operation
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct Task<T> {
    port_id: PortId,
    addr: SocketAddrs,
    tx: mpsc::Sender<T>,
    rx: mpsc::Receiver<ListenerAction>,
    recv_router: Arc<RecvRouter>,
}

impl<T> Task<T>
where
    T: Debug + Send + Sync + From<StreamAction> + 'static,
{
    pub(crate) fn new(
        port_id: PortId,
        addr: SocketAddrs,
        tx: mpsc::Sender<T>,
        recv_router: Arc<RecvRouter>,
    ) -> Self {
        let (listen_tx, rx) = mpsc::channel(128);
        recv_router.insert_connecter_tx(port_id, Sender(listen_tx));
        Self {
            port_id,
            addr,
            tx,
            rx,
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
            addr,
            tx,
            mut rx,
            recv_router,
        } = self;

        trace!("started");

        while let Some(msg) = rx.recv().await {
            trace!(?msg);
            match msg {
                ListenerAction::Connect(conn_id) => {
                    Self::connect(
                        StreamId::from((port_id, conn_id)),
                        addr.clone(),
                        tx.clone(),
                        recv_router.clone(),
                    )
                    .await?;
                }
                ListenerAction::ListenerClosed => break,
            }
        }

        trace!("finished");

        Ok(())
    }

    async fn connect(
        id: StreamId,
        addr: SocketAddrs,
        tx: mpsc::Sender<T>,
        recv_router: Arc<RecvRouter>,
    ) -> Result<()> {
        let _ = tokio::spawn(async move {
            let res = addr.connect().await;
            let resp = ConnecterAction::ConnectResponse(Response::new(&res));
            let msg = T::from((id, resp).into());
            tx.send(msg).await?;
            let stream = match res {
                Ok(stream) => stream,
                Err(err) => {
                    warn!(?err);
                    bail!(err);
                }
            };

            let (reader, writer) = stream.into_split();
            let _ = source::Task::new(id, reader, tx.clone(), &recv_router)
                .spawn(info_span!("forward_source", ?id));
            let _ = sink::Task::new(id, writer, tx.clone(), &recv_router)
                .spawn(info_span!("forward_sink", ?id));

            Ok::<(), Error>(())
        });
        Ok(())
    }
}
