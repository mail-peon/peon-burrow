//! 自更新的端到端测试：**全程无外网**。
//!
//! 两种「假网络」：
//!
//! 1. [`TestServer`]：用 `tokio::net::TcpListener` 手写的小 HTTP 服务器（只解析请求行，
//!    回固定字节）。内置的 `get()` 走明文 `http://127.0.0.1:<port>`，于是
//!    **URL 策略、请求构造、响应解析**都被真实地跑了一遍；
//! 2. [`Fetcher`] 打桩：一个纯内存的抓取函数，连 socket 都不开 —— 证明
//!    `UpdateContext::fetcher` 这个注入点能把网络完全摘掉。
//!
//! 所有用例都在 `tempfile` 的临时目录里操作，**不碰任何真实安装目录**。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use peon_burrow_update::{
    ASSET_DOWNLOAD_SLACK, Applied, Asset, Channel, Fetcher, MAX_MANIFEST_BYTES, SignatureVerifier,
    UpdateContext, UpdateError, UpdateStatus, apply, check, get, host_triple, manifest_url,
    replace_binary, sha256_hex,
};
use semver::Version;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

// ---------------------------------------------------------------------------
// 小 HTTP 服务器（只用 tokio::net，无任何 http 依赖）
// ---------------------------------------------------------------------------

/// 一条路由要回什么。
#[derive(Debug, Clone)]
enum Reply {
    /// 200 + `Content-Length`。
    Body(Vec<u8>),
    /// 200 + `Transfer-Encoding: chunked`（分两块发，逼解码器真的解）。
    Chunked(Vec<u8>),
    /// 302 + `Location`。
    Redirect(String),
    /// 任意状态码，空包体。
    Status(u16),
}

impl Reply {
    /// 变成「响应头 + 包体」两段字节。
    fn render(&self) -> (String, Vec<u8>) {
        match self {
            Reply::Body(bytes) => (
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    bytes.len()
                ),
                bytes.clone(),
            ),
            Reply::Chunked(bytes) => {
                let (first, second) = bytes.split_at(bytes.len() / 2);
                let mut framed = Vec::new();
                for part in [first, second] {
                    framed.extend_from_slice(format!("{:x}\r\n", part.len()).as_bytes());
                    framed.extend_from_slice(part);
                    framed.extend_from_slice(b"\r\n");
                }
                framed.extend_from_slice(b"0\r\n\r\n");
                (
                    "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
                        .to_string(),
                    framed,
                )
            }
            Reply::Redirect(location) => (
                format!(
                    "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                ),
                Vec::new(),
            ),
            Reply::Status(status) => (
                format!("HTTP/1.1 {status} Nope\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
                Vec::new(),
            ),
        }
    }
}

/// 一次性的本地 HTTP 服务器，记录收到的所有路径。
struct TestServer {
    /// `http://127.0.0.1:<port>`（当作 `base_url` 用，替换 github.com 前缀）。
    base_url: String,
    /// 收到过的请求路径（按顺序）。
    seen: Arc<Mutex<Vec<String>>>,
    /// accept 循环。
    task: tokio::task::JoinHandle<()>,
}

impl TestServer {
    /// 起一个服务器，路由表是 `路径 → 回复`。
    async fn start(routes: Vec<(String, Reply)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("绑定本地端口");
        let addr = listener.local_addr().expect("本地地址");
        let routes: Arc<HashMap<String, Reply>> = Arc::new(routes.into_iter().collect());
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_in_task = Arc::clone(&seen);

        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let routes = Arc::clone(&routes);
                let seen = Arc::clone(&seen_in_task);
                tokio::spawn(async move {
                    // 只解析请求行（我们的客户端一个请求只发这几个头，一次 read 够用）
                    let mut buf = vec![0u8; 8192];
                    let read = socket.read(&mut buf).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..read]).into_owned();
                    let path = request
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("/")
                        .to_string();
                    seen.lock().expect("seen 锁").push(path.clone());

                    let (head, body) = routes
                        .get(&path)
                        .cloned()
                        .unwrap_or(Reply::Status(404))
                        .render();
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(&body).await;
                    let _ = socket.shutdown().await;
                });
            }
        });

        Self {
            base_url: format!("http://{addr}"),
            seen,
            task,
        }
    }

    /// 起一个「什么都不回」的服务器（用于断言「一个请求都不该发」）。
    async fn empty() -> Self {
        Self::start(Vec::new()).await
    }

    /// 收到过的路径。
    fn seen(&self) -> Vec<String> {
        self.seen.lock().expect("seen 锁").clone()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

// ---------------------------------------------------------------------------
// 清单 / 资产 / 假二进制
// ---------------------------------------------------------------------------

/// 一条资产的描述（测试里手写，模拟 CI 生成的清单）。
struct Spec {
    target: String,
    name: String,
    size: u64,
    sha256: String,
    signature: Option<String>,
}

/// 按「真实字节」造一条合法的资产描述。
fn spec(name: &str, bytes: &[u8]) -> Spec {
    Spec {
        target: host_triple().expect("测试机应当是发布目标"),
        name: name.to_string(),
        size: bytes.len() as u64,
        sha256: sha256_hex(bytes),
        signature: Some("untrusted comment: minisign signature\nstub".to_string()),
    }
}

/// 造一份清单 JSON。
fn manifest_json(version: &str, channel: &str, assets: &[Spec]) -> Vec<u8> {
    let assets: Vec<serde_json::Value> = assets
        .iter()
        .map(|spec| {
            serde_json::json!({
                "target": spec.target,
                "name": spec.name,
                "size": spec.size,
                "sha256": spec.sha256,
                "signature": spec.signature,
            })
        })
        .collect();
    serde_json::to_vec(&serde_json::json!({
        "schema": 1,
        "version": version,
        "channel": channel,
        "releasedAt": "2026-10-09T04:00:00Z",
        "notesUrl": "https://github.com/mail-peon/peon-burrow/releases/tag/v0.2.0",
        "assets": assets,
    }))
    .expect("清单一定是合法 JSON")
}

/// 清单在本地服务器上对应的路径（= `Channel::manifest_path()`）。
fn manifest_path(channel: Channel) -> String {
    channel.manifest_path().to_string()
}

/// 资产在本地服务器上对应的路径（与清单同一个目录，只换文件名）。
fn asset_path(channel: Channel, name: &str) -> String {
    channel.manifest_path().replace("latest.json", name)
}

/// 安装目录里那个「正在跑的自己」叫什么。
fn installed_name() -> &'static str {
    if cfg!(windows) {
        "burrow.exe"
    } else {
        "burrow"
    }
}

