///! Async wrapper for the [`datachannel-rs`] crate.
///!
///! [`datachannel-rs`]: https://crates.io/crates/datachannel
use std::{sync::Arc, task::Poll};

use anyhow::Context;
pub use datachannel::{
    ConnectionState, DataChannelInit, IceCandidate, Reliability, RtcConfig, SessionDescription,
};
use datachannel::{DataChannelHandler, PeerConnectionHandler, RtcDataChannel, RtcPeerConnection, DataChannelInfo};
use futures::{
    channel::mpsc,
    io::{AsyncRead, AsyncWrite},
    StreamExt,
};
use parking_lot::Mutex;
#[cfg(feature = "derive")]
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;
use tracing::{debug, error};

#[derive(Debug)]
#[cfg_attr(feature = "derive", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "derive", serde(untagged))]
/// Messages to be used for external signalling.
#[allow(clippy::large_enum_variant)]
pub enum Message {
    RemoteDescription(SessionDescription),
    RemoteCandidate(IceCandidate),
}

struct DataChannel {
    tx_ready: mpsc::Sender<anyhow::Result<()>>,
    tx_inbound: mpsc::Sender<anyhow::Result<Vec<u8>>>,
}
#[allow(clippy::type_complexity)]
impl DataChannel {
    fn new() -> (
        mpsc::Receiver<anyhow::Result<()>>,
        mpsc::Receiver<anyhow::Result<Vec<u8>>>,
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
    fn on_open(&mut self) {
        debug!("on_open");
        // Signal open
        let _ = self.tx_ready.try_send(Ok(()));
    }

    fn on_closed(&mut self) {
        debug!("on_closed");
        let _ = self.tx_inbound.try_send(Err(anyhow::anyhow!("Closed")));
    }

    fn on_error(&mut self, err: &str) {
        let _ = self
            .tx_ready
            .try_send(Err(anyhow::anyhow!(err.to_string())));
        let _ = self
            .tx_inbound
            .try_send(Err(anyhow::anyhow!(err.to_string())));
    }

    fn on_message(&mut self, msg: &[u8]) {
        let s = String::from_utf8_lossy(msg);
        debug!("on_message {}", s);
        let _ = self.tx_inbound.try_send(Ok(msg.to_vec()));
    }

    // TODO?
    fn on_buffered_amount_low(&mut self) {}

    fn on_available(&mut self) {
        debug!("on_available");
    }
}

/// The opened data channel. This struct implements both [`AsyncRead`] and [`AsyncWrite`].
pub struct DataStream {
    /// The actual data channel
    inner: Box<RtcDataChannel<DataChannel>>,
    /// Receiver for inbound bytes from the data channel
    rx_inbound: mpsc::Receiver<anyhow::Result<Vec<u8>>>,
    /// Intermediate buffer of inbound bytes, to be polled by `poll_read`
    buf_inbound: Vec<u8>,
    /// Reference to the PeerConnection to keep around
    peer_con: Option<Arc<Mutex<Box<RtcPeerConnection<ConnInternal>>>>>,
    /// Reference to the outbound piper
    handle: Option<JoinHandle<()>>,
}

impl DataStream {
    pub fn buffered_amount(&self) -> usize {
        self.inner.buffered_amount()
    }

    pub fn available_amount(&self) -> usize {
        self.inner.available_amount()
    }
}

impl AsyncRead for DataStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        if !self.buf_inbound.is_empty() {
            let space = buf.len();
            if self.buf_inbound.len() <= space {
                let len = self.buf_inbound.len();
                buf[..len].copy_from_slice(&self.buf_inbound[..]);
                self.buf_inbound.drain(..);
                Poll::Ready(Ok(len))
            } else {
                buf.copy_from_slice(&self.buf_inbound[..space]);
                self.buf_inbound.drain(..space);
                Poll::Ready(Ok(space))
            }
        } else {
            match self.as_mut().rx_inbound.poll_next_unpin(cx) {
                std::task::Poll::Ready(Some(Ok(x))) => {
                    let space = buf.len();
                    if x.len() <= space {
                        buf[..x.len()].copy_from_slice(&x[..]);
                        Poll::Ready(Ok(x.len()))
                    } else {
                        buf.copy_from_slice(&x[..space]);
                        self.buf_inbound.extend_from_slice(&x[space..]);
                        Poll::Ready(Ok(space))
                    }
                }
                std::task::Poll::Ready(Some(Err(e))) => Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    e.to_string(),
                ))),
                std::task::Poll::Ready(None) => Poll::Ready(Ok(0)),
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

    fn poll_close(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        Poll::Ready(Ok(()))
    }
}

