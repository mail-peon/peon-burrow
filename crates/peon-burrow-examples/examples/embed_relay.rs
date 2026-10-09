//! 把中继嵌进自己的程序：起一个只监听本机的中继，打印扩展要填的地址。
//!
//! ```text
//! cargo run --example embed_relay
//! ```

use peon_burrow_core::{RelayOptions, RelayServer};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let relay = RelayServer::start(RelayOptions::on("127.0.0.1", 0)).await?;
    println!("扩展里填：{}", relay.url());
    println!("Ctrl+C 结束");

    tokio::signal::ctrl_c().await?;
    relay.stop().await?;
    Ok(())
}
