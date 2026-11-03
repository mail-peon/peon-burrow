use super::body::find;
use super::url::Endpoint;
use super::*;

#[test]
fn parses_the_urls_we_actually_build() {
    assert_eq!(
        parse_url("https://github.com/mail-peon/peon-burrow/releases/latest/download/latest.json")
            .unwrap(),
        Endpoint {
            https: true,
            host: "github.com".to_string(),
            port: 443,
            path: "/mail-peon/peon-burrow/releases/latest/download/latest.json".to_string(),
        }
    );
    assert_eq!(
        parse_url("http://127.0.0.1:8080/a/b").unwrap(),
        Endpoint {
            https: false,
            host: "127.0.0.1".to_string(),
            port: 8080,
            path: "/a/b".to_string(),
        }
    );
    // 没有路径 → "/"；镜像的嵌套前缀原样保留
    assert_eq!(parse_url("https://mirror.invalid").unwrap().path, "/");
    assert_eq!(
        parse_url("https://gh-proxy.com/https://github.com/x")
            .unwrap()
            .path,
        "/https://github.com/x"
    );
    // Host 头只在非默认端口时带端口
    assert_eq!(parse_url("https://a.b/c").unwrap().host_header(), "a.b");
    assert_eq!(parse_url("http://a.b:80/c").unwrap().host_header(), "a.b");
    assert_eq!(
        parse_url("https://a.b:8443/c").unwrap().host_header(),
        "a.b:8443"
    );
}

#[test]
fn rejects_urls_we_cannot_handle() {
    assert!(matches!(
        parse_url("ftp://a.b/c").unwrap_err(),
        UpdateError::BadUrl { .. }
    ));
    assert!(matches!(
        parse_url("github.com/x").unwrap_err(),
        UpdateError::BadUrl { .. }
    ));
    assert!(matches!(
        parse_url("https:///x").unwrap_err(),
        UpdateError::BadUrl { .. }
    ));
    assert!(matches!(
        parse_url("https://a.b:notaport/x").unwrap_err(),
        UpdateError::BadUrl { .. }
    ));
}

#[test]
fn resolves_the_redirects_github_sends() {
    assert_eq!(
        resolve_location(
            "https://github.com/x/latest/download/latest.json",
            "https://objects.githubusercontent.com/abc"
        )
        .unwrap(),
        "https://objects.githubusercontent.com/abc"
    );
    assert_eq!(
        resolve_location(
            "https://gh-proxy.com/https://github.com/x/latest/download/latest.json",
            "/real/path"
        )
        .unwrap(),
        "https://gh-proxy.com/real/path"
    );
    assert!(resolve_location("https://a.b/c", "relative/path").is_err());
}

#[tokio::test]
async fn proxy_is_rejected_loudly_instead_of_being_ignored() {
    let error = get("https://a.b/c", 1024, Some("http://p:8080"))
        .await
        .expect_err("内置实现这一版不支持代理");
    assert!(
        matches!(error, UpdateError::ProxyUnsupported { .. }),
        "{error}"
    );
    assert!(error.to_string().contains("代理"), "{error}");
}

#[test]
fn find_works_on_the_delimiters_we_use() {
    assert_eq!(find(b"abc\r\n\r\ndef", b"\r\n\r\n"), Some(3));
    assert_eq!(find(b"abc", b"\r\n\r\n"), None);
    assert_eq!(find(b"", b"\r\n"), None);
}