/// 造一个**真的能跑**的假二进制：Windows 上是 `.cmd`，其它平台上是 `sh` 脚本。
///
/// 它实现了冒烟测试的契约：`<exe> version --json` → `{"version":"…"}`。
fn fake_binary(stem: &str, reported: Option<&str>, exit_code: i32) -> (String, Vec<u8>) {
    let line = reported.unwrap_or("0.0.0");
    if cfg!(windows) {
        (
            format!("{stem}.cmd"),
            format!("@echo off\r\n@echo {{\"version\":\"{line}\"}}\r\n@exit /b {exit_code}\r\n")
                .into_bytes(),
        )
    } else {
        (
            stem.to_string(),
            format!("#!/bin/sh\necho '{{\"version\":\"{line}\"}}'\nexit {exit_code}\n")
                .into_bytes(),
        )
    }
}

/// 一个「什么都不校验」的验签器，但会记录自己被调用了几次、看到什么。
#[derive(Debug, Default)]
struct RecordingVerifier {
    calls: Mutex<Vec<(String, usize, bool)>>,
}

impl SignatureVerifier for RecordingVerifier {
    fn verify(&self, asset: &Asset, bytes: &[u8]) -> Result<(), String> {
        self.calls.lock().expect("calls 锁").push((
            asset.name.clone(),
            bytes.len(),
            asset.signature.is_some(),
        ));
        Ok(())
    }
}

/// 永远拒绝的验签器。
#[derive(Debug)]
struct RejectingVerifier;

impl SignatureVerifier for RejectingVerifier {
    fn verify(&self, _asset: &Asset, _bytes: &[u8]) -> Result<(), String> {
        Err("签名里的公钥不在 pubkeys 里（测试）".to_string())
    }
}

/// 在临时目录里放一个「旧二进制」，并返回 `(临时目录, 目标路径)`。
fn install_dir_with_binary() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("临时目录");
    let target = dir.path().join(installed_name());
    std::fs::write(&target, b"old binary").expect("写旧二进制");
    (dir, target)
}

/// 组一个指向本地服务器的上下文。
fn context(server: &TestServer, dir: &Path, current: &str) -> UpdateContext {
    let mut ctx = UpdateContext::new(
        Version::parse(current).expect("语义化版本"),
        dir.to_path_buf(),
    );
    ctx.base_url = Some(server.base_url.clone());
    ctx.binary_name = Some(installed_name().to_string());
    ctx.require_signature = false;
    ctx
}

