use async_datachannel_wasm::{Message, PeerConnection};
use futures::{
    future,
    io::{AsyncReadExt, AsyncWriteExt},
};
use log::{debug, info, Level};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use wasm_bindgen_test::{wasm_bindgen_test, wasm_bindgen_test_configure};

wasm_bindgen_test_configure!(run_in_browser);

#[derive(Debug, Serialize, Deserialize)]
struct SignalingMessage {
    // id of the peer this messaged is supposed for
    id: String,
    payload: Message,
}

#[wasm_bindgen_test]
async fn smoke() {
    console_log::init_with_level(Level::Trace).unwrap();
    let (tx_0, rx_0) = mpsc::channel(32);
    let (tx_1, rx_1) = mpsc::channel(32);

    let a = "initiator".to_string();
    let b = "franky".to_string();
    let f_0 = run(a.clone(), b.clone(), true, (tx_0, rx_1));
    let f_1 = run(b, a, false, (tx_1, rx_0));
    future::try_join(f_0, f_1).await.unwrap();
}

async fn run(
    my_id: String,
    other_peer: String,
    initiator: bool,
    (write, mut read): (
        mpsc::Sender<SignalingMessage>,
        mpsc::Receiver<SignalingMessage>,
    ),
) -> anyhow::Result<()> {
    let ice_servers = vec!["stun:stun.l.google.com:19302".into()];
    let (tx_sig_outbound, mut rx_sig_outbound) = mpsc::channel(32);
    let (tx_sig_inbound, rx_sig_inbound) = mpsc::channel(32);
    let listener = PeerConnection::new(ice_servers, (tx_sig_outbound, rx_sig_inbound))?;

    debug!("Starting up {}", my_id);
    let other_peer_c = other_peer.clone();
    let f_write = async move {
        while let Some(m) = rx_sig_outbound.recv().await {
            let m = SignalingMessage {
                payload: m,
                id: other_peer_c.clone(),
            };
            debug!("Sending {:?}", m);
            write.send(m).await.unwrap();
        }
        anyhow::Result::<_, anyhow::Error>::Ok(())
    };
    let f_read = async move {
        while let Some(c) = read.recv().await {
            println!("Received {:?}", c);
            if tx_sig_inbound.send(c.payload).await.is_err() {
                //panic!()
            }
        }
        anyhow::Result::<_, anyhow::Error>::Ok(())
    };

    let ping_pong = async move {
        let mut buf = [0; 10];
        if initiator {
            let mut dc = listener.dial("whatever").await?;
            info!("dial succeed");

            dc.write(b"Ping").await?;
            info!("wrote ping, waiting for pong");
            let n = dc.read(&mut buf).await?;
            assert_eq!(b"Pong", &buf[..n]);
        } else {
            debug!("Waiting for inbound connection");
            let mut dc = listener.accept().await?;
            info!("accept succeed");

            let n = dc.read(&mut buf).await?;
            assert_eq!(b"Ping", &buf[..n]);
            info!("received ping, sending pong");
            dc.write(b"Pong").await?;
        };
        anyhow::Result::<_, anyhow::Error>::Ok(())
    };
    future::try_join3(f_write, f_read, ping_pong).await?;
    Ok(())
}
