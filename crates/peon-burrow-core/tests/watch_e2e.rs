//! watch 模式的端到端测试（只在 `--features imap-watch` 下编译）。
//!
//! 对应 `ai-docs/04-parity-node-to-rust.md § 6.2` 第 10–12 条：
//! 状态机全流程、未标记问候、致命错误不重试。

#![cfg(feature = "imap-watch")]

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use peon_burrow_core::{PolicyRules, RelayOptions, RelayServer, TlsConfig};
use peon_burrow_testkit::imap::{self, Step};
use tokio::net::TcpStream;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn spawn_relay(token: Option<&str>) -> RelayServer {
    RelayServer::start_with(
        RelayOptions::on("127.0.0.1", 0),
        Arc::new(PolicyRules::new(
            token.map(str::to_owned),
            vec!["127.0.0.1".to_owned()],
        )),
        TlsConfig::default(),
    )
    .await
    .expect("start relay")
}

fn watch_request(port: u16, token: Option<&str>) -> String {
    let token = token.unwrap_or("");
    format!(
        r#"{{"__watch":1,"accountId":"acc_1","host":"127.0.0.1","port":{port},"tls":false,"user":"me@qq.com","pass":"pw","token":"{token}"}}"#
    )
}

/// 连上中继（watch 的 URL **故意不带目标**），并把请求作为第一帧发出去。
async fn start_watch(relay: &RelayServer, request: String) -> Client {
    let url = format!("ws://{}/", relay.local_addr());
    let (mut ws, _) = tokio::time::timeout(
        Duration::from_secs(5),
        tokio_tungstenite::connect_async(url),
    )
    .await
    .expect("connect within timeout")
    .expect("ws connect");
    ws.send(Message::text(request))
        .await
        .expect("send watch request");
    ws
}

/// 等一条包含 `needle` 的文本帧。
async fn wait_for(ws: &mut Client, needle: &str, timeout: Duration) -> Option<String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match tokio::time::timeout(remaining, ws.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                if text.as_str().contains(needle) {
                    return Some(text.as_str().to_owned());
                }
            }
            Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => continue,
            Ok(Some(Ok(Message::Close(Some(frame))))) => {
                return Some(format!("__closed:{}", u16::from(frame.code)));
            }
            _ => return None,
        }
    }
}