// ---------------------------------------------------------------------------
// 内置 HTTP 实现（真实地打本地明文服务器）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_reads_a_plain_body() {
    let body = b"{\"hello\":\"world\"}".to_vec();
    let server = TestServer::start(vec![("/plain".to_string(), Reply::Body(body.clone()))]).await;

    let got = get(&format!("{}/plain", server.base_url), 1024, None)
        .await
        .expect("读得到");
    assert_eq!(got, body);
    assert_eq!(server.seen(), vec!["/plain".to_string()]);
}

#[tokio::test]
async fn get_decodes_chunked_bodies() {
    let body = b"0123456789abcdefghijklmnopqrstuvwxyz".to_vec();
    let server =
        TestServer::start(vec![("/chunked".to_string(), Reply::Chunked(body.clone()))]).await;

    let got = get(&format!("{}/chunked", server.base_url), 4096, None)
        .await
        .expect("分块也能读");
    assert_eq!(got, body);
}

#[tokio::test]
async fn get_follows_redirects() {
    let body = b"redirected manifest".to_vec();
    let server = TestServer::start(vec![
        ("/start".to_string(), Reply::Redirect("/final".to_string())),
        ("/final".to_string(), Reply::Body(body.clone())),
    ])
    .await;

    let got = get(&format!("{}/start", server.base_url), 1024, None)
        .await
        .expect("跟一次重定向");
    assert_eq!(got, body);
    assert_eq!(
        server.seen(),
        vec!["/start".to_string(), "/final".to_string()]
    );
}

#[tokio::test]
async fn get_reports_status_codes_and_body_limits() {
    let server = TestServer::start(vec![
        ("/missing".to_string(), Reply::Status(404)),
        ("/big".to_string(), Reply::Body(vec![b'x'; 4096])),
    ])
    .await;

    let status = get(&format!("{}/missing", server.base_url), 1024, None)
        .await
        .expect_err("404 就是错误");
    assert!(
        matches!(status, UpdateError::HttpStatus { status: 404, .. }),
        "{status}"
    );

    let too_big = get(&format!("{}/big", server.base_url), 128, None)
        .await
        .expect_err("超过上限要断开");
    assert!(
        matches!(too_big, UpdateError::BodyTooLarge { .. }),
        "{too_big}"
    );
}

#[tokio::test]
async fn get_rejects_proxies_and_bad_urls_loudly() {
    let proxy = get("http://127.0.0.1:1/x", 16, Some("http://proxy:3128"))
        .await
        .expect_err("本版不支持代理");
    assert!(
        matches!(proxy, UpdateError::ProxyUnsupported { .. }),
        "{proxy}"
    );

    let bad = get("ftp://a.b/c", 16, None)
        .await
        .expect_err("不支持的 scheme");
    assert!(matches!(bad, UpdateError::BadUrl { .. }), "{bad}");
}

// ---------------------------------------------------------------------------
// check
// ---------------------------------------------------------------------------

#[tokio::test]
async fn check_finds_the_update_for_this_platform_and_uses_the_mirror_prefix() {
    let (name, _) = fake_binary("burrow-next", Some("0.2.0"), 0);
    let spec = spec(&name, b"whatever");
    let server = TestServer::start(vec![(
        manifest_path(Channel::Stable),
        Reply::Body(manifest_json("0.2.0", "stable", &[spec])),
    )])
    .await;

    let dir = tempfile::tempdir().expect("临时目录");
    let ctx = context(&server, dir.path(), "0.1.0");
    let status = check(&ctx).await.expect("应当发现新版本");

    assert!(status.available);
    assert_eq!(status.current, Version::new(0, 1, 0));
    assert_eq!(status.latest, Version::new(0, 2, 0));
    assert_eq!(status.asset.expect("有资产").name, name);

    // URL 策略：只打固定清单路径，**绝不碰 GitHub API**
    let seen = server.seen();
    assert_eq!(seen, vec![manifest_path(Channel::Stable)]);
    assert!(!seen[0].contains("/api/"), "{seen:?}");
    assert_eq!(
        ctx.manifest_url(),
        format!(
            "{}{}",
            server.base_url, "/mail-peon/peon-burrow/releases/latest/download/latest.json"
        )
    );
    assert_eq!(
        manifest_url(None, Channel::Stable),
        "https://github.com/mail-peon/peon-burrow/releases/latest/download/latest.json"
    );
}

