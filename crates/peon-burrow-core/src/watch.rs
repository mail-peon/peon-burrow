//! 常驻监听（IMAP `IDLE`）：替扩展挂住连接，有新邮件就推一行 JSON。
//!
//! 为什么必须由中继做：MV3 的 Service Worker 空闲约 30 秒就被回收，插件侧挂不住长连接。
//!
//! 状态机与 TS 版逐条对齐（`ai-docs/04-parity-node-to-rust.md § 3 E`）：
//!
//! ```text
//! greeting ──(未标记的 * OK / * PREAUTH)──▶ login ──(A000n OK)──▶ select ──(A000n OK)──▶ idle
//!                                                                                        │  ▲
//!                                                          25 分钟 ──DONE──▶ done ────────┘
//!                                                                    (A000n OK)
//! ```
//!
//! ⚠️ 问候语是**未标记**的（`* OK [CAPABILITY …] ready`），所以「等问候」不能靠等某个 tag ——
//! 要等的是**第一条以 `* ` 开头的行**。写成等 tag 会让状态机永远停在 greeting，
//! 而客户端那边只是「一条消息都收不到」（连接是好的、不报错）。

use std::time::Duration;

use peon_burrow_protocol::{ClientMessage, RelayTarget, WatchCredentials};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::error::{FatalReason, WatchError};
use crate::line_buffer::LineBuffer;
use crate::options::RelayOptions;
use crate::state::{CancelToken, SharedMetrics};
use crate::transport::{self, TlsConfig};

/// 状态机的位置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Greeting,
    Login,
    Select,
    Idle,
    Done,
}

/// 读缓冲。
const READ_BUFFER: usize = 8 * 1024;

/// 跑到底：连上、挂 IDLE、推消息；断了按阶梯重连，致命错误则停下。
pub(crate) async fn run_forever(
    credentials: WatchCredentials,
    tls: TlsConfig,
    messages: mpsc::Sender<ClientMessage>,
    metrics: SharedMetrics,
    mut cancel: CancelToken,
    options: &RelayOptions,
) {
    let account_id = credentials.account_id.clone();
    let mut attempt = 0usize;

    loop {
        if cancel.is_cancelled() {
            break;
        }

        match watch_once(&credentials, &tls, &messages, &mut cancel, options).await {
            // 服务器正常关了连接：重连，退避归零
            Ok(()) => attempt = 0,
            Err(error) if error.is_fatal() => {
                // 重试无用：交给插件去提示用户改配置（快速重连只会让服务器锁账号）
                warn!(account = %account_id, reason = %error.message(), "watch stopped");
                metrics.record_error(error.message());
                let _ = messages
                    .send(ClientMessage::Failed {
                        error: error.message(),
                    })
                    .await;
                break;
            }
            Err(error) => {
                debug!(account = %account_id, reason = %error.message(), "watch attempt failed");
                let _ = messages
                    .send(ClientMessage::Error {
                        error: error.message(),
                    })
                    .await;
            }
        }

        if cancel.is_cancelled() {
            break;
        }

        let delay = options
            .watch_retry_delays
            .get(attempt.min(options.watch_retry_delays.len().saturating_sub(1)))
            .copied()
            .unwrap_or(Duration::from_secs(300));
        attempt = attempt.saturating_add(1);

        let _ = messages
            .send(ClientMessage::Reconnecting {
                retry_in_ms: delay.as_millis() as u64,
            })
            .await;

        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(delay) => {}
        }
    }

    info!(account = %account_id, "watch finished");
}

