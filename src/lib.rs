use std::{sync::Arc, task::Poll};

use anyhow::Context;
pub use datachannel::RtcConfig;
use datachannel::{
    DataChannelHandler, IceCandidate, PeerConnectionHandler, RtcDataChannel, RtcPeerConnection,
    SessionDescription,
};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::mpsc,
};
use tracing::{debug, error};

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub enum ConnectionMessage {
    RemoteDescription(SessionDescription),
    RemoteCandidate(IceCandidate),
}

struct DataChannel {
    tx_ready: mpsc::Sender<anyhow::Result<()>>,
    tx_inbound: mpsc::Sender<Vec<u8>>,
}
impl DataChannel {
    fn new() -> (
        mpsc::Receiver<anyhow::Result<()>>,
        mpsc::Receiver<Vec<u8>>,
        Self,
    ) {
        let (tx_ready, rx_ready) = mpsc::channel(1);
        let (tx_inbound, rx_inbound) = mpsc::channel(128);
        (
            rx_ready,
            rx_inbound,
            Self {
                tx_ready,
                tx_inbound,
            },
        )
    }
}

impl DataChannelHandler for DataChannel {
    // TODO?
    fn on_open(&mut self) {
        debug!("on_open");
        // Signal open
        let _ = self.tx_ready.blocking_send(Ok(()));
    }

    // TODO?
    fn on_closed(&mut self) {
        debug!("on_closed");
    }

    fn on_error(&mut self, err: &str) {
        let _ = self
            .tx_ready
            .blocking_send(Err(anyhow::anyhow!(err.to_string())));
    }

    fn on_message(&mut self, msg: &[u8]) {
        let s = String::from_utf8_lossy(msg);
        debug!("on_message {}", s);
        let _ = self.tx_inbound.blocking_send(msg.to_vec());
    }

    // TODO?
    fn on_buffered_amount_low(&mut self) {}

    fn on_available(&mut self) {
        debug!("on_available");
    }
}

type PeerConnection = RtcPeerConnection<ListenerInternal>;
pub struct DataStream {
    inner: Box<RtcDataChannel<DataChannel>>,
    rx_inbound: mpsc::Receiver<Vec<u8>>,
    buf_inbound: Vec<u8>,
}
impl AsyncRead for DataStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if !self.buf_inbound.is_empty() {
            let space = buf.remaining();
            if self.buf_inbound.len() <= space {
                buf.put_slice(&self.buf_inbound[..]);
                self.buf_inbound.drain(..);
            } else {
                buf.put_slice(&self.buf_inbound[..space]);
                self.buf_inbound.drain(..space);
            }
            Poll::Ready(Ok(()))
        } else {
            match self.as_mut().rx_inbound.poll_recv(cx) {
                std::task::Poll::Ready(Some(x)) => {
                    let space = buf.remaining();
                    if x.len() <= space {
                        buf.put_slice(&x[..]);
                    } else {
                        buf.put_slice(&x[..space]);
                        self.buf_inbound.extend_from_slice(&x[space..]);
                    }
                    Poll::Ready(Ok(()))
                }
                std::task::Poll::Ready(None) => Poll::Ready(Ok(())),
                Poll::Pending => Poll::Pending,
            }
        }
    }
}

impl AsyncWrite for DataStream {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, std::io::Error>> {
        // TODO: Maybe query the underlying buffer to signal backpressure
        if let Err(e) = self.as_mut().inner.send(buf) {
            Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                e.to_string(),
            )))
        } else {
            Poll::Ready(Ok(buf.len()))
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        Poll::Ready(Ok(()))
    }
}

pub struct Listener {
    peer_con: Arc<Mutex<Box<PeerConnection>>>,
    rx_incoming: mpsc::Receiver<DataStream>,
}

impl Listener {
    pub fn new(
        config: &RtcConfig,
        (sig_tx, mut sig_rx): (
            mpsc::Sender<ConnectionMessage>,
            mpsc::Receiver<ConnectionMessage>,
        ),
    ) -> anyhow::Result<Self> {
        let (tx_incoming, rx_incoming) = mpsc::channel(8);
        let peer_con = Arc::new(Mutex::new(RtcPeerConnection::new(
            config,
            ListenerInternal {
                tx_signal: sig_tx,
                tx_incoming,
                pending: None,
            },
        )?));
        let pc = peer_con.clone();
        // TODO: handle drop
        tokio::spawn(async move {
            while let Some(m) = sig_rx.recv().await {
                if let Err(err) = match m {
                    ConnectionMessage::RemoteDescription(i) => pc.lock().set_remote_description(&i),
                    ConnectionMessage::RemoteCandidate(i) => pc.lock().add_remote_candidate(&i),
                } {
                    error!(?err, "Error interacting with RtcPeerConnection");
                }
            }
        });
        Ok(Self {
            peer_con,
            rx_incoming,
        })
    }

    // TODO: return more metadata?
    pub async fn accept(&mut self) -> anyhow::Result<DataStream> {
        self.rx_incoming.recv().await.context("FIXME")
    }

    pub async fn dial(&self, _peer: &str, label: &str) -> anyhow::Result<DataStream> {
        // TODO handle signaling for peer
        let (mut ready, rx_inbound, chan) = DataChannel::new();
        let dc = self.peer_con.lock().create_data_channel(label, chan)?;
        ready.recv().await.context("Tx dropped")??;
        Ok(DataStream {
            inner: dc,
            rx_inbound,
            buf_inbound: vec![],
        })
    }
}

struct ListenerInternal {
    tx_incoming: mpsc::Sender<DataStream>,
    tx_signal: mpsc::Sender<ConnectionMessage>,
    pending: Option<mpsc::Receiver<Vec<u8>>>,
}

impl PeerConnectionHandler for ListenerInternal {
    type DCH = DataChannel;

    fn data_channel_handler(&mut self) -> Self::DCH {
        let (_, rx, dc) = DataChannel::new();
        self.pending.replace(rx);
        dc
    }

    fn on_description(&mut self, sess_desc: SessionDescription) {
        let _ = self
            .tx_signal
            .blocking_send(ConnectionMessage::RemoteDescription(sess_desc));
    }

    fn on_candidate(&mut self, cand: IceCandidate) {
        let _ = self
            .tx_signal
            .blocking_send(ConnectionMessage::RemoteCandidate(cand));
    }

    fn on_connection_state_change(&mut self, state: datachannel::ConnectionState) {
        //TODO
    }

    fn on_data_channel(&mut self, data_channel: Box<RtcDataChannel<Self::DCH>>) {
        debug!("new incoming data channel");
        let _ = self.tx_incoming.blocking_send(DataStream {
            inner: data_channel,
            rx_inbound: self
                .pending
                .take()
                .expect("`data_channel_handler` was just called synchronously in the same thread"),
            buf_inbound: vec![],
        });
    }
}
