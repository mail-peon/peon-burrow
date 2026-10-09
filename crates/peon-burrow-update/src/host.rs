//! 当前 host triple：**target 选择的唯一依据**。
//!
//! 清单里的 `assets[].target` 写的是 Rust 的 host triple（`x86_64-pc-windows-msvc`、
//! `aarch64-apple-darwin`、`x86_64-unknown-linux-gnu`）。这里用
//! `std::env::consts::ARCH` + `std::env::consts::OS` 拼出来 —— 拼不出来就返回 `None`，
//! 调用方会得到「本平台暂无更新」，**绝不猜一个 target**（`ai-docs/design/update-flow.md § 2.2`）。

/// 当前平台的 Rust host triple；识别不了返回 `None`。
///
/// ```no_run
/// # use peon_burrow_update::host_triple;
/// match host_triple() {
///     Some(triple) => println!("本平台 {triple}"),
///     None => println!("本平台暂无更新"),
/// }
/// ```
#[must_use]
pub fn host_triple() -> Option<String> {
    triple_for(std::env::consts::ARCH, std::env::consts::OS)
}

/// `(arch, os)` → host triple 的**纯函数**版本（`std::env::consts` 的取值喂进来即可单测）。
///
/// 只覆盖我们真的会发布的组合：Windows 的 `-msvc`、macOS 的 `-apple-darwin`、
/// Linux 的 `-unknown-linux-gnu`，每个都是 `x86_64` / `aarch64`（Windows 额外有 32 位 `i686`）。
///
/// ```no_run
/// # use peon_burrow_update::triple_for;
/// assert_eq!(triple_for("x86_64", "windows").as_deref(), Some("x86_64-pc-windows-msvc"));
/// assert_eq!(triple_for("aarch64", "macos").as_deref(), Some("aarch64-apple-darwin"));
/// assert_eq!(triple_for("x86_64", "linux").as_deref(), Some("x86_64-unknown-linux-gnu"));
/// assert_eq!(triple_for("x86", "macos"), None); // 32 位 macOS 不是发布目标
/// ```
#[must_use]
pub fn triple_for(arch: &str, os: &str) -> Option<String> {
    let arch = match arch {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        // Rust 的 `std::env::consts::ARCH` 对 32 位 x86 报 "x86"，而 target 里叫 i686
        "x86" => "i686",
        _ => return None,
    };

    let triple = match os {
        "windows" => format!("{arch}-pc-windows-msvc"),
        // 32 位 macOS 从来不是发布目标（Apple 早就不发 32 位工具链了）
        "macos" if arch != "i686" => format!("{arch}-apple-darwin"),
        "linux" => format!("{arch}-unknown-linux-gnu"),
        _ => return None,
    };
    Some(triple)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_combinations_map_to_published_targets() {
        assert_eq!(
            triple_for("x86_64", "windows").as_deref(),
            Some("x86_64-pc-windows-msvc")
        );
        assert_eq!(
            triple_for("aarch64", "windows").as_deref(),
            Some("aarch64-pc-windows-msvc")
        );
        assert_eq!(
            triple_for("x86", "windows").as_deref(),
            Some("i686-pc-windows-msvc")
        );
        assert_eq!(
            triple_for("x86_64", "macos").as_deref(),
            Some("x86_64-apple-darwin")
        );
        assert_eq!(
            triple_for("aarch64", "macos").as_deref(),
            Some("aarch64-apple-darwin")
        );
        assert_eq!(
            triple_for("x86_64", "linux").as_deref(),
            Some("x86_64-unknown-linux-gnu")
        );
        assert_eq!(
            triple_for("aarch64", "linux").as_deref(),
            Some("aarch64-unknown-linux-gnu")
        );
    }

    #[test]
    fn unknown_combinations_are_none_instead_of_a_guess() {
        assert_eq!(triple_for("x86", "macos"), None);
        assert_eq!(triple_for("riscv64", "linux"), None);
        assert_eq!(triple_for("x86_64", "freebsd"), None);
        assert_eq!(triple_for("wasm32", "unknown"), None);
    }

    #[test]
    fn host_triple_is_detectable_on_this_machine() {
        // 这台机器（CI / 开发机）必须是发布目标之一；否则 check() 永远只会说「本平台暂无更新」
        let triple = host_triple().expect("本测试所在平台应当是发布目标之一");
        assert!(
            triple.ends_with("-pc-windows-msvc")
                || triple.ends_with("-apple-darwin")
                || triple.ends_with("-unknown-linux-gnu"),
            "{triple}"
        );
        assert!(
            triple.starts_with(std::env::consts::ARCH)
                || (std::env::consts::ARCH == "x86" && triple.starts_with("i686")),
            "{triple}"
        );
    }
}
