use crate::{
    prelude::*,
    protocol::{
        ConnecterAction, ListenerAction, PortId, SinkAction, SourceAction, StreamAction, StreamId,
    },
    stream::{connecter, sink, source},
};
use parking_lot::Mutex;
use std::collections::HashMap;
use tokio::sync::mpsc;

#[derive(Debug, Default)]
pub(crate) struct RecvRouter {
    sink_tx_map: Mutex<HashMap<StreamId, sink::Sender>>,
    source_tx_map: Mutex<HashMap<StreamId, source::Sender>>,
    connecter_tx_map: Mutex<HashMap<PortId, connecter::Sender>>,
    listener_tx_map: Mutex<HashMap<StreamId, mpsc::Sender<ConnecterAction>>>,
}

impl RecvRouter {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn insert_sink_tx(&self, id: StreamId, tx: sink::Sender) {
        assert!(self.sink_tx_map.lock().insert(id, tx).is_none())
    }

    pub(crate) fn insert_source_tx(&self, id: StreamId, tx: source::Sender) {
        assert!(self.source_tx_map.lock().insert(id, tx).is_none())
    }

    pub(crate) fn insert_connecter_tx(&self, id: PortId, tx: connecter::Sender) {
        assert!(self.connecter_tx_map.lock().insert(id, tx).is_none())
    }

    pub(crate) fn insert_listener_tx(&self, id: StreamId, tx: mpsc::Sender<ConnecterAction>) {
        assert!(self.listener_tx_map.lock().insert(id, tx).is_none())
    }

    pub(crate) fn remove_listener_tx(&self, id: StreamId) {
        assert!(self.listener_tx_map.lock().remove(&id).is_some())
    }

    pub(crate) async fn shutdown(&self) {
        let sink_tx_map = self.sink_tx_map.lock().drain().collect::<Vec<_>>();
        let source_tx_map = self.source_tx_map.lock().drain().collect::<Vec<_>>();
        let connecter_tx_map = self.connecter_tx_map.lock().drain().collect::<Vec<_>>();

        for (id, tx) in sink_tx_map {
            if let Err(err) = tx.shutdown().instrument(info_span!("sink_tx", ?id)).await {
                debug!(?err);
            }
        }
        for (id, tx) in source_tx_map {
            if let Err(err) = tx.shutdown().instrument(info_span!("source_tx", ?id)).await {
                debug!(?err);
            }
        }
        for (id, tx) in connecter_tx_map {
            if let Err(err) = tx
                .shutdown()
                .instrument(info_span!("connecter_tx", ?id))
                .await
            {
                debug!(?err);
            }
        }
    }

    pub(crate) async fn send_stream_action(&self, action: StreamAction) -> Result<()> {
        match action {
            StreamAction::Source(id, action) => self.send_source_action(id, action).await,
            StreamAction::Sink(id, action) => self.send_sink_action(id, action).await,
            StreamAction::Listener(id, action) => self.send_listener_action(id, action).await,
            StreamAction::Connecter(id, action) => self.send_connecter_action(id, action).await,
        }
    }

    async fn send_source_action(&self, id: StreamId, action: SourceAction) -> Result<()> {
        let tx = self
            .sink_tx_map
            .lock()
            .get(&id)
            .ok_or_else(|| eyre!("sink_tx not found: {:?}", id))?
            .clone();
        tx.send(action)
            .instrument(info_span!("sink_tx", ?id))
            .await?;
        Ok(())
    }

    async fn send_sink_action(&self, id: StreamId, action: SinkAction) -> Result<()> {
        let tx = self
            .source_tx_map
            .lock()
            .get(&id)
            .ok_or_else(|| eyre!("source_tx not found: {:?}", id))?
            .clone();
        tx.send(action)
            .instrument(info_span!("source_tx", ?id))
            .await?;
        Ok(())
    }

    async fn send_listener_action(&self, id: PortId, action: ListenerAction) -> Result<()> {
        let tx = self
            .connecter_tx_map
            .lock()
            .get(&id)
            .ok_or_else(|| eyre!("connecter_tx not found: {:?}", id))?
            .clone();
        tx.send(action)
            .instrument(info_span!("connecter_tx", ?id))
            .await?;
        Ok(())
    }

    async fn send_connecter_action(&self, id: StreamId, action: ConnecterAction) -> Result<()> {
        let tx = self
            .listener_tx_map
            .lock()
            .get(&id)
            .ok_or_else(|| eyre!("listener_tx not found: {:?}", id))?
            .clone();
        tx.send(action)
            .instrument(info_span!("listener_tx", ?id))
            .await?;
        Ok(())
    }
}
