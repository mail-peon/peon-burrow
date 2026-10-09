//! watch 模式（IMAP IDLE 推送）的报文。
//!
//! ⚠️ 这是**协议**：字段名与 `state` 的取值同时被扩展侧的 watch 客户端消费，
//! 改任何一个都是破坏性变更（见 `ai-docs/design/wire-protocol.md § 4`）。

use serde_json::{Map, Value};

use crate::policy::RejectReason;

/// 扩展 ↔ 中继 的协议版本。
///
/// ⚠️ 与控制面的 `IPC_PROTOCOL_VERSION`（GUI ↔ 服务）**分开命名** —— 混用是明确的隐患。
pub const WATCH_PROTOCOL_VERSION: u16 = 1;

/// 客户端发来的 watch 请求（连接的第一帧 JSON）。
///
/// 字段刻意保持「未解析」状态：这段 JSON **完全来自网络**，取值要走 [`WatchRequest::text`]
/// 那种宽容的强制转换，而不是指望它长得规整。
#[derive(Debug, Clone)]
pub struct WatchRequest {
    fields: Map<String, Value>,
}

impl WatchRequest {
    /// 解析第一帧；只有 `__watch === 1` 的 JSON 对象才算 watch 请求。
    ///
    /// 判据**只有这一条**：别的字段一概不看，好让「字段暂时缺失」的连接走到能给出
    /// 具体报错的那一层，而不是在分流处被当成透传帧。
    pub fn parse(text: &str) -> Option<Self> {
        let value: Value = serde_json::from_str(text).ok()?;
        let fields = value.as_object()?.clone();
        match fields.get("__watch") {
            Some(Value::Number(number))
                if number.as_i64() == Some(WATCH_PROTOCOL_VERSION as i64) =>
            {
                Some(Self { fields })
            }
            _ => None,
        }
    }

    /// 取一个字符串字段（缺省为空串），强制转换规则见本文件里的 `coerce`。
    pub fn text(&self, key: &str) -> String {
        coerce(self.fields.get(key))
    }

    /// 邮件服务器主机名。
    pub fn host(&self) -> String {
        self.text("host")
    }

    /// 用户名。
    pub fn user(&self) -> String {
        self.text("user")
    }

    /// 密码 / 授权码。
    pub fn pass(&self) -> String {
        self.text("pass")
    }

    /// 账号 id；缺省为 `"unknown"`（原样回显在推送里，扩展据此定位账号）。
    pub fn account_id(&self) -> String {
        let value = self.text("accountId");
        if value.is_empty() {
            "unknown".to_owned()
        } else {
            value
        }
    }

    /// 端口：必须是 1..=65535 的整数（数字，或纯数字字符串）。
    pub fn port(&self) -> Option<u16> {
        match self.fields.get("port") {
            Some(Value::Number(number)) => number
                .as_u64()
                .and_then(|port| u16::try_from(port).ok())
                .filter(|port| *port > 0),
            Some(Value::String(text)) => text.parse::<u16>().ok().filter(|port| *port > 0),
            _ => None,
        }
    }

    /// 是否使用 TLS。
    ///
    /// 与 TS 版一致：`false` 与数字 `0` 算明文，**其余（含缺省、含字符串 `"0"`）都是 TLS**。
    pub fn use_tls(&self) -> bool {
        match self.fields.get("tls") {
            Some(Value::Bool(false)) => false,
            Some(Value::Number(number)) if number.as_i64() == Some(0) => false,
            _ => true,
        }
    }

    /// 请求里带的 token（空串记作 `None`），交给策略层比对。
    pub fn token(&self) -> Option<String> {
        let value = self.text("token");
        if value.is_empty() { None } else { Some(value) }
    }

    /// 校验并归一成可信的连接参数。
    pub fn credentials(&self) -> Result<WatchCredentials, RejectReason> {
        let host = self.host();
        let port = self.port();
        if host.is_empty() || port.is_none() {
            return Err(RejectReason::InvalidTarget);
        }
        let user = self.user();
        let pass = self.pass();
        if user.is_empty() || pass.is_empty() {
            return Err(RejectReason::MissingCredentials);
        }
        Ok(WatchCredentials {
            host,
            port: port.unwrap_or_default(),
            tls: self.use_tls(),
            user,
            pass,
            account_id: self.account_id(),
            token: self.token(),
        })
    }
}

