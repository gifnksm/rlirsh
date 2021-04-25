use crate::{
    net::SocketAddrs,
    prelude::*,
    protocol::{ConnecterAction, ListenerAction, PortId, Response, StreamAction},
    stream::RecvRouter,
};
use std::fmt::Debug;
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
}

impl<T> Task<T>
where
    T: Debug + Send + Sync + From<StreamAction> + 'static,
{
    pub(crate) fn new(
        port_id: PortId,
        addr: SocketAddrs,
        tx: mpsc::Sender<T>,
        recv_router: &RecvRouter,
    ) -> Self {
        let (listen_tx, rx) = mpsc::channel(128);
        recv_router.insert_connecter_tx(port_id, Sender(listen_tx));
        Self {
            port_id,
            addr,
            tx,
            rx,
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
        } = self;

        trace!("started");

        while let Some(msg) = rx.recv().await {
            trace!(?msg);
            match msg {
                ListenerAction::Connect(conn_id) => {
                    let resp_id = (port_id, conn_id);
                    let addr = addr.clone();
                    let tx = tx.clone();
                    let _ = tokio::spawn(async move {
                        let res = addr.connect().await;
                        let resp = ConnecterAction::ConnectResponse(Response::new(&res));
                        let msg = T::from((resp_id, resp).into());
                        tx.send(msg).await?;
                        let stream = match res {
                            Ok(stream) => stream,
                            Err(err) => {
                                warn!(?err);
                                bail!(err);
                            }
                        };
                        // TODO
                        Ok::<(), Error>(())
                    });
                }
            }
        }

        trace!("finished");

        Ok(())
    }
}
