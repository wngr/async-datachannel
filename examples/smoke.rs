use std::time::Duration;

use async_datachannel_rs::{ConnectionMessage, Listener, RtcConfig};
use async_tungstenite::{tokio::connect_async, tungstenite::Message};
use futures::{SinkExt, StreamExt};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::mpsc,
};
use tracing::{debug, info};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let ice_servers = vec!["stun:stun.l.google.com:19302"];
    let conf = RtcConfig::new(&ice_servers);
    let (tx_sig_outbound, mut rx_sig_outbound) = mpsc::channel(32);
    let (tx_sig_inbound, rx_sig_inbound) = mpsc::channel(32);
    let mut listener = Listener::new(&conf, (tx_sig_outbound, rx_sig_inbound))?;

    let mut input = std::env::args().skip(1);

    let signaling_uri = input.next().unwrap();
    let other_peer_name = input.next().unwrap();

    let initiator = input.next().is_some();
    info!("Trying to connect to {}", signaling_uri);

    let (mut write, mut read) = connect_async(&signaling_uri).await?.0.split();

    let o = other_peer_name.clone();
    let f_write = async move {
        while let Some(m) = rx_sig_outbound.recv().await {
            let mut value = serde_json::to_value(m)?;
            value
                .as_object_mut()
                .unwrap()
                .insert("id".into(), serde_json::Value::String(o.clone()));
            let s = serde_json::to_string(&value)?;
            debug!("Sending {:?}", s);
            write.send(Message::text(s)).await?;
        }
        anyhow::Result::<_, anyhow::Error>::Ok(())
    };
    tokio::spawn(f_write);
    let f_read = async move {
        while let Some(Ok(m)) = read.next().await {
            debug!("received {:?}", m);
            if let Some(c) = match m {
                Message::Text(t) => Some(serde_json::from_str::<ConnectionMessage>(&t).unwrap()),
                Message::Binary(b) => Some(serde_json::from_slice(&b[..])?),
                Message::Close(_) => panic!(),
                _ => None,
            } {
                if tx_sig_inbound.send(c).await.is_err() {
                    panic!()
                }
            }
        }
        anyhow::Result::<_, anyhow::Error>::Ok(())
    };

    tokio::spawn(f_read);
    let mut dc = if initiator {
        let mut dc = listener.dial("whatever", &other_peer_name).await?;
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
        if n > 0 {
            println!("Read: \"{}\"", String::from_utf8_lossy(&buf[..n]));
        }
        dc.write(b"Ping").await?;
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