#[tokio::test]
async fn watching_then_new_mail_is_pushed() {
    let imap = imap::spawn(vec![
        Step::send("* OK [CAPABILITY IMAP4rev1] ready"),
        Step::expect("LOGIN"),
        Step::send("A0001 OK LOGIN completed"),
        Step::expect("SELECT INBOX"),
        Step::send("* 3 EXISTS"),
        Step::send("A0002 OK [READ-WRITE] SELECT completed"),
        Step::expect("IDLE"),
        Step::send("+ idling"),
        Step::delay_ms(200),
        Step::send("* 4 EXISTS"),
    ])
    .await;

    let relay = spawn_relay(None).await;
    let mut ws = start_watch(&relay, watch_request(imap.port(), None)).await;

    let watching = wait_for(&mut ws, r#""state":"watching""#, Duration::from_secs(5))
        .await
        .expect("should report watching");
    assert!(
        watching.contains(r#""exists":3"#),
        "baseline should be the SELECT value: {watching}"
    );

    let mail = wait_for(&mut ws, r#""type":"mail""#, Duration::from_secs(5))
        .await
        .expect("should push new mail");
    assert!(
        mail.contains(r#""accountId":"acc_1""#),
        "account id must be echoed: {mail}"
    );
    assert!(mail.contains(r#""exists":4"#), "new count: {mail}");

    relay.stop().await.expect("stop");
}

#[tokio::test]
async fn the_existing_mail_is_not_pushed_as_new() {
    let imap = imap::spawn(vec![
        Step::send("* OK ready"),
        Step::expect("LOGIN"),
        Step::send("A0001 OK"),
        Step::expect("SELECT INBOX"),
        Step::send("* 7 EXISTS"),
        Step::send("A0002 OK"),
        Step::expect("IDLE"),
        Step::send("+ idling"),
        Step::delay_ms(400),
    ])
    .await;

    let relay = spawn_relay(None).await;
    let mut ws = start_watch(&relay, watch_request(imap.port(), None)).await;

    let watching = wait_for(&mut ws, r#""state":"watching""#, Duration::from_secs(5))
        .await
        .expect("watching");
    assert!(watching.contains(r#""exists":7"#));

    // 基准值之后没有新的 EXISTS → 不该有任何 mail 推送
    assert!(
        wait_for(&mut ws, r#""type":"mail""#, Duration::from_millis(600))
            .await
            .is_none(),
        "existing mail must not be pushed"
    );

    relay.stop().await.expect("stop");
}

#[tokio::test]
async fn a_fatal_login_failure_stops_the_watch() {
    let imap = imap::spawn(vec![
        Step::send("* OK ready"),
        Step::expect("LOGIN"),
        Step::send("A0001 NO [AUTHENTICATIONFAILED] bad credentials"),
    ])
    .await;

    let relay = spawn_relay(None).await;
    let mut ws = start_watch(&relay, watch_request(imap.port(), None)).await;

    let failed = wait_for(&mut ws, r#""state":"failed""#, Duration::from_secs(5))
        .await
        .expect("should report a fatal failure");
    assert!(
        failed.contains("登录失败"),
        "message should be human readable: {failed}"
    );

    // 致命错误**不重连**：密码错时快速重连会把账号锁掉
    assert!(
        wait_for(
            &mut ws,
            r#""state":"reconnecting""#,
            Duration::from_millis(800)
        )
        .await
        .is_none(),
        "a fatal failure must not schedule a reconnect"
    );

    relay.stop().await.expect("stop");
}

#[tokio::test]
async fn a_bye_greeting_is_fatal() {
    let imap = imap::spawn(vec![Step::send("* BYE server shutting down")]).await;

    let relay = spawn_relay(None).await;
    let mut ws = start_watch(&relay, watch_request(imap.port(), None)).await;

    let failed = wait_for(&mut ws, r#""state":"failed""#, Duration::from_secs(5))
        .await
        .expect("should fail");
    assert!(failed.contains("问候失败"), "message: {failed}");

    relay.stop().await.expect("stop");
}

#[tokio::test]
async fn a_server_close_leads_to_a_reconnect_attempt() {
    let imap = imap::spawn(vec![
        Step::send("* OK ready"),
        Step::expect("LOGIN"),
        Step::Close,
    ])
    .await;

    let relay = spawn_relay(None).await;
    let mut ws = start_watch(&relay, watch_request(imap.port(), None)).await;

    let reconnecting = wait_for(&mut ws, r#""state":"reconnecting""#, Duration::from_secs(6))
        .await
        .expect("should schedule a reconnect");
    assert!(
        reconnecting.contains(r#""retryInMs""#),
        "retryInMs is part of the contract: {reconnecting}"
    );

    relay.stop().await.expect("stop");
}

#[tokio::test]
async fn a_bad_token_fails_with_a_message_then_closes() {
    let imap = imap::spawn(vec![Step::send("* OK ready")]).await;

    let relay = spawn_relay(Some("s3cret")).await;
    let mut ws = start_watch(&relay, watch_request(imap.port(), None)).await;

    let failed = wait_for(&mut ws, r#""state":"failed""#, Duration::from_secs(5))
        .await
        .expect("should report the policy rejection");
    assert!(failed.contains("invalid token"), "message: {failed}");

    // 之后应当是 1011 关闭（watch 启动失败），而不是静默挂着
    let closed = wait_for(&mut ws, "__closed", Duration::from_secs(5)).await;
    assert_eq!(closed.as_deref(), Some("__closed:1011"));

    relay.stop().await.expect("stop");
}
