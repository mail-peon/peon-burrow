//! 脚本化的 IMAP 服务器：按剧本收命令、发响应。
//!
//! 剧本而不是 `if/else` 堆叠 —— 每个测试只声明「我要的这段对话」，
//! 出问题时日志里就是一段可以照抄回真实邮箱的往来。

use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// 剧本里的一步。
#[derive(Debug, Clone)]
pub enum Step {
    /// 等到收到一条包含该子串的行（不匹配就报错并关连接）。
    Expect(String),
    /// 发送一行（自动补 CRLF）。
    Send(String),
    /// 等一会儿（用来制造「半行」「延迟推送」这类时序）。
    Delay(Duration),
    /// 直接关连接（模拟服务器断开）。
    Close,
}

impl Step {
    /// `Expect` 的简写。
    pub fn expect(line: impl Into<String>) -> Self {
        Self::Expect(line.into())
    }

    /// `Send` 的简写。
    pub fn send(line: impl Into<String>) -> Self {
        Self::Send(line.into())
    }

    /// `Delay` 的简写（毫秒）。
    pub fn delay_ms(millis: u64) -> Self {
        Self::Delay(Duration::from_millis(millis))
    }
}

/// 起一个按剧本演的 IMAP 服务器，返回它的地址。
pub async fn spawn(script: Vec<Step>) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock imap");
    let addr = listener.local_addr().expect("mock imap addr");
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let script = script.clone();
            tokio::spawn(async move {
                let mut pending = Vec::<u8>::new();
                let mut buffer = [0u8; 4096];
                for step in script {
                    match step {
                        Step::Send(line) => {
                            let payload = format!("{line}\r\n");
                            if stream.write_all(payload.as_bytes()).await.is_err() {
                                return;
                            }
                            let _ = stream.flush().await;
                        }
                        Step::Delay(duration) => tokio::time::sleep(duration).await,
                        Step::Close => return,
                        Step::Expect(expected) => {
                            let mut matched = false;
                            while !matched {
                                while let Some(position) = find_crlf(&pending) {
                                    let line =
                                        String::from_utf8_lossy(&pending[..position]).into_owned();
                                    pending.drain(..position + 2);
                                    if line.contains(&expected) {
                                        matched = true;
                                        break;
                                    }
                                }
                                if matched {
                                    break;
                                }
                                match stream.read(&mut buffer).await {
                                    Ok(0) | Err(_) => return,
                                    Ok(read) => pending.extend_from_slice(&buffer[..read]),
                                }
                            }
                        }
                    }
                }
            });
        }
    });
    addr
}

/// 找 CRLF 的位置。
fn find_crlf(bytes: &[u8]) -> Option<usize> {
    bytes.windows(2).position(|pair| pair == b"\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_scripted_server_answers_in_order() {
        let addr = spawn(vec![
            Step::send("* OK [CAPABILITY IMAP4rev1] ready"),
            Step::expect("A0001 LOGIN"),
            Step::send("A0001 OK LOGIN completed"),
            Step::expect("A0002 SELECT"),
            Step::send("A0002 OK [READ-WRITE] SELECT completed"),
        ])
        .await;

        let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
        let mut buffer = [0u8; 256];
        let read = stream.read(&mut buffer).await.expect("read greeting");
        assert!(String::from_utf8_lossy(&buffer[..read]).contains("ready"));

        stream
            .write_all(b"A0001 LOGIN \"me\" \"pw\"\r\n")
            .await
            .expect("login");
        let read = stream.read(&mut buffer).await.expect("read login");
        assert!(String::from_utf8_lossy(&buffer[..read]).contains("A0001 OK"));

        stream
            .write_all(b"A0002 SELECT INBOX\r\n")
            .await
            .expect("select");
        let read = stream.read(&mut buffer).await.expect("read select");
        assert!(String::from_utf8_lossy(&buffer[..read]).contains("READ-WRITE"));
    }
}
