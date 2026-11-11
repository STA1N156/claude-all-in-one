//! 内建工具护栏策略句(热配置)。
//!
//! 2026-11-10 由 lib.rs 拆出(只搬不改行为)。

use super::*;

/// 内建工具护栏的**策略句**默认值(见 [`chat`] 里 `builtin_tool_guard` 的模块文档)。
///
/// 只有这几句自然语言是热配置的;工具闭集与能力替代表由代码按本次请求的 `tools`
/// 固定生成,配置里没有占位符可拼错。
///
/// 2026-08-13 首版刻意**不含**「否则回答会被截断」这类后果威胁,也不点名
/// 「Cursor 内建的终端/文件读写」—— 理由见 `builtin_tool_guard` 文档的
/// 「刻意没有写进去的东西」一节。要试那两个变体,改设置项即可,不用重新部署。
pub const DEFAULT_TOOL_GUARD_POLICY: &str = "清单里没有的能力,直接向用户说明你需要什么、\
     并请他提供 —— 不要尝试调用清单外的工具(那些调用不会返回任何结果),\
     也不要向用户解释这套工具命名规则。";

/// 热配置的护栏策略句上限。够写三五句中文,又不至于让一次误粘贴把每个请求的
/// 系统提示顶掉几千 token。
pub(super) const TOOL_GUARD_POLICY_MAX_CHARS: usize = 2000;

/// 护栏策略句(进程级全局)。`None` = 未配 / 配置非法 → 用
/// [`DEFAULT_TOOL_GUARD_POLICY`]。
///
/// 存 `Arc<str>` 而不是 `String`:读侧(每个请求都读)拿快照后**立刻放锁**,
/// 绝不在拼几 KB 长提示的过程里持着锁。
pub(super) static TOOL_GUARD_POLICY: std::sync::RwLock<Option<Arc<str>>> = std::sync::RwLock::new(None);

/// 当前生效的护栏策略句。
pub fn tool_guard_policy() -> Arc<str> {
    // 快照即放锁(读守卫在表达式结束就 drop)。
    let snap = TOOL_GUARD_POLICY.read().ok().and_then(|g| g.clone());
    snap.unwrap_or_else(|| Arc::from(DEFAULT_TOOL_GUARD_POLICY))
}

/// 热应用护栏策略句(worker 30s 设置环调用)。空 = 回内置默认。
///
/// 校验失败**返回 Err 并保留上一份有效值**,绝不静默切回默认 —— 那会让一次误配置
/// 表现成「护栏悄悄变了一版」,而这道护栏的效果正在被按小时对比。
/// 非 cursor 进程从不读它,写到它身上是无害 no-op(与 `set_extra_models` 同)。
pub fn set_tool_guard_policy(text: &str) -> Result<(), String> {
    let trimmed = text.trim();
    if trimmed.chars().count() > TOOL_GUARD_POLICY_MAX_CHARS {
        return Err(format!(
            "护栏策略句过长({} 字符,上限 {})",
            trimmed.chars().count(),
            TOOL_GUARD_POLICY_MAX_CHARS
        ));
    }
    let next = (!trimmed.is_empty()).then(|| Arc::from(trimmed));
    match TOOL_GUARD_POLICY.write() {
        Ok(mut g) => {
            *g = next;
            Ok(())
        }
        Err(_) => Err("护栏策略句锁已中毒".to_string()),
    }
}

/// 串行化「改护栏策略句」的测试。它是**进程级全局**,并发跑的测试会互相顶掉对方的
/// 配置(与 `models::CATALOG_TEST_LOCK` 同一个理由:`EXTRA_MODELS` 也是全局)。
#[cfg(test)]
pub(crate) static GUARD_TEST_LOCK: Mutex<()> = Mutex::new(());


/// 当前策略句的短指纹,给 `/health` 与内建收口日志回显。
///
/// **只回显指纹不回显全文**:全文会跟着 worker 的健康快照到处走,而它是每个请求
/// 都发给上游的系统提示的一部分。指纹足够回答「线上跑的是哪一版、从什么时候起」,
/// 这正是按版本分桶比对收口率需要的那一个字段。
pub fn tool_guard_rev() -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(tool_guard_policy().as_bytes());
    h.finalize()[..4]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}