/// 归一之后的 watch 连接参数（**可信**：已经过校验与强制转换）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchCredentials {
    /// 邮件服务器主机名。
    pub host: String,
    /// 端口。
    pub port: u16,
    /// 是否用 TLS。
    pub tls: bool,
    /// 用户名。
    pub user: String,
    /// 密码 / 授权码。
    pub pass: String,
    /// 账号 id（缺省 `"unknown"`）。
    pub account_id: String,
    /// 请求里带的 token。
    pub token: Option<String>,
}

/// 中继推给客户端的消息（WebSocket **文本帧**）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientMessage {
    /// 有新邮件（只在 `EXISTS` **变大**时发；推送里不含邮件内容）。
    Mail {
        /// 账号 id。
        account_id: String,
        /// 邮件总数。
        exists: u32,
    },
    /// 已挂上 IDLE；`exists` 是 `SELECT` 那一刻的基准值。
    Watching {
        /// 基准邮件数。
        exists: u32,
    },
    /// 连接断了，正在退避重连。
    Reconnecting {
        /// 下次重连前等待的毫秒数。
        retry_in_ms: u64,
    },
    /// 本轮出错但还会重试。
    Error {
        /// 错误文案。
        error: String,
    },
    /// 重试无用（凭据 / 配置问题）—— 中继已停止重连，扩展也不该重连。
    Failed {
        /// 错误文案。
        error: String,
    },
}

impl ClientMessage {
    /// 序列化成一行 JSON。
    ///
    /// 手写格式而不是用 `json!` 宏：`serde_json::Map` 默认按**字典序**排键，
    /// 那样输出会是 `{"accountId":…,"exists":…,"type":"mail"}` —— 语义相同，
    /// 但和设计文档里写死的形状不一致，排查时对照起来很别扭。
    pub fn to_json(&self) -> String {
        match self {
            Self::Mail { account_id, exists } => format!(
                r#"{{"type":"mail","accountId":{},"exists":{exists}}}"#,
                escaped(account_id)
            ),
            Self::Watching { exists } => {
                format!(r#"{{"type":"state","state":"watching","exists":{exists}}}"#)
            }
            Self::Reconnecting { retry_in_ms } => {
                format!(r#"{{"type":"state","state":"reconnecting","retryInMs":{retry_in_ms}}}"#)
            }
            Self::Error { error } => format!(
                r#"{{"type":"state","state":"error","error":{}}}"#,
                escaped(error)
            ),
            Self::Failed { error } => format!(
                r#"{{"type":"state","state":"failed","error":{}}}"#,
                escaped(error)
            ),
        }
    }
}

/// JS `String(value ?? '')` 的等价物。
///
/// 对象与数组返回空串 —— JS 会给出 `"[object Object]"`，那个字符串对我们没有任何用处。
fn coerce(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Number(number)) => number.to_string(),
        Some(Value::Bool(flag)) => flag.to_string(),
        Some(_) => String::new(),
    }
}

