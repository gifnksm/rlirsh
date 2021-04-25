use crate::{
    prelude::*,
    protocol::{SinkAction, SourceAction, StreamAction, StreamId},
    stream::{sink, source},
};
use derive_more::From;
use parking_lot::Mutex;
use std::collections::HashMap;

#[derive(Debug, From)]
pub(crate) enum StreamSender {
    Sink(sink::Sender),
    Source(source::Sender),
}

#[derive(Debug, Default)]
pub(crate) struct RecvRouter {
    sink_tx_map: Mutex<HashMap<StreamId, sink::Sender>>,
    source_tx_map: Mutex<HashMap<StreamId, source::Sender>>,
}

impl RecvRouter {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn insert_tx(&self, id: StreamId, tx: impl Into<StreamSender>) {
        match tx.into() {
            StreamSender::Sink(tx) => assert!(self.sink_tx_map.lock().insert(id, tx).is_none()),
            StreamSender::Source(tx) => assert!(self.source_tx_map.lock().insert(id, tx).is_none()),
        }
    }

    pub(crate) async fn shutdown(&self) {
        let sink_tx_map = self.sink_tx_map.lock().drain().collect::<Vec<_>>();
        let source_tx_map = self.source_tx_map.lock().drain().collect::<Vec<_>>();

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
    }

    pub(crate) async fn send_stream_action(
        &self,
        id: StreamId,
        action: StreamAction,
    ) -> Result<()> {
        match action {
            StreamAction::SourceAction(action) => self.send_source_action(id, action).await,
            StreamAction::SinkAction(action) => self.send_sink_action(id, action).await,
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
}
