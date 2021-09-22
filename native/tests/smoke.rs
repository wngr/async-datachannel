use std::{process::Stdio, sync::Arc};

use async_datachannel::{Message, PeerConnection, RtcConfig};
use async_tungstenite::{tokio::connect_async, tungstenite};
use futures::{
    future,
    io::{AsyncReadExt, AsyncWriteExt},
    SinkExt, StreamExt,
};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    sync::mpsc,
};
use tracing::{debug, info};

#[derive(Debug, Serialize, Deserialize)]
struct SignalingMessage {
    // id of the peer this messaged is supposed for
    id: String,
    payload: Message,
}

#[tokio::test]
/// Works with the signalling server from ../signaling-server
async fn smoke() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let mut cmd = Command::new("cargo");
    cmd.args(&["run"])
        .current_dir("../signaling-server")
        .stdout(Stdio::piped())
        .kill_on_drop(true);
    let mut server = cmd.spawn()?;
    let stdout = server.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout).lines();
    while let Some(line) = reader.next_line().await? {
        if line.starts_with("Listening on") {
            break;
        }
    }
    info!("Signaling server started!");

    let f_0 = run("ws://127.0.0.1:8000".into(), "other_peer".into(), None);
    let f_1 = run(
        "ws://127.0.0.1:8000".into(),
        "initiator".into(),
        Some("other_peer".into()),
    );
    future::try_join(f_0, f_1).await?;
    Ok(())
}

async fn run(
    signaling_uri: String,
    my_id: String,
    peer_to_dial: Option<String>,
) -> anyhow::Result<()> {
    let ice_servers = vec!["stun:stun.l.google.com:19302"];
    let conf = RtcConfig::new(&ice_servers);
    let (tx_sig_outbound, mut rx_sig_outbound) = mpsc::channel(32);
    let (tx_sig_inbound, rx_sig_inbound) = mpsc::channel(32);
    let listener = PeerConnection::new(&conf, (tx_sig_outbound, rx_sig_inbound))?;

    let signaling_uri = format!("{}/{}", signaling_uri, my_id);
    info!("Trying to connect to {}", signaling_uri);

    let (mut write, mut read) = connect_async(&signaling_uri).await?.0.split();

    let other_peer = Arc::new(Mutex::new(peer_to_dial.clone()));
    let other_peer_c = other_peer.clone();
    let f_write = async move {
        while let Some(m) = rx_sig_outbound.recv().await {
            let m = SignalingMessage {
                payload: m,
                id: other_peer_c.lock().as_ref().cloned().unwrap().to_string(),
            };
            let s = serde_json::to_string(&m).unwrap();
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
                let c: SignalingMessage = serde_json::from_value(val).unwrap();
                println!("msg {:?}", c);
                other_peer.lock().replace(c.id);
                if tx_sig_inbound.send(c.payload).await.is_err() {
                    panic!()
                }
            }
        }
        anyhow::Result::<_, anyhow::Error>::Ok(())
    };

    tokio::spawn(f_read);
    let mut buf = [0; 10];
    if peer_to_dial.is_some() {
        let mut dc = listener.dial("whatever").await?;
        info!("dial succeed");

        dc.write(b"Ping").await?;
        let n = dc.read(&mut buf).await?;
        assert_eq!(b"Pong", &buf[..n]);
    } else {
        let mut dc = listener.accept().await?;
        info!("accept succeed");

        let n = dc.read(&mut buf).await?;
        assert_eq!(b"Ping", &buf[..n]);
        dc.write(b"Pong").await?;
    };
    Ok(())
}
