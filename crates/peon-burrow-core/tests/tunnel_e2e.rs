//! 透传隧道的端到端测试（**真 TCP**，全部在本机）。
//!
//! 覆盖 `ai-docs/04-parity-node-to-rust.md § 6.1` 的第 1–4、7–9 条；
//! TLS 相关的第 5、6 条要等 S6 的 `testkit`（rcgen 自签证书）补上。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use peon_burrow_core::{PolicyRules, RelayOptions, RelayServer, TlsConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// 测试里装上日志：挂住时能看出走到哪一步了（`--nocapture` 可见）。
fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_test_writer()
        .try_init();
}

/// 起一个「原样回写」的上游，返回它的地址。
async fn spawn_echo() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind echo");
    let addr = listener.local_addr().expect("echo addr");
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buffer = [0u8; 8192];
                loop {
                    match stream.read(&mut buffer).await {
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            if stream.write_all(&buffer[..read]).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }
    });
    addr
}

/// 起一个中继（`port: 0` 由内核分配，测试之间不会互相抢端口）。
async fn spawn_relay(allowed_hosts: Vec<String>) -> RelayServer {
    RelayServer::start_with(
        RelayOptions::on("127.0.0.1", 0),
        Arc::new(PolicyRules::new(None, allowed_hosts)),
        TlsConfig::default(),
    )
    .await
    .expect("start relay")
}

fn loopback() -> Vec<String> {
    vec!["127.0.0.1".to_owned()]
}

/// 带超时的客户端连接：**测试绝不允许无限等待**。
async fn connect_within(url: String, timeout: Duration) -> Result<Client, String> {
    match tokio::time::timeout(timeout, tokio_tungstenite::connect_async(url.clone())).await {
        Ok(Ok((ws, _))) => Ok(ws),
        Ok(Err(error)) => Err(error.to_string()),
        Err(_) => Err(format!("连接 {url} 超时")),
    }
}

async fn connect(relay: &RelayServer, target: &str) -> Client {
    let url = format!("ws://{}/{}", relay.local_addr(), target);
    connect_within(url, Duration::from_secs(5))
        .await
        .expect("ws connect")
}

/// 收下一帧二进制数据。
async fn next_binary(ws: &mut Client) -> Vec<u8> {
    loop {
        match tokio::time::timeout(Duration::from_secs(10), ws.next()).await {
            Ok(Some(Ok(Message::Binary(data)))) => return data.to_vec(),
            Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => continue,
            other => panic!("expected binary data, got {other:?}"),
        }
    }
}

/// 连上去，返回中继给出的关闭码（`None` = 没等到关闭帧）。
async fn close_code(url: String) -> Option<u16> {
    let Ok(mut ws) = connect_within(url, Duration::from_secs(5)).await else {
        return None;
    };
    loop {
        match tokio::time::timeout(Duration::from_secs(5), ws.next()).await {
            Ok(Some(Ok(Message::Close(Some(frame))))) => return Some(frame.code.into()),
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(_))) | Ok(None) | Err(_) => return None,
        }
    }
}

#[tokio::test]
async fn bytes_round_trip_unchanged() {
    init_tracing();
    let echo = spawn_echo().await;
    let relay = spawn_relay(loopback()).await;
    let mut ws = connect(&relay, &format!("{echo}?tls=0")).await;

    // 含 CRLF、NUL、高位字节：字节透传必须逐字节一致
    let payload = vec![
        0x00, 0xFF, 0x0D, 0x0A, 0x7F, b'A', b'0', b'0', b'0', b'1', b' ', b'L', b'O', b'G', b'I',
        b'N', 0x0D, 0x0A,
    ];
    ws.send(Message::Binary(payload.clone().into()))
        .await
        .expect("send");
    assert_eq!(next_binary(&mut ws).await, payload);

    relay.stop().await.expect("stop");
}

#[tokio::test]
async fn a_large_transfer_loses_nothing() {
    let echo = spawn_echo().await;
    let relay = spawn_relay(loopback()).await;
    let mut ws = connect(&relay, &format!("{echo}?tls=0")).await;

    let big: Vec<u8> = (0..256 * 1024).map(|index| (index % 251) as u8).collect();
    ws.send(Message::Binary(big.clone().into()))
        .await
        .expect("send");

    let mut received = Vec::with_capacity(big.len());
    while received.len() < big.len() {
        match tokio::time::timeout(Duration::from_secs(15), ws.next()).await {
            Ok(Some(Ok(Message::Binary(data)))) => received.extend_from_slice(&data),
            Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => continue,
            other => panic!(
                "unexpected while receiving {received_len} bytes: {other:?}",
                received_len = received.len()
            ),
        }
    }
    assert_eq!(received.len(), big.len());
    assert_eq!(received, big);

    relay.stop().await.expect("stop");
}

#[tokio::test]
async fn plaintext_on_the_implicit_tls_port_is_rejected() {
    let relay = spawn_relay(loopback()).await;
    let code = close_code(format!("ws://{}/127.0.0.1:993?tls=0", relay.local_addr())).await;
    assert_eq!(code, Some(1008));
    relay.stop().await.expect("stop");
}

#[tokio::test]
async fn hosts_outside_the_allow_list_are_rejected_before_connecting() {
    let relay = spawn_relay(loopback()).await;
    // 这个 host 根本不会被连接：策略检查在建连之前（I3/I4）
    let code = close_code(format!(
        "ws://{}/imap.example.com:993?tls=1",
        relay.local_addr()
    ))
    .await;
    assert_eq!(code, Some(1008));
    relay.stop().await.expect("stop");
}

