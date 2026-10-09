//! 换一套访问策略：只允许连一个固定的邮件服务器。
//!
//! 这是给接入方留的口子 —— 不用改引擎，只要实现 `Policy`。
//!
//! ```text
//! cargo run --example custom_policy
//! ```

use std::sync::Arc;

use peon_burrow_core::{Policy, PolicyRules, RelayOptions, RelayServer, TlsConfig};
use peon_burrow_protocol::{RejectReason, RelayTarget};

/// 只放行一个固定目标（连 host 都不让客户端自己选）。
#[derive(Debug)]
struct OnlyThisServer {
    host: String,
    inner: PolicyRules,
}

impl Policy for OnlyThisServer {
    fn check(&self, target: &RelayTarget) -> Result<(), RejectReason> {
        if target.host != self.host {
            return Err(RejectReason::HostNotAllowed);
        }
        self.inner.check(target)
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let policy = OnlyThisServer {
        host: "imap.qq.com".to_owned(),
        inner: PolicyRules::new(
            Some("my-secret-token".to_owned()),
            vec!["imap.qq.com".to_owned()],
        ),
    };

    let relay = RelayServer::start_with(
        RelayOptions::on("127.0.0.1", 0),
        Arc::new(policy),
        TlsConfig::default(),
    )
    .await?;

    println!("地址：{}（token 就是 my-secret-token）", relay.url());
    tokio::signal::ctrl_c().await?;
    relay.stop().await?;
    Ok(())
}
