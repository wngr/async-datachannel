use std::time::Duration;

use async_datachannel::{Message, PeerConnection, PeerId, RtcConfig};
use async_tungstenite::{tokio::connect_async, tungstenite};
use futures::{SinkExt, StreamExt};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::mpsc,
};
use tracing::{debug, info};

/// Works with the signalling server from https://github.com/paullouisageneau/libdatachannel/tree/master/examples/signaling-server-rust
/// Start two shells
/// 1. RUST_LOG=debug cargo run --example smoke -- ws://127.0.0.1:8000/other_peer
/// 2. RUST_LOG=debug cargo run --example smoke -- ws://127.0.0.1:8000/initiator other_peer
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let ice_servers = vec!["stun:stun.l.google.com:19302"];
    let conf = RtcConfig::new(&ice_servers);
    let (tx_sig_outbound, mut rx_sig_outbound) = mpsc::channel(32);
    let (tx_sig_inbound, rx_sig_inbound) = mpsc::channel(32);
    let listener = PeerConnection::new(&conf, (tx_sig_outbound, rx_sig_inbound))?;

    let mut input = std::env::args().skip(1);

    let signaling_uri = input.next().unwrap();
    let peer_to_dial = input.next();
    info!("Trying to connect to {}", signaling_uri);

    let (mut write, mut read) = connect_async(&signaling_uri).await?.0.split();

    let f_write = async move {
        while let Some(m) = rx_sig_outbound.recv().await {
            let mut value = serde_json::to_value(m.1).unwrap();
            value
                .as_object_mut()
                .unwrap()
                .insert("id".into(), serde_json::Value::String(m.0.to_string()));
            let s = serde_json::to_string(&value).unwrap();
            debug!("Sending {:?}", s);
            write.send(tungstenite::Message::text(s)).await.unwrap();
        }
        anyhow::Result::<_, anyhow::Error>::Ok(())
    };
    tokio::spawn(f_write);
    let f_read = async move {
        while let Some(Ok(m)) = read.next().await {
            debug!("received {:?}", m);
            if let Some(val) = match m {
                tungstenite::Message::Text(t) => {
                    Some(serde_json::from_str::<serde_json::Value>(&t).unwrap())
                }
                tungstenite::Message::Binary(b) => Some(serde_json::from_slice(&b[..]).unwrap()),
                tungstenite::Message::Close(_) => panic!(),
                _ => None,
            } {
                let id: PeerId = val
                    .as_object()
                    .unwrap()
                    .get("id")
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_string()
                    .into();
                let c: Message = serde_json::from_value(val).unwrap();
                println!("msg {:?} {:?}", id, c);
                if tx_sig_inbound.send((id, c)).await.is_err() {
                    panic!()
                }
            }
        }
        anyhow::Result::<_, anyhow::Error>::Ok(())
    };

    tokio::spawn(f_read);
    let mut dc = if let Some(peer_to_dial) = peer_to_dial {
        let mut dc = listener.dial(peer_to_dial.into(), "whatever").await?;
        info!("dial succeed");

        dc.write(b"Ping").await?;
        dc
    } else {
        let dc = listener.accept().await?;
        info!("accept succeed");
        dc
    };
    let mut buf = vec![0; 32];

    loop {
        let n = dc.read(&mut buf).await?;
        println!("Read: \"{}\"", String::from_utf8_lossy(&buf[..n]));
        dc.write(b"Ping").await?;
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