#[tokio::test]
async fn a_text_frame_without_a_target_is_rejected() {
    let relay = spawn_relay(loopback()).await;
    let url = format!("ws://{}/", relay.local_addr());
    let mut ws = connect_within(url, Duration::from_secs(5))
        .await
        .expect("ws connect");
    ws.send(Message::text("A0001 LOGIN \"me\" \"pw\""))
        .await
        .expect("send");

    let code = loop {
        match tokio::time::timeout(Duration::from_secs(5), ws.next()).await {
            Ok(Some(Ok(Message::Close(Some(frame))))) => break Some(frame.code.into()),
            Ok(Some(Ok(_))) => continue,
            _ => break None,
        }
    };
    assert_eq!(code, Some(1008));
    relay.stop().await.expect("stop");
}

#[tokio::test]
async fn the_relay_survives_repeated_rejections() {
    let echo = spawn_echo().await;
    let relay = spawn_relay(loopback()).await;

    for _ in 0..5 {
        let _ = close_code(format!("ws://{}/127.0.0.1:993?tls=0", relay.local_addr())).await;
        let _ = close_code(format!(
            "ws://{}/imap.example.com:993?tls=1",
            relay.local_addr()
        ))
        .await;
    }

    // 中继仍然活着，而且新连接照常工作
    let mut ws = connect(&relay, &format!("{echo}?tls=0")).await;
    ws.send(Message::Binary(b"still alive".to_vec().into()))
        .await
        .expect("send");
    assert_eq!(next_binary(&mut ws).await, b"still alive".to_vec());

    // 策略拒绝是**正常运营**，不该污染 last_error；这里只要求进程还活着
    let state = relay.snapshot();
    assert!(state.running, "relay should still be running");
    assert_eq!(state.last_error, None, "policy rejections are not errors");

    relay.stop().await.expect("stop");
}

#[tokio::test]
async fn an_unreachable_upstream_is_reported_in_the_state() {
    // 先占一个端口再放掉，拿到一个「几乎肯定没人监听」的地址
    let dead = {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        listener.local_addr().expect("addr")
    };

    let relay = spawn_relay(loopback()).await;
    let code = close_code(format!("ws://{}/{}?tls=0", relay.local_addr(), dead)).await;
    assert_eq!(code, Some(1008));

    let state = relay.snapshot();
    assert!(
        state
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains(&dead.port().to_string())),
        "the failed upstream should be recorded, got {:?}",
        state.last_error
    );

    relay.stop().await.expect("stop");
}

#[tokio::test]
async fn two_instances_can_run_in_one_process() {
    // 布局铁律 L3 的验收：没有模块级全局状态
    let echo = spawn_echo().await;
    let first = spawn_relay(loopback()).await;
    let second = spawn_relay(loopback()).await;
    assert_ne!(first.local_addr(), second.local_addr());

    for relay in [&first, &second] {
        let mut ws = connect(relay, &format!("{echo}?tls=0")).await;
        ws.send(Message::Binary(b"ping".to_vec().into()))
            .await
            .expect("send");
        assert_eq!(next_binary(&mut ws).await, b"ping".to_vec());
    }

    first.stop().await.expect("stop first");
    second.stop().await.expect("stop second");
}

#[tokio::test]
async fn stopping_the_relay_refuses_new_connections() {
    let relay = spawn_relay(loopback()).await;
    let addr = relay.local_addr();
    relay.stop().await.expect("stop");

    // 监听套接字已关：新的连接必须失败（给内核一点时间回收）
    tokio::time::sleep(Duration::from_millis(200)).await;
    let attempt = connect_within(
        format!("ws://{addr}/127.0.0.1:993?tls=0"),
        Duration::from_secs(5),
    )
    .await;
    assert!(attempt.is_err(), "connect should fail after stop");
}

#[tokio::test]
async fn tls_handshake_happens_and_data_flows() {
    // 用 testkit 现场生成的 CA/叶子证书，通过 extra_roots 注入信任 —— 这条断言验证的是
    // 「中继确实做了 TLS 握手」，而不是「证书校验被关掉了」
    let certificates = peon_burrow_testkit::tls::certificates_for(&["localhost", "127.0.0.1"]);
    let echo = peon_burrow_testkit::tls::spawn_tls_echo(&certificates).await;

    let relay = RelayServer::start_with(
        RelayOptions::on("127.0.0.1", 0),
        Arc::new(PolicyRules::new(None, loopback())),
        TlsConfig::default().with_root(certificates.ca_der.clone()),
    )
    .await
    .expect("start relay");

    let mut ws = connect(&relay, &format!("{echo}?tls=1")).await;
    let command = b"A0001 CAPABILITY\r\n".to_vec();
    ws.send(Message::Binary(command.clone().into()))
        .await
        .expect("send");
    assert_eq!(next_binary(&mut ws).await, command, "TLS 隧道应原样回传");

    relay.stop().await.expect("stop");
}

#[tokio::test]
async fn a_self_signed_certificate_is_rejected_by_default() {
    let certificates = peon_burrow_testkit::tls::certificates_for(&["localhost", "127.0.0.1"]);
    let echo = peon_burrow_testkit::tls::spawn_tls_echo(&certificates).await;

    // 没有注入 CA：默认必须拒绝（这条守的是安全属性，功能测试看不出来）
    let relay = spawn_relay(loopback()).await;
    let code = close_code(format!("ws://{}/{}?tls=1", relay.local_addr(), echo)).await;
    assert_eq!(code, Some(1008));

    let state = relay.snapshot();
    let error = state.last_error.unwrap_or_default();
    assert!(
        error.contains("证书") || error.to_ascii_lowercase().contains("certificate"),
        "错误文案应说清是证书问题：{error}"
    );

    relay.stop().await.expect("stop");
}