#[tokio::test]
async fn check_says_nothing_to_do_for_the_same_version() {
    let (name, _) = fake_binary("burrow-next", Some("0.2.0"), 0);
    let spec = spec(&name, b"whatever");
    let server = TestServer::start(vec![(
        manifest_path(Channel::Stable),
        Reply::Body(manifest_json("0.2.0", "stable", &[spec])),
    )])
    .await;

    let dir = tempfile::tempdir().expect("临时目录");
    let ctx = context(&server, dir.path(), "0.2.0");
    let status = check(&ctx).await.expect("同版本不算错误");

    assert!(!status.available);
    assert!(status.asset.is_none());
    assert_eq!(status.latest, Version::new(0, 2, 0));
    // 没有更新就不该去下资产
    assert_eq!(server.seen(), vec![manifest_path(Channel::Stable)]);
}

#[tokio::test]
async fn check_never_downgrades() {
    let (name, _) = fake_binary("burrow-next", Some("0.2.0"), 0);
    let spec = spec(&name, b"whatever");
    let server = TestServer::start(vec![(
        manifest_path(Channel::Stable),
        Reply::Body(manifest_json("0.2.0", "stable", &[spec])),
    )])
    .await;

    let dir = tempfile::tempdir().expect("临时目录");
    // 本地比清单还新（渠道从 beta 切回 stable 时会遇到）
    let ctx = context(&server, dir.path(), "0.3.0");
    let status = check(&ctx).await.expect("不降级不是错误");

    assert!(!status.available, "本地更新时绝不回退");
    assert!(status.asset.is_none());
    assert_eq!(status.current, Version::new(0, 3, 0));
    assert_eq!(status.latest, Version::new(0, 2, 0));
}

#[tokio::test]
async fn check_rejects_a_manifest_from_another_channel() {
    let (name, _) = fake_binary("burrow-next", Some("0.2.0"), 0);
    let spec = spec(&name, b"whatever");
    let server = TestServer::start(vec![(
        manifest_path(Channel::Stable),
        Reply::Body(manifest_json("0.2.0", "beta", &[spec])),
    )])
    .await;

    let dir = tempfile::tempdir().expect("临时目录");
    let ctx = context(&server, dir.path(), "0.1.0");
    let error = check(&ctx).await.expect_err("渠道不符要整份作废");

    assert!(
        matches!(error, UpdateError::ChannelMismatch { ref manifest, .. } if manifest == "beta"),
        "{error}"
    );
}

#[tokio::test]
async fn check_reports_no_asset_for_this_platform_and_downloads_nothing() {
    let other = Spec {
        target: "riscv64gc-unknown-linux-gnu".to_string(),
        name: "burrow-riscv64".to_string(),
        size: 10,
        sha256: "a".repeat(64),
        signature: None,
    };
    let server = TestServer::start(vec![(
        manifest_path(Channel::Stable),
        Reply::Body(manifest_json("0.2.0", "stable", &[other])),
    )])
    .await;

    let dir = tempfile::tempdir().expect("临时目录");
    let ctx = context(&server, dir.path(), "0.1.0");
    let error = check(&ctx).await.expect_err("本平台没有资产");

    assert!(
        matches!(error, UpdateError::NoAssetForTarget { .. }),
        "{error}"
    );
    assert!(error.to_string().contains("本平台暂无更新"), "{error}");
    assert!(error.is_platform_unsupported());
    // 只问了清单，**没有下载任何东西**
    assert_eq!(server.seen(), vec![manifest_path(Channel::Stable)]);
}

#[tokio::test]
async fn check_surfaces_a_missing_manifest() {
    let server = TestServer::empty().await;
    let dir = tempfile::tempdir().expect("临时目录");
    let ctx = context(&server, dir.path(), "0.1.0");

    let error = check(&ctx).await.expect_err("清单 404");
    assert!(
        matches!(error, UpdateError::HttpStatus { status: 404, .. }),
        "{error}"
    );
    assert!(!error.is_platform_unsupported(), "404 值得退避重试");
}

