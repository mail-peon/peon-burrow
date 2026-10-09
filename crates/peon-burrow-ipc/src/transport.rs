//! 传输实现：本地 socket（命名管道 / Unix domain socket）与 loopback TCP。
//!
//! 两个平台分支都藏在 [`IoStream`] 后面 —— 上层（客户端 / 服务端）只看这个 trait，
//! 所以换实现不需要动逻辑。

use std::io;

use tokio::io::{AsyncRead, AsyncWrite};

use crate::endpoint::{ControlEndpoint, TransportKind};

/// 双向字节流。
pub trait IoStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T> IoStream for T where T: AsyncRead + AsyncWrite + Unpin + Send {}

/// 监听端。
pub enum Listener {
    /// loopback TCP。
    Tcp(tokio::net::TcpListener),
    /// Unix domain socket。
    #[cfg(unix)]
    Unix(tokio::net::UnixListener, std::path::PathBuf),
    /// Windows 命名管道。
    ///
    /// ⚠️ 必须**始终保留一个预备实例**：命名管道的客户端是「先连、服务端再 accept」，
    /// 没有实例时 `open()` 直接报 `os error 2`（文件不存在）—— 现用现建就是这个失败。
    #[cfg(windows)]
    Pipe {
        /// 管道名（`\\.\pipe\…`）。
        name: String,
        /// 预备实例：保证客户端 `open()` 时一定有东西可连。
        pending: tokio::sync::Mutex<Option<tokio::net::windows::named_pipe::NamedPipeServer>>,
    },
}

impl std::fmt::Debug for Listener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tcp(_) => f.write_str("Listener::Tcp"),
            #[cfg(unix)]
            Self::Unix(_, path) => f.debug_tuple("Listener::Unix").field(path).finish(),
            #[cfg(windows)]
            Self::Pipe { name, .. } => f
                .debug_struct("Listener::Pipe")
                .field("name", name)
                .finish(),
        }
    }
}

impl Listener {
    /// 按地址监听。
    ///
    /// TCP 的端口可以给 `0`（内核分配），之后用 [`Listener::address`] 问实际地址。
    pub async fn bind(endpoint: &ControlEndpoint) -> io::Result<Self> {
        match endpoint.kind {
            TransportKind::LoopbackTcp => {
                let listener = tokio::net::TcpListener::bind(&endpoint.address).await?;
                Ok(Self::Tcp(listener))
            }
            TransportKind::LocalSocket => {
                #[cfg(unix)]
                {
                    let path = std::path::PathBuf::from(&endpoint.address);
                    // 上一次没退干净会留下 socket 文件，bind 会失败；先清掉
                    let _ = std::fs::remove_file(&path);
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    let listener = tokio::net::UnixListener::bind(&path)?;
                    Ok(Self::Unix(listener, path))
                }
                #[cfg(windows)]
                {
                    use tokio::net::windows::named_pipe::ServerOptions;
                    let name = pipe_name(&endpoint.address);
                    let first = ServerOptions::new().create(&name)?;
                    Ok(Self::Pipe {
                        name,
                        pending: tokio::sync::Mutex::new(Some(first)),
                    })
                }
                #[cfg(not(any(unix, windows)))]
                {
                    Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "当前平台没有本地 socket 实现",
                    ))
                }
            }
        }
    }

    /// 实际地址（TCP 用了内核分配端口时尤其重要）。
    pub fn address(&self) -> io::Result<String> {
        match self {
            Self::Tcp(listener) => Ok(listener.local_addr()?.to_string()),
            #[cfg(unix)]
            Self::Unix(_, path) => Ok(path.to_string_lossy().into_owned()),
            #[cfg(windows)]
            Self::Pipe { name, .. } => Ok(name.clone()),
        }
    }

    /// 接受一条连接。
    pub async fn accept(&self) -> io::Result<Box<dyn IoStream>> {
        match self {
            Self::Tcp(listener) => {
                let (stream, _) = listener.accept().await?;
                Ok(Box::new(stream))
            }
            #[cfg(unix)]
            Self::Unix(listener, _) => {
                let (stream, _) = listener.accept().await?;
                Ok(Box::new(stream))
            }
            #[cfg(windows)]
            Self::Pipe { name, pending } => {
                use tokio::net::windows::named_pipe::ServerOptions;
                let name = name.as_str();
                let server = {
                    let mut guard = pending.lock().await;
                    let server = match guard.take() {
                        Some(server) => server,
                        None => ServerOptions::new().create(name)?,
                    };
                    // 立刻为下一条连接补一个实例，再等当前这条连上来
                    *guard = Some(ServerOptions::new().create(name)?);
                    server
                };
                server.connect().await?;
                Ok(Box::new(server))
            }
        }
    }
}

