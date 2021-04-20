use crate::{
    prelude::*,
    protocol::{self, C2sStreamKind, ClientAction, S2cStreamKind},
    sink, source, terminal,
};
use std::{collections::HashMap, sync::Arc};
use tokio::{
    io::AsyncRead,
    sync::{
        mpsc::{Receiver, Sender},
        Notify,
    },
};
use tokio_pty_command::PtyMaster;
use tracing::Span;

#[derive(Debug)]
pub(super) struct Task<R> {
    reader: R,
    pty_master: Option<PtyMaster>,
    c2s_tx_map: HashMap<C2sStreamKind, sink::Sender>,
    s2c_tx_map: HashMap<S2cStreamKind, source::Sender>,
    finish_notify: Arc<Notify>,
    send_error_rx: Receiver<Error>,
    kill_error_tx: Sender<Error>,
}

impl<R> Task<R>
where
    R: AsyncRead + Send + 'static,
{
    pub(super) fn new(
        reader: R,
        pty_master: Option<PtyMaster>,
        c2s_tx_map: HashMap<C2sStreamKind, sink::Sender>,
        s2c_tx_map: HashMap<S2cStreamKind, source::Sender>,
        finish_notify: Arc<Notify>,
        send_error_rx: Receiver<Error>,
        kill_error_tx: Sender<Error>,
    ) -> Self {
        Self {
            reader,
            pty_master,
            c2s_tx_map,
            s2c_tx_map,
            finish_notify,
            send_error_rx,
            kill_error_tx,
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
            pty_master,
            c2s_tx_map,
            s2c_tx_map,
            finish_notify,
            mut send_error_rx,
            kill_error_tx,
        } = self;
        tokio::pin!(reader);

        trace!("started");
        let mut receive_failed = None;
        loop {
            let res = tokio::select! {
                res = protocol::recv_message(&mut reader) => res,
                Some(res) = send_error_rx.recv() => Err(res), // error reported from sender task
            };
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
                ClientAction::WindowSizeChange(ws) => {
                    if let Some(pty_master) = &pty_master {
                        if let Err(err) = terminal::set_window_size(pty_master, ws.width, ws.height)
                        {
                            warn!(?err, "failed to set window size");
                        }
                    } else {
                        warn!("PTY is not allocated");
                    }
                }
                ClientAction::Finished => break,
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
            if let Err(err) = kill_error_tx
                .send(err.wrap_err("connection disconnected unexpectedly"))
                .await
            {
                debug!(?err);
            }
        }
        finish_notify.notify_one();
        trace!("finished");

        Ok(())
    }
}
