use super::*;

#[test]
fn windows_program_parsing_follows_the_quoted_and_escaped_forms() {
    // 无引号 + 参数
    assert_eq!(
        parse_windows_program("C:\\tools\\burrow.exe run").as_deref(),
        Some("C:\\tools\\burrow.exe")
    );
    // 有引号 + 参数（`sc qc` 里 binPath 的常态）
    assert_eq!(
        parse_windows_program("\"C:\\Program Files\\b\\burrow.exe\" run").as_deref(),
        Some("C:\\Program Files\\b\\burrow.exe")
    );
    // 带转义反斜杠的路径
    assert_eq!(
        parse_windows_program("\"C:\\\\dir with space\\\\burrow.exe\" run").as_deref(),
        Some("C:\\dir with space\\burrow.exe")
    );
    // 按 `CommandLineToArgvW`，`\"` 是**字面引号**（不是收尾引号），
    // 所以路径里那个 `"` 要保留 —— 这正是「不能简单按 `"` 切分」的原因。
    assert_eq!(
        parse_windows_program("\"C:\\weird\\burrow.exe\\\" run").as_deref(),
        Some("C:\\weird\\burrow.exe\" run")
    );
    assert_eq!(parse_windows_program("   "), None);
    assert_eq!(parse_windows_program("\"\""), None);
}

#[test]
fn windows_command_line_follows_create_process_rules() {
    assert_eq!(
        windows_command_line(r"C:\tools\burrow.exe", &["run".to_owned()]),
        r"C:\tools\burrow.exe run"
    );
    assert_eq!(
        windows_command_line(r"C:\Program Files\burrow.exe", &[]),
        r#""C:\Program Files\burrow.exe""#
    );
    // 结尾反斜杠必须翻倍，否则会把收尾引号转义掉
    assert_eq!(
        windows_command_line(r"C:\dir with space\", &[]),
        r#""C:\dir with space\\""#
    );
}