/// 连上控制面。
pub async fn connect(endpoint: &ControlEndpoint) -> io::Result<Box<dyn IoStream>> {
    match endpoint.kind {
        TransportKind::LoopbackTcp => {
            let stream = tokio::net::TcpStream::connect(&endpoint.address).await?;
            stream.set_nodelay(true).ok();
            Ok(Box::new(stream))
        }
        TransportKind::LocalSocket => {
            #[cfg(unix)]
            {
                let stream = tokio::net::UnixStream::connect(&endpoint.address).await?;
                Ok(Box::new(stream))
            }
            #[cfg(windows)]
            {
                use tokio::net::windows::named_pipe::ClientOptions;
                let stream = ClientOptions::new().open(pipe_name(&endpoint.address))?;
                Ok(Box::new(stream))
            }
            #[cfg(not(any(unix, windows)))]
            {
                Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "当前平台没有本地 socket 实现",
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn loopback_tcp_listens_on_an_assigned_port() {
        let mut endpoint = ControlEndpoint::loopback_tcp(0, "t");
        let listener = Listener::bind(&endpoint).await.expect("bind");
        let address = listener.address().expect("address");
        assert!(
            !address.ends_with(":0"),
            "应当拿到内核分配的端口：{address}"
        );
        endpoint.address = address;
        assert_eq!(endpoint.kind, TransportKind::LoopbackTcp);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn a_named_pipe_round_trips_bytes() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let name = format!("peon-burrow-test-{}", std::process::id());
        let endpoint = ControlEndpoint::local_socket(&name, "t");
        let listener = Listener::bind(&endpoint).await.expect("bind pipe");

        let accept = tokio::spawn(async move {
            let mut stream = listener.accept().await.expect("accept");
            let mut buffer = [0u8; 16];
            let read = stream.read(&mut buffer).await.expect("read");
            stream.write_all(&buffer[..read]).await.expect("write");
        });

        let mut client = connect(&endpoint).await.expect("connect");
        client.write_all(b"ping").await.expect("write");
        let mut buffer = [0u8; 4];
        client.read_exact(&mut buffer).await.expect("read");
        assert_eq!(&buffer, b"ping");
        accept.await.expect("join");
    }
}

/// 把短名补成完整的命名管道路径。
///
/// ⚠️ Windows 的 `CreateNamedPipe` / `CreateFile` 只认 `\\.\pipe\<名字>` 全名；
/// 传 `"peon-burrow"` 会得到 `os error 123`（文件名语法不正确）—— 一个和「连不上」
/// 长得完全不像的错误。所以在这里统一补齐，调用方可以只写短名。
#[cfg(windows)]
fn pipe_name(address: &str) -> String {
    const PREFIX: &str = r"\\.\pipe\";
    if address.starts_with(PREFIX) {
        address.to_owned()
    } else {
        format!("{PREFIX}{address}")
    }
}

#[cfg(all(test, windows))]
mod pipe_name_tests {
    use super::pipe_name;

    #[test]
    fn short_names_get_the_pipe_prefix() {
        assert_eq!(pipe_name("peon-burrow"), r"\\.\pipe\peon-burrow");
        assert_eq!(pipe_name(r"\\.\pipe\x"), r"\\.\pipe\x");
    }
}
