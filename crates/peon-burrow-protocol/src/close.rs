//! 关闭码，以及「关闭原因不能超过 123 字节」这条规范带来的截断规则。

/// 策略拒绝：token 不匹配、host 不在白名单、`tls=0` 连 993、没有目标等。
pub const POLICY_VIOLATION: u16 = 1008;

/// watch 启动失败（请求字段非法之类）。
pub const WATCH_FAILED: u16 = 1011;

/// 中继正在重启。
pub const RESTARTING: u16 = 1001;

/// 关闭原因的长度上限。
///
/// WebSocket 规范给的硬上限是 **123 字节**，超了 `close()` 会直接失败 ——
/// TS 版因此把**整个中继进程**打崩过（一个非法请求即可 DoS）。这里留 3 字节余量。
pub const MAX_REASON_BYTES: usize = 120;

/// 把关闭原因截断到 [`MAX_REASON_BYTES`] 字节以内，且**不切坏 UTF-8**。
///
/// ⚠️ 按**字节**截，不是按字符：随便切字符串可能把一个多字节字符切成半个，
/// 编码后的字节数反而变了。
pub fn truncate_close_reason(text: &str) -> String {
    if text.len() <= MAX_REASON_BYTES {
        return text.to_owned();
    }
    let mut end = 0;
    for (index, ch) in text.char_indices() {
        let next = index + ch.len_utf8();
        if next > MAX_REASON_BYTES {
            break;
        }
        end = next;
    }
    text[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_below_the_limit_is_untouched() {
        let text = "host not allowed";
        assert_eq!(truncate_close_reason(text), text);
    }

    #[test]
    fn exactly_the_limit_is_untouched() {
        let text = "a".repeat(MAX_REASON_BYTES);
        assert_eq!(truncate_close_reason(&text), text);
    }

    #[test]
    fn one_byte_over_is_cut_to_the_limit() {
        let text = "a".repeat(MAX_REASON_BYTES + 1);
        let cut = truncate_close_reason(&text);
        assert_eq!(cut.len(), MAX_REASON_BYTES);
    }

    #[test]
    fn chinese_is_cut_on_a_char_boundary() {
        // 每个汉字 3 字节：120 字节正好 40 个
        let text = "端口".repeat(30); // 60 个汉字 = 180 字节
        let cut = truncate_close_reason(&text);
        assert!(cut.len() <= MAX_REASON_BYTES);
        assert_eq!(cut.len(), 120);
        assert!(cut.chars().all(|c| c == '端' || c == '口'));
    }

    #[test]
    fn a_multi_byte_char_is_never_split() {
        // 41 个汉字 = 123 字节 → 只能保留 40 个（120 字节）
        let text = "好".repeat(41);
        let cut = truncate_close_reason(&text);
        assert_eq!(cut.chars().count(), 40);
        assert_eq!(cut.len(), 120);
    }

    #[test]
    fn emoji_are_never_split() {
        // 每个 emoji 4 字节：120 / 4 = 30 个
        let text = "🦀".repeat(31);
        let cut = truncate_close_reason(&text);
        assert_eq!(cut.chars().count(), 30);
        assert_eq!(cut.len(), 120);
    }
}