/// 挂一次 `IDLE`，直到连接断开。
///
/// `Ok(())` = 服务器正常关了连接（重连）；`Err` = 出错，调用方据此决定「还重不重试」。
async fn watch_once(
    credentials: &WatchCredentials,
    tls: &TlsConfig,
    messages: &mpsc::Sender<ClientMessage>,
    cancel: &mut CancelToken,
    options: &RelayOptions,
) -> Result<(), WatchError> {
    let target = RelayTarget {
        host: credentials.host.clone(),
        port: credentials.port,
        tls: credentials.tls,
        token: credentials.token.clone(),
    };

    let mut socket = transport::connect(&target, tls, options.connect_timeout)
        .await
        .map_err(|error| WatchError::Transport(error.user_message()))?;

    info!(host = %credentials.host, port = credentials.port, account = %credentials.account_id, "watch connected");

    let mut lines = LineBuffer::new();
    let mut buffer = vec![0u8; READ_BUFFER];
    let mut phase = Phase::Greeting;
    let mut tag = 0u32;
    // 服务器报告的邮件总数 / 已经推给插件的那个数（避免同一封推两次）
    let mut exists: i64 = -1;
    let mut notified: i64 = -1;
    // 下一次重挂 IDLE 的时刻
    let mut reidle_at: Option<tokio::time::Instant> = None;

    loop {
        if let Some(line) = lines.next_line() {
            if options.trace {
                info!(inbound = %line, "watch trace");
            }

            // --- 服务器主动推的未标记响应：* n EXISTS -------------------------
            if let Some(count) = line.strip_prefix("* ").and_then(parse_exists) {
                exists = count;
                // 只在**变大**时推：变小说明有人在别处删邮件，不是新邮件
                if notified >= 0 && exists > notified {
                    info!(host = %credentials.host, from = notified, to = exists, "new mail");
                    let _ = messages
                        .send(ClientMessage::Mail {
                            account_id: credentials.account_id.clone(),
                            exists: exists as u32,
                        })
                        .await;
                }
                notified = exists;
                continue;
            }

            // `+ idling`：已经进入 IDLE，除了等和到点重挂，什么都不做
            if line.starts_with("+ ") {
                continue;
            }

            match phase {
                Phase::Greeting => {
                    // 问候语没有 tag：靠「第一条 * 开头的行」判断它到了
                    if !line.starts_with("* ") {
                        continue;
                    }
                    if is_greeting_ok(&line) {
                        tag += 1;
                        let command = format!(
                            "A{tag:04} LOGIN {} {}",
                            quote(&credentials.user)?,
                            quote(&credentials.pass)?
                        );
                        if options.trace {
                            info!("watch trace: -> {command}");
                        }
                        write_line(&mut socket, &command).await?;
                        phase = Phase::Login;
                    } else {
                        // `* BYE` 之类：服务器拒绝服务
                        return Err(WatchError::Fatal(FatalReason::GreetingFailed(line)));
                    }
                }
                Phase::Login | Phase::Select | Phase::Idle | Phase::Done => {
                    let Some((status, rest)) = parse_tagged(&line, tag) else {
                        // 其它未标记响应（FLAGS / PERMANENTFLAGS / CAPABILITY …）与判断无关
                        continue;
                    };
                    let ok = status == "OK";

                    match phase {
                        Phase::Login => {
                            if !ok {
                                // 认证失败**不重试**：快速重连只会让服务器把账号锁掉
                                return Err(WatchError::Fatal(FatalReason::LoginFailed(rest)));
                            }
                            tag += 1;
                            write_line(&mut socket, &format!("A{tag:04} SELECT INBOX")).await?;
                            phase = Phase::Select;
                        }
                        Phase::Select => {
                            if !ok {
                                return Err(WatchError::Fatal(FatalReason::SelectFailed(rest)));
                            }
                            // 基准值 = SELECT 那一刻的 EXISTS：这样「插件连上之前刚到的那封」
                            // 不会被误推
                            notified = if exists >= 0 { exists } else { 0 };
                            phase = Phase::Idle;
                            info!(host = %credentials.host, exists = notified, "watching");
                            let _ = messages
                                .send(ClientMessage::Watching {
                                    exists: notified as u32,
                                })
                                .await;
                            tag += 1;
                            write_line(&mut socket, &format!("A{tag:04} IDLE")).await?;
                            reidle_at = Some(tokio::time::Instant::now() + options.watch_reidle);
                        }
                        Phase::Idle | Phase::Done => {
                            if phase == Phase::Done {
                                if !ok {
                                    // DONE 被拒：当作可重试（TS 版同样不归为致命）
                                    return Err(WatchError::Transport(format!(
                                        "DONE 被拒：{}",
                                        if rest.is_empty() {
                                            "服务器拒绝"
                                        } else {
                                            &rest
                                        }
                                    )));
                                }
                                phase = Phase::Idle;
                                tag += 1;
                                write_line(&mut socket, &format!("A{tag:04} IDLE")).await?;
                                reidle_at =
                                    Some(tokio::time::Instant::now() + options.watch_reidle);
                            }
                        }
                        Phase::Greeting => unreachable!("greeting 不是带标记的响应"),
                    }
                }
            }
            continue;
        }

        // 缓冲里没有完整行：等数据、等 reidle 到点、或者被取消
        let timer = async {
            match reidle_at {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending::<()>().await,
            }
        };

        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            read = socket.read(&mut buffer) => {
                let read = read.map_err(|error| WatchError::Transport(error.to_string()))?;
                if read == 0 {
                    // 服务器正常关了连接
                    return Ok(());
                }
                lines.push(&buffer[..read]);
            }
            _ = timer => {
                // RFC 2177 建议客户端的 IDLE 不超过 29 分钟：到点重挂一次
                debug!(host = %credentials.host, "re-idling");
                write_line(&mut socket, "DONE").await?;
                phase = Phase::Done;
                reidle_at = None;
            }
        }
    }
}