// ---------------------------------------------------------------------------
// apply：happy path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn apply_swaps_the_binary_verifies_the_signature_and_cleans_staging() {
    let (name, bytes) = fake_binary("burrow-next", Some("0.2.0"), 0);
    let spec = spec(&name, &bytes);
    let server = TestServer::start(vec![
        (
            manifest_path(Channel::Stable),
            Reply::Body(manifest_json("0.2.0", "stable", &[spec])),
        ),
        (
            asset_path(Channel::Stable, &name),
            Reply::Body(bytes.clone()),
        ),
    ])
    .await;

    let (dir, target) = install_dir_with_binary();
    let mut ctx = context(&server, dir.path(), "0.1.0");
    ctx.require_signature = true;
    ctx.pubkeys = vec!["RWQ...（测试公钥）".to_string()];
    let verifier = Arc::new(RecordingVerifier::default());
    ctx.verifier = Some(verifier.clone());
    ctx.proxy = None;

    let applied: Applied = apply(&ctx).await.expect("更新应当成功");

    assert_eq!(applied.from, Version::new(0, 1, 0));
    assert_eq!(applied.to, Version::new(0, 2, 0));
    assert_eq!(applied.asset, name);

    // 文件真的换了
    assert_eq!(std::fs::read(&target).expect("读新二进制"), bytes);
    // 验签器被调用过，而且看到的是「有签名」的资产
    let calls = verifier.calls.lock().expect("calls 锁").clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, name);
    assert_eq!(calls[0].1, bytes.len());
    assert!(calls[0].2, "清单里有 signature");

    // staging 清干净；安装目录里不留任何 .old-*
    let staged_dir = ctx.staging_root().join("0.2.0");
    assert!(!staged_dir.exists(), "staging 该被删掉");
    let leftovers: Vec<String> = std::fs::read_dir(dir.path())
        .expect("读安装目录")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".old-"))
        .collect();
    assert!(leftovers.is_empty(), "遗留：{leftovers:?}");

    // 清单与资产都是从镜像前缀（本地服务器）拿的，路径就是 URL 策略算出来的那条
    assert_eq!(
        server.seen(),
        vec![
            manifest_path(Channel::Stable),
            asset_path(Channel::Stable, &name)
        ]
    );
}

#[tokio::test]
async fn apply_reports_no_update_instead_of_downgrading() {
    let (name, bytes) = fake_binary("burrow-next", Some("0.2.0"), 0);
    let spec = spec(&name, &bytes);
    let server = TestServer::start(vec![(
        manifest_path(Channel::Stable),
        Reply::Body(manifest_json("0.2.0", "stable", &[spec])),
    )])
    .await;

    let (dir, target) = install_dir_with_binary();
    let ctx = context(&server, dir.path(), "0.3.0");
    let error = apply(&ctx).await.expect_err("没有更新可应用");

    assert!(
        matches!(error, UpdateError::NoUpdateAvailable { .. }),
        "{error}"
    );
    assert_eq!(std::fs::read(&target).expect("旧二进制还在"), b"old binary");
}

// ---------------------------------------------------------------------------
// apply：四道闸逐条拦下
// ---------------------------------------------------------------------------

#[tokio::test]
async fn apply_rejects_a_size_mismatch_and_keeps_the_old_binary() {
    let (name, bytes) = fake_binary("burrow-next", Some("0.2.0"), 0);
    let mut spec = spec(&name, &bytes);
    spec.size += 7; // 清单说比实际大（模拟被截断 / 清单写错）
    let server = TestServer::start(vec![
        (
            manifest_path(Channel::Stable),
            Reply::Body(manifest_json("0.2.0", "stable", &[spec])),
        ),
        (
            asset_path(Channel::Stable, &name),
            Reply::Body(bytes.clone()),
        ),
    ])
    .await;

    let (dir, target) = install_dir_with_binary();
    let ctx = context(&server, dir.path(), "0.1.0");
    let error = apply(&ctx).await.expect_err("size 不符必须拒绝");

    match error {
        UpdateError::SizeMismatch { expected, actual } => {
            assert_eq!(expected, bytes.len() as u64 + 7);
            assert_eq!(actual, bytes.len() as u64);
        }
        other => panic!("期望 SizeMismatch，得到 {other:?}"),
    }
    assert_eq!(std::fs::read(&target).expect("旧二进制还在"), b"old binary");
    assert!(!ctx.staging_root().join("0.2.0").exists(), "半成品要删掉");
}

#[tokio::test]
async fn apply_rejects_a_sha256_mismatch_and_keeps_the_old_binary() {
    let (name, bytes) = fake_binary("burrow-next", Some("0.2.0"), 0);
    let mut spec = spec(&name, &bytes);
    spec.sha256 = "b".repeat(64); // 大小对、内容对不上
    let server = TestServer::start(vec![
        (
            manifest_path(Channel::Stable),
            Reply::Body(manifest_json("0.2.0", "stable", &[spec])),
        ),
        (
            asset_path(Channel::Stable, &name),
            Reply::Body(bytes.clone()),
        ),
    ])
    .await;

    let (dir, target) = install_dir_with_binary();
    let ctx = context(&server, dir.path(), "0.1.0");
    let error = apply(&ctx).await.expect_err("校验和不匹配必须拒绝");

    assert!(
        matches!(error, UpdateError::Sha256Mismatch { .. }),
        "{error}"
    );
    assert!(error.to_string().contains("校验和不匹配"), "{error}");
    assert_eq!(std::fs::read(&target).expect("旧二进制还在"), b"old binary");
}

