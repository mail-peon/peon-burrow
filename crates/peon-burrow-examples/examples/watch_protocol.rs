//! 只用**协议层**解析/生成 watch 报文（不依赖引擎）。
//!
//! 想用别的语言写扩展侧客户端时，这个文件就是最小的行为说明：
//! 第一帧长什么样、中继会推什么、关闭原因怎么截断到合法长度。
//!
//! ```text
//! cargo run --example watch_protocol
//! ```

use peon_burrow_protocol::{
    ClientMessage, POLICY_VIOLATION, WATCH_PROTOCOL_VERSION, WatchRequest, truncate_close_reason,
};

fn main() {
    println!("watch 协议版本：{WATCH_PROTOCOL_VERSION}");

    // 扩展发出的第一帧
    let first_frame = r#"{"__watch":1,"accountId":"acc_1","host":"imap.qq.com","port":993,"tls":true,"user":"me@qq.com","pass":"secret","token":"t"}"#;
    let request = WatchRequest::parse(first_frame).expect("应当被识别为 watch 请求");
    let credentials = request.credentials().expect("应当校验通过");
    println!(
        "host = {}:{} tls = {}",
        credentials.host, credentials.port, credentials.tls
    );
    println!("accountId = {}", credentials.account_id);

    // 中继会推的消息
    for message in [
        ClientMessage::Watching { exists: 3 },
        ClientMessage::Mail {
            account_id: "acc_1".to_owned(),
            exists: 4,
        },
        ClientMessage::Reconnecting { retry_in_ms: 5000 },
        ClientMessage::Failed {
            error: "登录失败：授权码错".to_owned(),
        },
    ] {
        println!("{}", message.to_json());
    }

    // 关闭原因必须能在任意 UTF-8 边界安全截断（WebSocket 上限 123 字节）
    let long = "很长的中文原因".repeat(30);
    let truncated = truncate_close_reason(&long);
    println!("截断后 {} 字节（上限 120）", truncated.len());
    println!("拒绝用的关闭码：{POLICY_VIOLATION}");
}
