use std::{sync::Arc, task::Poll};

use anyhow::Context;
pub use datachannel::{ConnectionState, IceCandidate, RtcConfig, SessionDescription};
use datachannel::{DataChannelHandler, PeerConnectionHandler, RtcDataChannel, RtcPeerConnection};
use derive_more::{AsRef, Display, From};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::mpsc,
    task::JoinHandle,
};
use tracing::{debug, error};

#[derive(From, Debug, Eq, PartialEq, Clone, AsRef, Display)]
#[from(forward)]
#[as_ref(forward)]
pub struct PeerId(String);
pub type ConnectionMessage = (PeerId, Message);
#[derive(Serialize, Deserialize, Debug)]
#[serde(untagged)]
pub enum Message {
    RemoteDescription(SessionDescription),
    RemoteCandidate(IceCandidate),
    #[serde(skip)]
    ConnectionState(ConnectionState),
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
        let _ = self.tx_ready.blocking_send(Ok(()));
    }

    fn on_closed(&mut self) {
        debug!("on_closed");
        let _ = self
            .tx_inbound
            .blocking_send(Err(anyhow::anyhow!("Closed")));
    }

    fn on_error(&mut self, err: &str) {
        let _ = self
            .tx_ready
            .blocking_send(Err(anyhow::anyhow!(err.to_string())));
        let _ = self
            .tx_inbound
            .blocking_send(Err(anyhow::anyhow!(err.to_string())));
    }

    fn on_message(&mut self, msg: &[u8]) {
        let s = String::from_utf8_lossy(msg);
        debug!("on_message {}", s);
        let _ = self.tx_inbound.blocking_send(Ok(msg.to_vec()));
    }

    // TODO?
    fn on_buffered_amount_low(&mut self) {}

    fn on_available(&mut self) {
        debug!("on_available");
    }
}

type PeerConnection = RtcPeerConnection<ListenerInternal>;
pub struct DataStream {
    /// The actual data channel
    inner: Box<RtcDataChannel<DataChannel>>,
    /// Receiver for inbound bytes from the data channel
    rx_inbound: mpsc::Receiver<anyhow::Result<Vec<u8>>>,
    /// Intermediate buffer of inbound bytes, to be polled by `poll_read`
    buf_inbound: Vec<u8>,
    /// Reference to the PeerConnection to keep around
    peer_con: Option<Arc<Mutex<Box<PeerConnection>>>>,
    /// Reference to the outbound piper
    handle: Option<JoinHandle<()>>,
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
                std::task::Poll::Ready(Some(Ok(x))) => {
                    let space = buf.remaining();
                    if x.len() <= space {
                        buf.put_slice(&x[..]);
                    } else {
                        buf.put_slice(&x[..space]);
                        self.buf_inbound.extend_from_slice(&x[space..]);
                    }
                    Poll::Ready(Ok(()))
                }
                std::task::Poll::Ready(Some(Err(e))) => Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    e.to_string(),
                ))),
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
    remote: Arc<Mutex<Option<PeerId>>>,
    handle: JoinHandle<()>,
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
        let remote = Arc::new(Mutex::new(None));
        let peer_con = Arc::new(Mutex::new(RtcPeerConnection::new(
            config,
            ListenerInternal {
                tx_signal: sig_tx,
                tx_incoming,
                pending: None,
                remote: remote.clone(),
            },
        )?));
        let pc = peer_con.clone();
        let remote_c = remote.clone();
        let handle = tokio::spawn(async move {
            while let Some(m) = sig_rx.recv().await {
                remote_c.lock().replace(m.0);

                if let Err(err) = match m.1 {
                    Message::RemoteDescription(i) => pc.lock().set_remote_description(&i),
                    Message::RemoteCandidate(i) => pc.lock().add_remote_candidate(&i),
                    _ => Ok(()),
                } {
                    error!(?err, "Error interacting with RtcPeerConnection");
                }
            }
        });
        Ok(Self {
            peer_con,
            rx_incoming,
            handle,
            remote,
        })
    }

    pub async fn accept(mut self) -> anyhow::Result<DataStream> {
        let mut s = self.rx_incoming.recv().await.context("Tx dropped")?;
        s.handle = Some(self.handle);
        s.peer_con = Some(self.peer_con);
        Ok(s)
    }

    pub async fn dial(self, peer: PeerId, label: &str) -> anyhow::Result<DataStream> {
        self.remote.lock().replace(peer);
        let (mut ready, rx_inbound, chan) = DataChannel::new();
        let dc = self.peer_con.lock().create_data_channel(label, chan)?;
        ready.recv().await.context("Tx dropped")??;
        Ok(DataStream {
            inner: dc,
            rx_inbound,
            buf_inbound: vec![],
            handle: Some(self.handle),
            peer_con: Some(self.peer_con),
        })
    }
}

struct ListenerInternal {
    tx_incoming: mpsc::Sender<DataStream>,
    tx_signal: mpsc::Sender<ConnectionMessage>,
    pending: Option<mpsc::Receiver<anyhow::Result<Vec<u8>>>>,
    remote: Arc<Mutex<Option<PeerId>>>,
}

impl PeerConnectionHandler for ListenerInternal {
    type DCH = DataChannel;

    fn data_channel_handler(&mut self) -> Self::DCH {
        let (_, rx, dc) = DataChannel::new();
        self.pending.replace(rx);
        dc
    }

    fn on_description(&mut self, sess_desc: SessionDescription) {
        let remote = self.remote.lock().as_ref().cloned().expect("Remote is set");
        let _ = self
            .tx_signal
            .blocking_send((remote, Message::RemoteDescription(sess_desc)));
    }

    fn on_candidate(&mut self, cand: IceCandidate) {
        let remote = self.remote.lock().as_ref().cloned().expect("Remote is set");
        let _ = self
            .tx_signal
            .blocking_send((remote, Message::RemoteCandidate(cand)));
    }

    fn on_connection_state_change(&mut self, state: datachannel::ConnectionState) {
        let remote = self.remote.lock().as_ref().cloned().expect("Remote is set");
        let _ = self
            .tx_signal
            .blocking_send((remote, Message::ConnectionState(state)));
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
            handle: None,
            peer_con: Default::default(),
        });
    }
}