/// `* 12 EXISTS` 的 `12`（`EXISTS` 之后允许有别的内容）。
fn parse_exists(rest: &str) -> Option<i64> {
    let (count, tail) = rest.split_once(' ')?;
    let count = count.parse::<i64>().ok()?;
    tail.starts_with("EXISTS").then_some(count)
}

/// 问候语是不是「可以继续」的那种（`* OK` / `* PREAUTH`）。
fn is_greeting_ok(line: &str) -> bool {
    let upper = line.to_ascii_uppercase();
    upper.starts_with("* OK") || upper.starts_with("* PREAUTH")
}

/// 解析带标记的响应：`A0001 OK ...`。
fn parse_tagged(line: &str, tag: u32) -> Option<(String, String)> {
    let prefix = format!("A{tag:04} ");
    let rest = line.strip_prefix(&prefix)?;
    let (status, detail) = rest.split_once(' ').unwrap_or((rest, ""));
    matches!(status, "OK" | "NO" | "BAD").then(|| (status.to_owned(), detail.trim().to_owned()))
}

/// IMAP quoted string 转义。
///
/// ⚠️ 两件事都要做对：转义 `\` 与 `"`；**拒绝 CR/LF** —— 凭据里出现换行的话，
/// 直接拼进命令行就变成命令注入。
fn quote(value: &str) -> Result<String, WatchError> {
    if value.contains(['\r', '\n']) {
        return Err(WatchError::Fatal(FatalReason::InvalidRequest(
            "凭据里不能包含换行符".to_owned(),
        )));
    }
    Ok(format!(
        "\"{}\"",
        value.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

/// 发一条命令（自带 CRLF）。
async fn write_line<S>(socket: &mut S, line: &str) -> Result<(), WatchError>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    socket
        .write_all(format!("{line}\r\n").as_bytes())
        .await
        .map_err(|error| WatchError::Transport(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exists_lines_are_parsed() {
        assert_eq!(parse_exists("12 EXISTS"), Some(12));
        assert_eq!(parse_exists("0 EXISTS"), Some(0));
        assert_eq!(parse_exists("4 EXISTS something"), Some(4));
        assert_eq!(parse_exists("12 RECENT"), None);
        assert_eq!(parse_exists("nope"), None);
    }

    #[test]
    fn greetings_are_recognised() {
        assert!(is_greeting_ok("* OK [CAPABILITY IMAP4rev1] ready"));
        assert!(is_greeting_ok("* PREAUTH ready"));
        assert!(is_greeting_ok("* ok lowercase"));
        assert!(!is_greeting_ok("* BYE going away"));
        assert!(!is_greeting_ok("A0001 OK done"));
    }

    #[test]
    fn tagged_responses_are_matched_by_tag() {
        assert_eq!(
            parse_tagged("A0001 OK LOGIN completed", 1),
            Some(("OK".to_owned(), "LOGIN completed".to_owned()))
        );
        assert_eq!(
            parse_tagged("A0002 NO [AUTHENTICATIONFAILED] bad", 2).map(|(s, _)| s),
            Some("NO".to_owned())
        );
        assert_eq!(parse_tagged("A0002 OK done", 3), None, "别的 tag 不算");
        assert_eq!(
            parse_tagged("A0001 OK", 1),
            Some(("OK".to_owned(), String::new()))
        );
        assert_eq!(parse_tagged("* 12 EXISTS", 1), None);
    }

    #[test]
    fn quoting_escapes_and_refuses_newlines() {
        assert_eq!(quote("plain").unwrap(), "\"plain\"");
        assert_eq!(quote("p@ss\"word").unwrap(), "\"p@ss\\\"word\"");
        assert_eq!(quote("back\\slash").unwrap(), "\"back\\\\slash\"");
        assert!(quote("bad\r\ninjection").is_err());
        assert!(quote("bad\nonly").is_err());
    }
}
