use super::super::link_metrics::Recorder;
use super::*;

#[tokio::test]
async fn link_metrics_websocket_counts_real_application_payloads_and_successful_sends() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let accept = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        tokio_tungstenite::accept_async(stream).await.unwrap()
    });
    let (mut client, _) = tokio_tungstenite::connect_async(format!("ws://{address}/fixture"))
        .await
        .unwrap();
    let mut server = accept.await.unwrap();
    let mut metrics = Recorder::disabled();
    assert_eq!(
        metrics.totals(),
        (0, 0),
        "HTTP handshake is not application traffic"
    );
    let frame = Frame::Reject {
        message: "offline 中文 fixture".into(),
    };
    let mut encoded = serde_json::to_vec(&frame).unwrap();
    encoded.push(b'\n');
    send_application(&mut client, &frame, &mut metrics)
        .await
        .unwrap();
    let sent = server.next().await.unwrap().unwrap();
    assert_eq!(sent.into_data().as_ref(), encoded.as_slice());
    assert_eq!(metrics.totals(), (encoded.len() as u64, 0));

    server
        .send(Message::Text(
            String::from_utf8(encoded.clone()).unwrap().into(),
        ))
        .await
        .unwrap();
    let received = client.next().await.unwrap().unwrap();
    assert!(matches!(
        application_frame(received, &mut metrics).unwrap(),
        Some(Frame::Reject { .. })
    ));
    server
        .send(Message::Binary(encoded.clone().into()))
        .await
        .unwrap();
    let received = client.next().await.unwrap().unwrap();
    assert!(application_frame(received, &mut metrics).unwrap().is_some());
    assert_eq!(
        metrics.totals(),
        (encoded.len() as u64, 2 * encoded.len() as u64)
    );

    server
        .send(Message::Ping(b"heartbeat".to_vec().into()))
        .await
        .unwrap();
    let received = client.next().await.unwrap().unwrap();
    assert!(matches!(received, Message::Ping(_)));
    assert!(application_frame(received, &mut metrics).unwrap().is_none());
    assert!(
        application_frame(Message::Pong(b"heartbeat".to_vec().into()), &mut metrics)
            .unwrap()
            .is_none()
    );
    let before = metrics.totals();
    server
        .send(Message::Binary(b"broken".to_vec().into()))
        .await
        .unwrap();
    let received = client.next().await.unwrap().unwrap();
    assert!(application_frame(received, &mut metrics).is_err());
    assert_eq!(
        metrics.totals(),
        (before.0, before.1 + 6),
        "received invalid payload still used the connection"
    );

    client.close(None).await.unwrap();
    let before = metrics.totals();
    assert!(
        send_application(&mut client, &frame, &mut metrics)
            .await
            .is_err()
    );
    assert_eq!(metrics.totals(), before, "a rejected send is never counted");
}