pub struct PeerConnection {
    peer_con: Arc<Mutex<Box<RtcPeerConnection<ConnInternal>>>>,
    rx_incoming: mpsc::Receiver<DataStream>,
    handle: JoinHandle<()>,
}

impl PeerConnection {
    /// Create a new [`PeerConnection`] to be used for either dialing or accepting an inbound
    /// connection. The channel tuple is used to interface with an external signalling system.
    pub fn new(
        config: &RtcConfig,
        (sig_tx, mut sig_rx): (mpsc::Sender<Message>, mpsc::Receiver<Message>),
    ) -> anyhow::Result<Self> {
        let (tx_incoming, rx_incoming) = mpsc::channel(8);
        let peer_con = Arc::new(Mutex::new(RtcPeerConnection::new(
            config,
            ConnInternal {
                tx_signal: sig_tx,
                tx_incoming,
                pending: None,
            },
        )?));
        let pc = peer_con.clone();
        let handle = tokio::spawn(async move {
            while let Some(m) = sig_rx.next().await {
                if let Err(err) = match m {
                    Message::RemoteDescription(i) => pc.lock().set_remote_description(&i),
                    Message::RemoteCandidate(i) => pc.lock().add_remote_candidate(&i),
                } {
                    error!(?err, "Error interacting with RtcPeerConnection");
                }
            }
        });
        Ok(Self {
            peer_con,
            rx_incoming,
            handle,
        })
    }

    /// Wait for an inbound connection.
    pub async fn accept(mut self) -> anyhow::Result<DataStream> {
        let mut s = self.rx_incoming.next().await.context("Tx dropped")?;
        s.handle = Some(self.handle);
        s.peer_con = Some(self.peer_con);
        Ok(s)
    }

    /// Initiate an outbound dialing.
    pub async fn dial(self, label: &str) -> anyhow::Result<DataStream> {
        let (mut ready, rx_inbound, chan) = DataChannel::new();
        let dc = self.peer_con.lock().create_data_channel(label, chan)?;
        ready.next().await.context("Tx dropped")??;
        Ok(DataStream {
            inner: dc,
            rx_inbound,
            buf_inbound: vec![],
            handle: Some(self.handle),
            peer_con: Some(self.peer_con),
        })
    }

    /// Initiate an outbound dialing with extra options.
    pub async fn dial_ex(
        self,
        label: &str,
        dc_init: &DataChannelInit,
    ) -> anyhow::Result<DataStream> {
        let (mut ready, rx_inbound, chan) = DataChannel::new();
        let dc = self
            .peer_con
            .lock()
            .create_data_channel_ex(label, chan, dc_init)?;
        ready.next().await.context("Tx dropped")??;
        Ok(DataStream {
            inner: dc,
            rx_inbound,
            buf_inbound: vec![],
            handle: Some(self.handle),
            peer_con: Some(self.peer_con),
        })
    }
}

struct ConnInternal {
    tx_incoming: mpsc::Sender<DataStream>,
    tx_signal: mpsc::Sender<Message>,
    pending: Option<mpsc::Receiver<anyhow::Result<Vec<u8>>>>,
}

impl PeerConnectionHandler for ConnInternal {
    type DCH = DataChannel;

    fn data_channel_handler(&mut self, _info: DataChannelInfo) -> Self::DCH {
        let (_, rx, dc) = DataChannel::new();
        self.pending.replace(rx);
        dc
    }

    fn on_description(&mut self, sess_desc: SessionDescription) {
        let _ = self
            .tx_signal
            .try_send(Message::RemoteDescription(sess_desc));
    }

    fn on_candidate(&mut self, cand: IceCandidate) {
        let _ = self.tx_signal.try_send(Message::RemoteCandidate(cand));
    }

    fn on_connection_state_change(&mut self, _state: datachannel::ConnectionState) {
        // TODO
    }

    fn on_data_channel(&mut self, data_channel: Box<RtcDataChannel<Self::DCH>>) {
        debug!("new incoming data channel");
        let _ = self.tx_incoming.try_send(DataStream {
            inner: data_channel,
            rx_inbound: self
                .pending
                .take()
                .expect("`data_channel_handler` was just called synchronously in the same thread"),
            buf_inbound: vec![],
            handle: None,
            peer_con: Default::default(),
        });
    }
}
