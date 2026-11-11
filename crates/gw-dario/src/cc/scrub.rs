//! 擦掉客户端 system 提示词里第三方客户端的名字(照搬 dario `cc-template.ts` 的 FRAMEWORK_PATTERNS),
//! 如「You are OpenCode」「GitHub Copilot」。消息正文不擦(见 `shape::rewrite_history`)。
//!
//! 与 JS 版对齐的两点:单词边界用 ASCII 口径(JS 的 `\b` 只认 ASCII,Rust 默认按 Unicode,
//! 「用opencode写」两边结果会不同);命中处前面是 `. / \ - _` 或后面是 `/ \` 的视为路径/标识
//! 的一部分,保留(dario#35:`/Users/foo/.openclaw/workspace/` 曾被擦成 `/Users/foo/./workspace/`)。

use std::sync::LazyLock;

use regex::Regex;

const B: &str = r"(?-u:\b)";

static SYSTEM_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        format!(r"(?i){B}(roo[- ]?cline|roo[- ]?code|big[- ]?agi|claude[- ]?bridge|amazon\s+q){B}"),
        format!(r"(?i){B}(openclaw|hermes|aider|cursor|windsurf|cline|continue|copilot|cody){B}"),
        format!(r"(?i){B}(zed|plandex|tabby|opencode|daytona){B}"),
        format!(r"(?i){B}(librechat|typingmind){B}"),
        format!(r"(?i){B}(openai|gpt-4|gpt-3\.5){B}"),
        r"(?i)powered by [a-z]+".to_string(),
        format!(r"(?i){B}gateway{B}"),
        format!(r"(?i){B}sessions_[a-z_]+{B}"),
    ]
    .iter()
    .map(|p| Regex::new(p).expect("擦除规则"))
    .collect()
});

/// system 提示词:全套规则。
pub(crate) fn scrub_system(text: &str) -> String {
    scrub_with(text, &SYSTEM_PATTERNS)
}

fn scrub_with(text: &str, patterns: &[Regex]) -> String {
    let mut result = text.to_string();
    for re in patterns {
        if !re.is_match(&result) {
            continue;
        }
        let src = result;
        let mut out = String::with_capacity(src.len());
        let mut last = 0;
        for m in re.find_iter(&src) {
            let before = src[..m.start()].chars().next_back();
            let after = src[m.end()..].chars().next();
            let in_path = matches!(before, Some('.' | '/' | '\\' | '-' | '_')) || matches!(after, Some('/' | '\\'));
            out.push_str(&src[last..m.start()]);
            if in_path {
                out.push_str(m.as_str());
            }
            last = m.end();
        }
        out.push_str(&src[last..]);
        result = out;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_scrub_removes_client_names_but_keeps_paths() {
        assert_eq!(scrub_system("You are OpenCode, the best"), "You are , the best");
        assert_eq!(scrub_system("respond with \"GitHub Copilot\"."), "respond with \"GitHub \".");
        // 路径 / 标识里的不动。
        assert_eq!(scrub_system("/Users/foo/.openclaw/workspace/"), "/Users/foo/.openclaw/workspace/");
        assert_eq!(scrub_system("see ~/opencode/config"), "see ~/opencode/config");
        // ASCII 单词边界:中文紧挨着也算词边界(与 JS 一致)。
        assert_eq!(scrub_system("用opencode写"), "用写");
        assert_eq!(scrub_system("Powered by Stripe"), "");
    }
}
