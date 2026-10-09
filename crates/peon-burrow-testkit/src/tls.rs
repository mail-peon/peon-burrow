//! 测试用的 TLS 材料与 TLS 回声服务器。
//!
//! 用 `rcgen` **现场生成**证书，不依赖 `openssl` 命令 —— TS 版的端到端测试在没有 openssl 的
//! 机器上会**跳过** TLS 断言，而「默认拒绝自签证书」那条是**安全属性**，不能靠跳过蒙混。

use std::net::SocketAddr;
use std::sync::Arc;

use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

/// 一套测试证书：一个自签 CA + 一张由它签发的叶子证书。
#[derive(Debug)]
pub struct TestCertificates {
    /// CA 证书（DER）—— 客户端把它当信任根注入。
    pub ca_der: CertificateDer<'static>,
    /// 服务器要发的证书链（叶子 + CA）。
    pub chain: Vec<CertificateDer<'static>>,
    /// 叶子证书的私钥。
    pub key: PrivateKeyDer<'static>,
}

/// 生成一套证书；`names` 是 SAN（`localhost`、`127.0.0.1` 之类）。
pub fn certificates_for(names: &[&str]) -> TestCertificates {
    let ca_key = KeyPair::generate().expect("generate ca key");
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).expect("ca params");
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "peon-burrow test ca");
    let ca_cert = ca_params.self_signed(&ca_key).expect("self sign ca");

    let leaf_key = KeyPair::generate().expect("generate leaf key");
    let leaf_params = CertificateParams::new(
        names
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<Vec<_>>(),
    )
    .expect("leaf params");
    let issuer = Issuer::from_params(&ca_params, &ca_key);
    let leaf_cert = leaf_params
        .signed_by(&leaf_key, &issuer)
        .expect("sign leaf");

    TestCertificates {
        ca_der: ca_cert.der().clone(),
        chain: vec![leaf_cert.der().clone(), ca_cert.der().clone()],
        key: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der())),
    }
}

/// 起一个 TLS 回声服务器（用 `certificates` 里的叶子证书）。
pub async fn spawn_tls_echo(certificates: &TestCertificates) -> SocketAddr {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_no_client_auth()
        .with_single_cert(certificates.chain.clone(), certificates.key.clone_key())
        .expect("server config");
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind tls echo");
    let addr = listener.local_addr().expect("tls echo addr");
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(mut stream) = acceptor.accept(stream).await else {
                    return;
                };
                let mut buffer = [0u8; 8192];
                loop {
                    match stream.read(&mut buffer).await {
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            if stream.write_all(&buffer[..read]).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }
    });
    addr
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn certificates_can_be_generated() {
        let certificates = certificates_for(&["localhost", "127.0.0.1"]);
        assert_eq!(certificates.chain.len(), 2, "leaf + ca");
        assert!(!certificates.ca_der.as_ref().is_empty());
    }

    #[tokio::test]
    async fn the_tls_echo_server_accepts_a_connection_from_our_own_ca() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let certificates = certificates_for(&["localhost", "127.0.0.1"]);
        let addr = spawn_tls_echo(&certificates).await;

        let mut roots = rustls::RootCertStore::empty();
        roots.add(certificates.ca_der.clone()).expect("add root");
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(Arc::new(config));

        let stream = tokio::net::TcpStream::connect(addr).await.expect("tcp");
        let name = rustls::pki_types::ServerName::try_from("localhost").expect("server name");
        let mut tls = connector.connect(name, stream).await.expect("handshake");
        tls.write_all(b"hello").await.expect("write");
        let mut buffer = [0u8; 5];
        tls.read_exact(&mut buffer).await.expect("read");
        assert_eq!(&buffer, b"hello");
    }
}
