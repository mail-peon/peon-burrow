//! 带**行边界保护**的字节缓冲。
//!
//! ⚠️ 这一层不是过度设计：TLS 分片会在**任意字节位置**切开一行。一次 `read()` 可能拿到
//! 半行、也可能拿到三行半 —— 把每段当整行解析，会在「`* 12 EXISTS` 被切成 `* 12 EX` + `ISTS`」
//! 时解析出垃圾，而症状是「偶尔漏掉一封邮件」，几乎无法复现。

/// 按 CRLF 切行的缓冲。
#[derive(Debug, Default)]
pub(crate) struct LineBuffer {
    pending: Vec<u8>,
}

impl LineBuffer {
    /// 新建一个空缓冲。
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 追加一段刚收到的字节（可能在某一行中间被切开）。
    pub(crate) fn push(&mut self, chunk: &[u8]) {
        self.pending.extend_from_slice(chunk);
    }

    /// 取下一行（**不含** CRLF）；没有完整行时返回 `None`。
    pub(crate) fn next_line(&mut self) -> Option<String> {
        let position = self.pending.windows(2).position(|pair| pair == b"\r\n")?;
        let line = String::from_utf8_lossy(&self.pending[..position]).into_owned();
        self.pending.drain(..position + 2);
        Some(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_complete_line_comes_out_without_crlf() {
        let mut buffer = LineBuffer::new();
        buffer.push(b"* OK ready\r\n");
        assert_eq!(buffer.next_line().as_deref(), Some("* OK ready"));
        assert_eq!(buffer.next_line(), None);
    }

    #[test]
    fn a_half_line_waits_for_the_rest() {
        let mut buffer = LineBuffer::new();
        buffer.push(b"* 12 EX");
        assert_eq!(buffer.next_line(), None);
        buffer.push(b"ISTS\r\n");
        assert_eq!(buffer.next_line().as_deref(), Some("* 12 EXISTS"));
    }

    #[test]
    fn several_lines_in_one_chunk_come_out_one_by_one() {
        let mut buffer = LineBuffer::new();
        buffer.push(b"A0001 OK one\r\nA0002 OK two\r\nA0003 OK three\r\n");
        assert_eq!(buffer.next_line().as_deref(), Some("A0001 OK one"));
        assert_eq!(buffer.next_line().as_deref(), Some("A0002 OK two"));
        assert_eq!(buffer.next_line().as_deref(), Some("A0003 OK three"));
        assert_eq!(buffer.next_line(), None);
    }

    #[test]
    fn crlf_split_across_chunks_is_handled() {
        let mut buffer = LineBuffer::new();
        buffer.push(b"* OK ready\r");
        assert_eq!(buffer.next_line(), None);
        buffer.push(b"\n");
        assert_eq!(buffer.next_line().as_deref(), Some("* OK ready"));
    }

    #[test]
    fn an_empty_line_is_a_line() {
        let mut buffer = LineBuffer::new();
        buffer.push(b"\r\n");
        assert_eq!(buffer.next_line().as_deref(), Some(""));
    }

    #[test]
    fn non_utf8_bytes_do_not_panic() {
        // IMAP 的响应可以是任意字节（附件正文走字面量），行内容解码失败时只能 lossy
        let mut buffer = LineBuffer::new();
        buffer.push(&[0xFF, 0xFE, b'\r', b'\n']);
        assert!(buffer.next_line().is_some());
    }
}
