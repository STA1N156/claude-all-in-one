//! 截断提示表(下一轮告诉模型上次内建工具失败)。
//!
//! 2026-11-10 由 lib.rs 拆出(只搬不改行为)。

use super::*;

/// 「上一轮被内建工具截断」的待发纠偏标记,按 conversation 存。
///
/// ## 为什么不挂在 [`ConvRegistry`] 上
///
/// `ConvRegistry` 整张表被 `stateful` 开关门控(`CURSOR_STATEFUL=0` 时根本不写),
/// 而纠偏与有状态会话无关:客户端每轮都重传全量历史,那段半截回答**在两种模式下
/// 都在上下文里**,模型两种模式下都会重复撞墙。挂进去等于让一个退路开关顺手
/// 关掉一个不相干的修复。
///
/// ## 为什么按 conversation 而不按 (conversation, 账号)
///
/// 触发重复调用的是**客户端历史里那段半截回答**,不是服务端会话状态。换号之后
/// 客户端照样把它重传上来,风险不变,所以标记不该跟着账号失效。
///
/// 语义是**取走即消费**:只对紧接着的下一轮生效。留着不清会让同一段话术在整个
/// 会话里反复出现,而模型早就照做了 —— 那就变成噪声。
#[derive(Debug, Default)]
pub struct TruncationNotices {
    pub(super) inner: Mutex<HashMap<String, (Option<&'static str>, Instant)>>,
}

/// 纠偏标记的存活时间。短到只覆盖「用户看到半截回答、马上追问」这一下;
/// 隔了半小时才回来的那次,上下文早变了,那句「你上一轮…」会指错地方。
pub(super) const NOTICE_TTL: Duration = Duration::from_secs(600);
/// 标记表的条数上限(防一个爬虫式客户端把它顶爆)。超了先清过期,还超就整表丢弃 ——
/// 纠偏是尽力而为的优化,丢标记只是少一次提醒,绝不能变成内存泄漏。
pub(super) const NOTICE_MAX: usize = 4096;

impl TruncationNotices {
    pub(super) fn map(&self) -> std::sync::MutexGuard<'_, HashMap<String, (Option<&'static str>, Instant)>> {
        // 锁中毒也要能继续:纠偏丢了只是少一次提醒,不该把整条推理路径带崩。
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 记下「这个会话上一轮被内建工具截断了」。`cap` = 认得出的能力说法,`None` = 认不出。
    pub fn record(&self, conversation_id: &str, cap: Option<&'static str>) {
        if conversation_id.is_empty() {
            return;
        }
        let mut map = self.map();
        map.retain(|_, (_, at)| at.elapsed() < NOTICE_TTL);
        if map.len() >= NOTICE_MAX {
            tracing::warn!(entries = map.len(), "cursor 纠偏标记表超上限,整表丢弃");
            map.clear();
        }
        map.insert(conversation_id.to_string(), (cap, Instant::now()));
    }

    /// 取走标记(消费语义)。过期的当不存在。
    pub fn take(&self, conversation_id: &str) -> Option<Option<&'static str>> {
        let mut map = self.map();
        let (cap, at) = map.remove(conversation_id)?;
        (at.elapsed() < NOTICE_TTL).then_some(cap)
    }
}
