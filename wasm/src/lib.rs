///! Async wrapper for WebRTC data channels. This aims to be a drop-in replacemnt for the
///[`async-datachannel`] crate.
///!
///! [`async-datachannel`]: https://crates.io/crates/async-datachannel
use std::task::Poll;

use futures::{stream, StreamExt};
use js_sys::Reflect;
use log::*;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::mpsc,
};
use tokio_stream::wrappers::ReceiverStream;
use wasm_bindgen::{prelude::*, JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    RtcConfiguration, RtcDataChannel, RtcDataChannelEvent, RtcDataChannelType, RtcIceCandidateInit,
    RtcIceServer, RtcPeerConnection, RtcPeerConnectionIceEvent, RtcSdpType,
    RtcSessionDescriptionInit,
};
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IceCandidate {
    pub candidate: String,
    #[serde(rename = "sdpMid")]
    pub mid: String,
}

#[derive(Serialize, Deserialize, Debug)]
// considered opaque
pub struct SessionDescription {
    pub sdp: String,
    #[serde(rename = "type")]
    pub sdp_type: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
/// Messages to be used for external signalling.
pub enum Message {
    RemoteDescription(SessionDescription),
    RemoteCandidate(IceCandidate),
}

/// The opened data channel. This struct implements both [`AsyncRead`] and [`AsyncWrite`]
/// (tokio-flavoured).
pub struct DataStream {
    /// The actual data channel
    //    inner: Box<RtcDataChannel<DataChannel>>,
    /// Receiver for inbound bytes from the data channel
    rx_inbound: mpsc::Receiver<anyhow::Result<Vec<u8>>>,
    /// Intermediate buffer of inbound bytes, to be polled by `poll_read`
    buf_inbound: Vec<u8>,
    // Reference to the PeerConnection to keep around
    //   peer_con: Option<Arc<Mutex<Box<RtcPeerConnection<ConnInternal>>>>>,
    //
    _on_message: Closure<dyn FnMut(web_sys::MessageEvent)>,
    inner: RtcDataChannel,
    // Do we need the peer_con?
    //peer_con: RtcPeerConnection,
}

impl DataStream {
    fn new(inner: RtcDataChannel) -> Self {
        inner.set_binary_type(RtcDataChannelType::Arraybuffer);
        let (tx, rx_inbound) = mpsc::channel(32);
        let on_message = Closure::wrap(Box::new(move |ev: web_sys::MessageEvent| {
            let res = match ev.data().dyn_into::<js_sys::ArrayBuffer>() {
                Ok(data) => {
                    let byte_array: Vec<u8> = js_sys::Uint8Array::new(&data).to_vec();
                    Ok(byte_array)
                }
                Err(data) => Err(anyhow::anyhow!(
                    "Expected ArrayBuffer, received: \"{:?}\"",
                    data
                )),
            };
            tx.blocking_send(res).expect("FIXME channel died l76");
        }) as Box<dyn FnMut(web_sys::MessageEvent)>);
        inner.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        Self {
            _on_message: on_message,
            inner,
            buf_inbound: vec![],
            rx_inbound,
        }
    }
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
        if let Err(e) = self.as_mut().inner.send_with_u8_array(buf) {
            Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("{:?}", e),
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

pub struct PeerConnection {
    //    peer_con: Arc<Mutex<Box<RtcPeerConnection<ConnInternal>>>>,
    //rx_incoming: mpsc::Receiver<DataStream>,
    inner: RtcPeerConnection,
    sig_tx: mpsc::Sender<Message>,
    sig_rx: mpsc::Receiver<Message>,
    _on_ice_candidate: Closure<dyn FnMut(RtcPeerConnectionIceEvent)>,
}

impl PeerConnection {
    /// Create a new [`PeerConnection`] to be used for either dialing or accepting an inbound
    /// connection. The channel tuple is used to interface with an external signalling system.
    pub fn new(
        turn: Vec<String>,
        (sig_tx, sig_rx): (mpsc::Sender<Message>, mpsc::Receiver<Message>),
    ) -> anyhow::Result<Self> {
        let mut config = RtcConfiguration::new();

        let ice_servers = js_sys::Array::new();
        for s in turn {
            // TODO: handle stun?
            let mut stun_server = RtcIceServer::new();
            let stun_servers = js_sys::Array::new();
            stun_servers.push(&JsValue::from(&s));
            stun_server.urls(&stun_servers);
            ice_servers.push(&JsValue::from(&stun_server));
        }
        config.ice_servers(&ice_servers);

        let inner = RtcPeerConnection::new_with_configuration(&config)
            .map_err(|e| anyhow::anyhow!("FIXME creating peer connection {:?}", e.as_string()))?;

        let sig_tx_c = sig_tx.clone();
        let on_ice_candidate = Closure::wrap(Box::new(move |ev: RtcPeerConnectionIceEvent| {
            if let Some(candidate) = ev.candidate() {
                if let Err(e) = sig_tx_c.blocking_send(Message::RemoteCandidate(IceCandidate {
                    candidate: candidate.candidate(),
                    mid: candidate.sdp_mid().unwrap_or_else(|| "".to_string()),
                })) {
                    todo!("Sending failed {:?}", e);
                }
            }
        })
            as Box<dyn FnMut(RtcPeerConnectionIceEvent)>);

        inner.set_onicecandidate(Some(on_ice_candidate.as_ref().unchecked_ref()));
        Ok(Self {
            inner,
            sig_rx,
            sig_tx,
            _on_ice_candidate: on_ice_candidate,
        })
    }

    /// Wait for an inbound connection.
    /// wait for remote offer
    /// set_remote_desc(&offer)
    /// create answer(&offer)
    /// set_local_desc(&answer)
    /// send(&answer)
    pub async fn accept(self) -> anyhow::Result<DataStream> {
        let Self {
            inner,
            sig_rx,
            sig_tx,
            ..
        } = self;
        enum Either<A, B> {
            Left(A),
            Right(B),
        }
        let (tx_open, mut rx_open) = mpsc::channel(1);
        let (tx_chan, rx_chan) = mpsc::channel(1);

        let on_open = Closure::wrap(Box::new(move || {
            trace!("Inbound data channel opened");
            tx_open.blocking_send(()).expect("channel diend l226");
        }) as Box<dyn FnMut()>);
        let on_data_channel = Closure::wrap(Box::new(move |ev: RtcDataChannelEvent| {
            trace!("Inbound connection attempt");
            let channel = ev.channel();
            channel.set_onopen(Some(on_open.as_ref().unchecked_ref()));
            if let Err(e) = tx_chan.blocking_send(channel) {
                todo!("err {:?}", e);
            }
        }) as Box<dyn FnMut(RtcDataChannelEvent)>);
        inner.set_ondatachannel(Some(on_data_channel.as_ref().unchecked_ref()));
        let mut s = stream::select(
            ReceiverStream::new(sig_rx).map(Either::Left),
            ReceiverStream::new(rx_chan).map(Either::Right),
        );

        while let Some(m) = s.next().await {
            match m {
                Either::Left(remote_msg) => match remote_msg {
                    Message::RemoteDescription(desc) => {
                        if desc.sdp_type == "offer" {
                            trace!("Received offer from remote");
                            let mut description = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
                            description.sdp(&desc.sdp);
                            JsFuture::from(inner.set_remote_description(&description))
                                .await
                                .expect("FIXME set remote descri l249");

                            let answer = JsFuture::from(inner.create_answer())
                                .await
                                .expect("FIXME create answer l252");
                            let answer_sdp = Reflect::get(&answer, &JsValue::from_str("sdp"))
                                .expect("FIXME get sdp from answer l254")
                                .as_string()
                                .unwrap();
                            let mut answer_obj = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
                            answer_obj.sdp(&answer_sdp);
                            JsFuture::from(inner.set_local_description(&answer_obj))
                                .await
                                .expect("FIXME set local desc");

                            sig_tx
                                .send(Message::RemoteDescription(SessionDescription {
                                    sdp_type: "answer".into(),
                                    sdp: answer_sdp,
                                }))
                                .await
                                .expect("FIXME channel died l264");
                            trace!("Sent answer to remote");
                        }
                    }
                    Message::RemoteCandidate(c) => {
                        let mut cand = RtcIceCandidateInit::new(&c.candidate);
                        cand.sdp_mid(Some(&c.mid));
                        JsFuture::from(
                            inner.add_ice_candidate_with_opt_rtc_ice_candidate_init(Some(&cand)),
                        )
                        .await
                        .expect("FIXME add ice candidate");
                    }
                },
                Either::Right(dc) => {
                    // Forget them closures
                    inner.set_onicecandidate(None);
                    inner.set_ondatachannel(None);

                    rx_open.recv().await.expect("FIXME channel died l287");
                    dc.set_onopen(None);
                    return Ok(DataStream::new(dc));
                }
            }
        }
        anyhow::bail!("Channel didn't open");
    }

    /// Initiate an outbound dialing.
    /// dial
    /// create offer
    /// set local_description(&offer)
    /// send(offer)
    /// wait for remote answer
    /// set_remote_description(&answer)
    pub async fn dial(self, label: &str) -> anyhow::Result<DataStream> {
        let Self {
            sig_tx,
            inner,
            sig_rx,
            ..
        } = self;
        let dc = inner.create_data_channel(label);
        enum Either<A, B> {
            Left(A),
            Right(B),
        }
        let (tx_open, rx_open) = mpsc::channel::<()>(1);

        let on_open = Closure::wrap(Box::new(move || {
            trace!("Outbound Datachannel opened");
            tx_open.blocking_send(()).expect("FIXME channel died l318");
        }) as Box<dyn FnMut()>);
        dc.set_onopen(Some(on_open.as_ref().unchecked_ref()));
        let mut s = stream::select(
            ReceiverStream::new(sig_rx).map(Either::Left),
            ReceiverStream::new(rx_open).map(Either::Right),
        );

        let offer = JsFuture::from(inner.create_offer())
            .await
            .expect("FIXME creating offer");
        let offer_sdp = Reflect::get(&offer, &JsValue::from_str("sdp"))
            .expect("FIXME extracting sdp from offer")
            .as_string()
            .unwrap();

        let mut offer_obj = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
        offer_obj.sdp(&offer_sdp);
        let sld_promise = inner.set_local_description(&offer_obj);
        JsFuture::from(sld_promise)
            .await
            .expect("FIXME set local desc l335");
        sig_tx
            .send(Message::RemoteDescription(SessionDescription {
                sdp_type: "offer".into(),
                sdp: offer_sdp,
            }))
            .await
            .expect("FIXME channel died l342");

        while let Some(m) = s.next().await {
            match m {
                Either::Left(remote_msg) => match remote_msg {
                    Message::RemoteDescription(desc) => {
                        if desc.sdp_type == "answer" {
                            let mut description = RtcSessionDescriptionInit::new(
                                RtcSdpType::from_js_value(&JsValue::from_str(&desc.sdp_type))
                                    .expect("FIXME create rtc type l351"),
                            );
                            description.sdp(&desc.sdp);
                            JsFuture::from(inner.set_remote_description(&description))
                                .await
                                .expect("FIXME set remote desc l356");
                        }
                    }
                    Message::RemoteCandidate(c) => {
                        let mut cand = RtcIceCandidateInit::new(&c.candidate);
                        cand.sdp_mid(Some(&c.mid));
                        JsFuture::from(
                            inner.add_ice_candidate_with_opt_rtc_ice_candidate_init(Some(&cand)),
                        )
                        .await
                        .expect("FIXME add ice cand l366");
                    }
                },
                Either::Right(_) => {
                    // Forget them closures
                    inner.set_onicecandidate(None);
                    dc.set_onopen(None);

                    return Ok(DataStream::new(dc));
                }
            }
        }

        anyhow::bail!("Channel didn't open");
    }
}
