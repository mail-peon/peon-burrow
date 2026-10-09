//! 控制面的「地址」：通道类型 + 名字 + token。
//!
//! 落盘时就写成一个 JSON 文件（`control.json`），权限收到**只有当前用户可读**。
//! ⚠️ 发现文件不是权威：权威顺序是「控制面 → 服务管理器 → 发现文件」。

use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// 用哪种通道。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TransportKind {
    /// 本地 socket：Windows 命名管道 / Unix domain socket。**默认**。
    LocalSocket,
    /// loopback TCP（系统服务模式下跨完整性级别的退路）。
    LoopbackTcp,
}

/// 怎么连上控制面。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlEndpoint {
    /// 通道类型。
    pub kind: TransportKind,
    /// 通道地址：管道名 / socket 路径 / `127.0.0.1:port`。
    pub address: String,
    /// 鉴权 token。
    pub token: String,
    /// 写这个文件的进程 pid（**可选**：老版本写的文件里没有它）。
    ///
    /// 桌面端靠它区分「服务没在跑」与「发现文件是旧的」：前者是状态，
    /// 后者要在诊断里说明 —— 否则用户看到的是「明明没在跑却有个文件」。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

impl ControlEndpoint {
    /// 本地 socket 形式。
    pub fn local_socket(address: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            kind: TransportKind::LocalSocket,
            address: address.into(),
            token: token.into(),
            pid: None,
        }
    }

    /// loopback TCP 形式。
    pub fn loopback_tcp(port: u16, token: impl Into<String>) -> Self {
        Self {
            kind: TransportKind::LoopbackTcp,
            address: format!("127.0.0.1:{port}"),
            token: token.into(),
            pid: None,
        }
    }

    /// 记下写这个文件的进程（调用的地方只有 `burrow run`）。
    pub fn with_pid(mut self, pid: u32) -> Self {
        self.pid = Some(pid);
        self
    }
}

/// 把地址写进文件（原子替换 + 收紧权限）。
pub fn write_endpoint(path: &Path, endpoint: &ControlEndpoint) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let payload = serde_json::to_vec_pretty(endpoint)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;

    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, payload)?;
    restrict_permissions(&temporary)?;
    std::fs::rename(&temporary, path)?;
    Ok(())
}

/// 读回地址。
pub fn read_endpoint(path: &Path) -> io::Result<ControlEndpoint> {
    let payload = std::fs::read(path)?;
    serde_json::from_slice(&payload)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// Unix 上把权限收到 0600。
#[cfg(unix)]
fn restrict_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

/// Windows 上文件继承父目录 ACL（`%LOCALAPPDATA%` 本身就是当前用户私有）。
#[cfg(windows)]
fn restrict_permissions(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_json_shape_is_stable() {
        let endpoint = ControlEndpoint::loopback_tcp(41317, "s3cret");
        let encoded = serde_json::to_string(&endpoint).expect("encode");
        // 没有 pid 时形状不变（老版本写的文件、老客户端都还能读）
        assert_eq!(
            encoded,
            r#"{"kind":"loopback-tcp","address":"127.0.0.1:41317","token":"s3cret"}"#
        );

        // 记了 pid 就多一个字段（桌面端靠它判断发现文件是不是陈旧）
        let encoded = serde_json::to_string(&endpoint.with_pid(4321)).expect("encode");
        assert!(encoded.contains(r#""pid":4321"#), "{encoded}");
    }

    #[test]
    fn endpoints_round_trip_through_a_file() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("control.json");
        let endpoint = ControlEndpoint::local_socket("peon-burrow", "token");
        write_endpoint(&path, &endpoint).expect("write");
        assert_eq!(read_endpoint(&path).expect("read"), endpoint);
        assert!(
            !path.with_extension("tmp").exists(),
            "临时文件应当已被 rename 掉"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_endpoint_file_is_private_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("control.json");
        write_endpoint(&path, &ControlEndpoint::local_socket("x", "y")).expect("write");
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