#[tokio::test]
async fn apply_rejects_a_malformed_sha256() {
    let (name, bytes) = fake_binary("burrow-next", Some("0.2.0"), 0);
    let mut spec = spec(&name, &bytes);
    spec.sha256 = "not-a-hash".to_string();
    let server = TestServer::start(vec![
        (
            manifest_path(Channel::Stable),
            Reply::Body(manifest_json("0.2.0", "stable", &[spec])),
        ),
        (
            asset_path(Channel::Stable, &name),
            Reply::Body(bytes.clone()),
        ),
    ])
    .await;

    let (dir, target) = install_dir_with_binary();
    let ctx = context(&server, dir.path(), "0.1.0");
    let error = apply(&ctx).await.expect_err("清单自己不可信");

    assert!(
        matches!(error, UpdateError::InvalidSha256 { .. }),
        "{error}"
    );
    assert_eq!(std::fs::read(&target).expect("旧二进制还在"), b"old binary");
}

#[tokio::test]
async fn apply_fails_closed_when_a_signature_is_required_without_a_verifier() {
    let server = TestServer::empty().await;
    let (dir, target) = install_dir_with_binary();

    let mut ctx = context(&server, dir.path(), "0.1.0");
    ctx.require_signature = true; // 要验签，但没注入 verifier
    let error = apply(&ctx).await.expect_err("必须 fail closed");

    assert!(
        matches!(error, UpdateError::SignatureVerifierMissing),
        "{error}"
    );
    assert!(error.to_string().contains("验签"), "{error}");
    // 连清单都不去拉：拒绝发生在任何网络动作之前
    assert!(server.seen().is_empty(), "{:?}", server.seen());
    assert_eq!(std::fs::read(&target).expect("旧二进制还在"), b"old binary");
}

#[tokio::test]
async fn apply_rejects_an_invalid_signature() {
    let (name, bytes) = fake_binary("burrow-next", Some("0.2.0"), 0);
    let spec = spec(&name, &bytes);
    let server = TestServer::start(vec![
        (
            manifest_path(Channel::Stable),
            Reply::Body(manifest_json("0.2.0", "stable", &[spec])),
        ),
        (
            asset_path(Channel::Stable, &name),
            Reply::Body(bytes.clone()),
        ),
    ])
    .await;

    let (dir, target) = install_dir_with_binary();
    let mut ctx = context(&server, dir.path(), "0.1.0");
    ctx.require_signature = true;
    ctx.verifier = Some(Arc::new(RejectingVerifier));
    let error = apply(&ctx).await.expect_err("验签失败必须拒绝");

    assert!(
        matches!(error, UpdateError::SignatureInvalid { .. }),
        "{error}"
    );
    assert!(error.to_string().contains("签名校验失败"), "{error}");
    assert_eq!(std::fs::read(&target).expect("旧二进制还在"), b"old binary");
}

#[tokio::test]
async fn apply_rejects_a_smoke_test_failure() {
    let (name, bytes) = fake_binary("burrow-next", None, 3); // 起得来，但退出码 3
    let spec = spec(&name, &bytes);
    let server = TestServer::start(vec![
        (
            manifest_path(Channel::Stable),
            Reply::Body(manifest_json("0.2.0", "stable", &[spec])),
        ),
        (
            asset_path(Channel::Stable, &name),
            Reply::Body(bytes.clone()),
        ),
    ])
    .await;

    let (dir, target) = install_dir_with_binary();
    let ctx = context(&server, dir.path(), "0.1.0");
    let error = apply(&ctx).await.expect_err("冒烟测试失败必须拒绝替换");

    assert!(
        matches!(error, UpdateError::SmokeTestFailed { .. }),
        "{error}"
    );
    assert_eq!(std::fs::read(&target).expect("旧二进制还在"), b"old binary");
}

