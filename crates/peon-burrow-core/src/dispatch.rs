//! 第一帧分流：这条连接是**透传**还是 **watch**。
//!
//! ⚠️ 顺序不能动（TS 版在这里踩过三个坑，见 `ai-docs/04-parity-node-to-rust.md` C1–C12）：
//! 1. 上游 TCP **立刻**建（不等第一帧）—— 有些客户端一个字节都不发，只等连接结果；
//! 2. 策略检查在建连**之前**同步完成 —— 被拒的连接一个字节都不发；
//! 3. 分流在**建连之后、转发之前** —— 否则 watch 的 JSON 会被当成 IMAP 命令发出去；
//! 4. 判定为透传时，被扣下的那一帧要**补投**。

use peon_burrow_protocol::{RejectReason, WatchRequest};
use tokio_tungstenite::tungstenite::Message;

/// 第一帧的分类结果。
#[derive(Debug)]
pub(crate) enum Dispatch {
    /// 透传：`first_frame` 由调用方补投给上游。
    Passthrough,
    /// watch 模式（需要 `imap-watch` feature）。
    Watch(Box<WatchRequest>),
    /// 拒绝，并给出原因。
    Reject(RejectReason),
}

/// 判定第一帧属于哪种模式。
///
/// 判据只有「文本帧 + `__watch === 1`」一条；其它一切（二进制、非 JSON、`__watch` 是别的值）
/// 都按透传处理 —— 这样「字段暂时缺失」的连接能走到给出具体报错的那一层。
pub(crate) fn classify(frame: &Message, has_target: bool) -> Dispatch {
    let request = match frame {
        Message::Text(text) => WatchRequest::parse(text.as_str()),
        _ => None,
    };

    if let Some(request) = request {
        if cfg!(feature = "imap-watch") {
            return Dispatch::Watch(Box::new(request));
        }
        // 关掉 feature 时必须**明确拒绝**，不能静默当成透传
        return Dispatch::Reject(RejectReason::WatchNotEnabled);
    }

    if has_target {
        Dispatch::Passthrough
    } else {
        Dispatch::Reject(RejectReason::MissingTarget)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(body: &str) -> Message {
        Message::text(body)
    }

    #[test]
    fn a_watch_request_needs_the_feature() {
        let frame = text(r#"{"__watch":1,"host":"imap.qq.com","port":993}"#);
        let dispatched = classify(&frame, false);
        if cfg!(feature = "imap-watch") {
            assert!(
                matches!(dispatched, Dispatch::Watch(_)),
                "got {dispatched:?}"
            );
        } else {
            assert!(
                matches!(dispatched, Dispatch::Reject(RejectReason::WatchNotEnabled)),
                "got {dispatched:?}"
            );
        }
    }

    #[test]
    fn binary_frames_are_passthrough_when_there_is_a_target() {
        assert!(matches!(
            classify(&Message::Binary(vec![1, 2, 3].into()), true),
            Dispatch::Passthrough
        ));
    }

    #[test]
    fn binary_without_a_target_is_rejected() {
        assert!(matches!(
            classify(&Message::Binary(vec![1, 2, 3].into()), false),
            Dispatch::Reject(RejectReason::MissingTarget)
        ));
    }

    #[test]
    fn a_non_watch_text_frame_is_passthrough() {
        assert!(matches!(
            classify(&text("A0001 LOGIN"), true),
            Dispatch::Passthrough
        ));
        assert!(matches!(
            classify(&text("not json at all"), true),
            Dispatch::Passthrough
        ));
    }

    #[test]
    fn a_non_watch_text_frame_without_a_target_is_rejected() {
        assert!(matches!(
            classify(&text("A0001 LOGIN"), false),
            Dispatch::Reject(RejectReason::MissingTarget)
        ));
    }
}
