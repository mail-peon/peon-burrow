//! 连一个用私有 CA 的自签证书服务器：**注入信任根**，而不是关掉校验。
//!
//! ```text
//! cargo run --example private_ca -- /path/to/ca.der
//! ```

use std::sync::Arc;

use peon_burrow_core::{CertificateDer, PolicyRules, RelayOptions, RelayServer, TlsConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).ok_or("用法：private_ca <ca.der>")?;
    let der = std::fs::read(&path)?;
    let tls = TlsConfig::default().with_root(CertificateDer::from(der));

    let relay = RelayServer::start_with(
        RelayOptions::on("127.0.0.1", 0),
        Arc::new(PolicyRules::new(
            None,
            vec!["mail.example.internal".to_owned()],
        )),
        tls,
    )
    .await?;

    println!("地址：{}", relay.url());
    tokio::signal::ctrl_c().await?;
    relay.stop().await?;
    Ok(())
}