#[tokio::test]
async fn apply_rejects_a_smoke_test_version_mismatch() {
    let (name, bytes) = fake_binary("burrow-next", Some("9.9.9"), 0);
    let spec = spec(&name, &bytes);
    let server = TestServer::start(vec![
        (
            manifest_path(Channel::Stable),
            Reply::Body(manifest_json("0.2.0", "stable", &[spec])),
        ),
        (
            asset_path(Channel::Stable, &name),
            Reply::Body(bytes.clone()),
        ),
    ])
    .await;

    let (dir, target) = install_dir_with_binary();
    let ctx = context(&server, dir.path(), "0.1.0");
    let error = apply(&ctx).await.expect_err("版本对不上必须拒绝替换");

    match &error {
        UpdateError::SmokeTestVersionMismatch {
            expected, actual, ..
        } => {
            assert_eq!(expected, "0.2.0");
            assert_eq!(actual, "9.9.9");
        }
        other => panic!("期望 SmokeTestVersionMismatch，得到 {other:?}"),
    }
    assert_eq!(std::fs::read(&target).expect("旧二进制还在"), b"old binary");
}

#[tokio::test]
async fn apply_can_skip_the_smoke_test_only_when_told_to() {
    // 这个「二进制」根本不是可执行文件：不跳过就必失败
    let name = if cfg!(windows) {
        "burrow-next.exe"
    } else {
        "burrow-next"
    };
    let bytes = b"not an executable".to_vec();
    let spec = spec(name, &bytes);
    let server = TestServer::start(vec![
        (
            manifest_path(Channel::Stable),
            Reply::Body(manifest_json("0.2.0", "stable", &[spec])),
        ),
        (
            asset_path(Channel::Stable, name),
            Reply::Body(bytes.clone()),
        ),
    ])
    .await;

    let (dir, target) = install_dir_with_binary();
    let mut ctx = context(&server, dir.path(), "0.1.0");

    let strict = apply(&ctx).await.expect_err("默认必须跑冒烟测试");
    assert!(
        matches!(
            strict,
            UpdateError::SmokeTestUnrunnable { .. } | UpdateError::SmokeTestFailed { .. }
        ),
        "{strict}"
    );

    ctx.skip_smoke_test = true; // 显式跳过（文档里写明生产不要开）
    let applied = apply(&ctx).await.expect("显式跳过后应当替换成功");
    assert_eq!(applied.to, Version::new(0, 2, 0));
    assert_eq!(std::fs::read(&target).expect("新二进制"), bytes);
}

// ---------------------------------------------------------------------------
// apply：格式与体积
// ---------------------------------------------------------------------------

#[tokio::test]
async fn apply_rejects_archive_names_before_downloading() {
    let bytes = b"PK\x03\x04 pretend zip".to_vec();
    let spec = spec("burrow-x86_64-pc-windows-msvc.zip", &bytes);
    let server = TestServer::start(vec![(
        manifest_path(Channel::Stable),
        Reply::Body(manifest_json("0.2.0", "stable", &[spec])),
    )])
    .await;

    let (dir, target) = install_dir_with_binary();
    let ctx = context(&server, dir.path(), "0.1.0");
    let error = apply(&ctx).await.expect_err("压缩包一律拒绝");

    assert!(
        matches!(error, UpdateError::UnsupportedAssetFormat { .. }),
        "{error}"
    );
    // 名字就露馅了 → 连资产都不去下
    assert_eq!(server.seen(), vec![manifest_path(Channel::Stable)]);
    assert_eq!(std::fs::read(&target).expect("旧二进制还在"), b"old binary");
}

#[tokio::test]
async fn apply_rejects_archives_detected_by_magic_bytes() {
    // 名字骗人（写着裸二进制），内容却是 zip → 必须在替换之前拦下
    let bytes = b"PK\x03\x04 not really a binary".to_vec();
    let spec = spec("burrow-next", &bytes);
    let server = TestServer::start(vec![
        (
            manifest_path(Channel::Stable),
            Reply::Body(manifest_json("0.2.0", "stable", &[spec])),
        ),
        (
            asset_path(Channel::Stable, "burrow-next"),
            Reply::Body(bytes.clone()),
        ),
    ])
    .await;

    let (dir, target) = install_dir_with_binary();
    let ctx = context(&server, dir.path(), "0.1.0");
    let error = apply(&ctx).await.expect_err("魔数是 zip");

    match error {
        UpdateError::UnsupportedAssetFormat { detail, .. } => {
            assert!(detail.contains("魔数"), "{detail}");
        }
        other => panic!("期望 UnsupportedAssetFormat，得到 {other:?}"),
    }
    assert_eq!(std::fs::read(&target).expect("旧二进制还在"), b"old binary");
}