/// 把一个字符串安全地嵌进 JSON。
fn escaped(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_watch_one_is_a_watch_request() {
        assert!(WatchRequest::parse(r#"{"__watch":1}"#).is_some());
        assert!(WatchRequest::parse(r#"{"__watch":2}"#).is_none());
        assert!(WatchRequest::parse(r#"{"__watch":"1"}"#).is_none());
        assert!(WatchRequest::parse(r#"{"__watch":true}"#).is_none());
        assert!(WatchRequest::parse("not json").is_none());
        assert!(WatchRequest::parse("[1,2,3]").is_none());
        assert!(WatchRequest::parse("42").is_none());
        assert!(WatchRequest::parse(r#"{"host":"imap.qq.com"}"#).is_none());
    }

    #[test]
    fn the_full_request_from_the_extension_parses() {
        let text = r#"{"__watch":1,"accountId":"acc_1","host":"imap.qq.com","port":993,"tls":true,"user":"me@qq.com","pass":"secret","token":"t"}"#;
        let request = WatchRequest::parse(text).expect("should parse");
        let credentials = request.credentials().expect("should validate");
        assert_eq!(
            credentials,
            WatchCredentials {
                host: "imap.qq.com".to_owned(),
                port: 993,
                tls: true,
                user: "me@qq.com".to_owned(),
                pass: "secret".to_owned(),
                account_id: "acc_1".to_owned(),
                token: Some("t".to_owned()),
            }
        );
    }

    #[test]
    fn tls_only_accepts_false_and_zero_as_plaintext() {
        let plain = |text: &str| WatchRequest::parse(text).unwrap().use_tls();
        assert!(plain(r#"{"__watch":1}"#));
        assert!(plain(r#"{"__watch":1,"tls":true}"#));
        assert!(plain(r#"{"__watch":1,"tls":1}"#));
        assert!(plain(r#"{"__watch":1,"tls":"0"}"#)); // 字符串 "0" 仍是 TLS（与 TS 一致）
        assert!(!plain(r#"{"__watch":1,"tls":false}"#));
        assert!(!plain(r#"{"__watch":1,"tls":0}"#));
    }

    #[test]
    fn port_accepts_numbers_and_digit_strings_only() {
        let port = |text: &str| WatchRequest::parse(text).unwrap().port();
        assert_eq!(port(r#"{"__watch":1,"port":993}"#), Some(993));
        assert_eq!(port(r#"{"__watch":1,"port":"143"}"#), Some(143));
        assert_eq!(port(r#"{"__watch":1,"port":0}"#), None);
        assert_eq!(port(r#"{"__watch":1,"port":99999}"#), None);
        assert_eq!(port(r#"{"__watch":1,"port":"abc"}"#), None);
        assert_eq!(port(r#"{"__watch":1,"port":true}"#), None);
        assert_eq!(port(r#"{"__watch":1}"#), None);
    }

    #[test]
    fn coercion_matches_javascript_semantics() {
        let request =
            WatchRequest::parse(r#"{"__watch":1,"host":123,"user":true,"pass":null}"#).unwrap();
        assert_eq!(request.host(), "123");
        assert_eq!(request.user(), "true");
        assert_eq!(request.pass(), "");
    }

    #[test]
    fn account_id_falls_back_to_unknown() {
        assert_eq!(
            WatchRequest::parse(r#"{"__watch":1}"#)
                .unwrap()
                .account_id(),
            "unknown"
        );
        assert_eq!(
            WatchRequest::parse(r#"{"__watch":1,"accountId":""}"#)
                .unwrap()
                .account_id(),
            "unknown"
        );
    }

    #[test]
    fn credentials_report_the_documented_failures() {
        let invalid = WatchRequest::parse(r#"{"__watch":1,"user":"u","pass":"p"}"#).unwrap();
        assert_eq!(invalid.credentials(), Err(RejectReason::InvalidTarget));

        let missing_user =
            WatchRequest::parse(r#"{"__watch":1,"host":"imap.qq.com","port":993,"pass":"p"}"#)
                .unwrap();
        assert_eq!(
            missing_user.credentials(),
            Err(RejectReason::MissingCredentials)
        );

        let missing_pass =
            WatchRequest::parse(r#"{"__watch":1,"host":"imap.qq.com","port":993,"user":"u"}"#)
                .unwrap();
        assert_eq!(
            missing_pass.credentials(),
            Err(RejectReason::MissingCredentials)
        );
    }

    #[test]
    fn outbound_messages_match_the_frozen_shape() {
        assert_eq!(
            ClientMessage::Mail {
                account_id: "acc_1".to_owned(),
                exists: 4
            }
            .to_json(),
            r#"{"type":"mail","accountId":"acc_1","exists":4}"#
        );
        assert_eq!(
            ClientMessage::Watching { exists: 3 }.to_json(),
            r#"{"type":"state","state":"watching","exists":3}"#
        );
        assert_eq!(
            ClientMessage::Reconnecting { retry_in_ms: 5000 }.to_json(),
            r#"{"type":"state","state":"reconnecting","retryInMs":5000}"#
        );
        assert_eq!(
            ClientMessage::Error {
                error: "boom".to_owned()
            }
            .to_json(),
            r#"{"type":"state","state":"error","error":"boom"}"#
        );
        assert_eq!(
            ClientMessage::Failed {
                error: "登录失败".to_owned()
            }
            .to_json(),
            r#"{"type":"state","state":"failed","error":"登录失败"}"#
        );
    }

    #[test]
    fn quotes_and_newlines_are_escaped() {
        let message = ClientMessage::Failed {
            error: "say \"hi\"\nnow".to_owned(),
        };
        let json = message.to_json();
        assert!(!json.contains('\n'), "newline must be escaped: {json}");
        assert!(json.contains(r#"\"hi\""#));
    }
}