// ---------------------------------------------------------------------------
// 注入 Fetcher：一个 socket 都不开
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_injected_fetcher_serves_everything_without_touching_sockets() {
    let (name, bytes) = fake_binary("burrow-next", Some("0.2.0"), 0);
    let spec = spec(&name, &bytes);

    let dir = tempfile::tempdir().expect("临时目录");
    let target = dir.path().join(installed_name());
    std::fs::write(&target, b"old binary").expect("写旧二进制");

    let mut ctx = UpdateContext::new(Version::new(0, 1, 0), dir.path().to_path_buf());
    ctx.binary_name = Some(installed_name().to_string());
    ctx.base_url = Some("https://mirror.invalid".to_string());
    ctx.require_signature = false; // 这条链路测的是「注入 fetcher」，不是验签

    let manifest = manifest_json("0.2.0", "stable", &[spec]);
    let asset_url = ctx.asset_url(&name);
    let manifest_url = ctx.manifest_url();
    let calls: Arc<Mutex<Vec<(String, usize)>>> = Arc::new(Mutex::new(Vec::new()));
    let calls_in_fetcher = Arc::clone(&calls);
    let asset_source = bytes.clone();

    let fetcher: Fetcher = Arc::new(move |url: &str, max: usize| {
        let url = url.to_string();
        let manifest = manifest.clone();
        let asset = asset_source.clone();
        let calls = Arc::clone(&calls_in_fetcher);
        Box::pin(async move {
            calls.lock().expect("calls 锁").push((url.clone(), max));
            if url.ends_with("latest.json") {
                Ok(manifest)
            } else {
                Ok(asset)
            }
        })
    });
    ctx.fetcher = Some(fetcher);

    let status: UpdateStatus = check(&ctx).await.expect("打桩的清单");
    assert!(status.available);

    let applied = apply(&ctx).await.expect("打桩的资产");
    assert_eq!(applied.to, Version::new(0, 2, 0));
    assert_eq!(std::fs::read(&target).expect("新二进制"), bytes);

    let calls = calls.lock().expect("calls 锁").clone();
    assert_eq!(calls.len(), 3, "check 一次 + apply 里的 check 与资产各一次");
    assert_eq!(calls[0].0, manifest_url);
    assert_eq!(calls[0].1, MAX_MANIFEST_BYTES as usize);
    assert_eq!(calls[2].0, asset_url);
    assert_eq!(
        calls[2].1,
        (bytes.len() as u64 + ASSET_DOWNLOAD_SLACK) as usize
    );
}

// ---------------------------------------------------------------------------
// 替换（临时目录，不碰真实安装目录）
// ---------------------------------------------------------------------------

#[test]
fn replacement_swaps_the_file_and_can_be_rolled_back() {
    let dir = tempfile::tempdir().expect("临时目录");

    // 正常替换：新内容上位，旧内容留在 <name>.old-<pid>
    let target = dir.path().join(installed_name());
    let fresh = dir.path().join("fresh");
    std::fs::write(&target, b"old binary").expect("写旧二进制");
    std::fs::write(&fresh, b"new binary").expect("写新二进制");

    let backup = replace_binary(&target, &fresh).expect("替换应当成功");
    assert_eq!(std::fs::read(&target).expect("新二进制"), b"new binary");
    assert_eq!(std::fs::read(&backup).expect("备份"), b"old binary");
    assert_eq!(backup.parent().expect("同目录"), dir.path());

    // 回滚路径：源文件不存在 → 报错，且原文件必须原封不动
    let second = dir.path().join("second");
    std::fs::write(&target, b"current binary").expect("写当前二进制");
    let error = replace_binary(&target, &second).expect_err("源文件不存在");

    assert!(matches!(error, UpdateError::Replace { .. }), "{error}");
    assert_eq!(
        std::fs::read(&target).expect("回滚后还是当前二进制"),
        b"current binary"
    );
}

#[tokio::test]
async fn apply_reports_a_missing_installed_binary() {
    let (name, bytes) = fake_binary("burrow-next", Some("0.2.0"), 0);
    let spec = spec(&name, &bytes);
    let server = TestServer::start(vec![(
        manifest_path(Channel::Stable),
        Reply::Body(manifest_json("0.2.0", "stable", &[spec])),
    )])
    .await;

    let dir = tempfile::tempdir().expect("临时目录"); // 里面**没有**二进制
    let ctx = context(&server, dir.path(), "0.1.0");
    let error = apply(&ctx).await.expect_err("安装目录里没有目标");

    assert!(
        matches!(error, UpdateError::MissingBinary { .. }),
        "{error}"
    );
    assert_eq!(server.seen(), vec![manifest_path(Channel::Stable)]);
}
