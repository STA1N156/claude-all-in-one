//! 会话注册表(conversation_id × 账号 → 续轮阶段)。
//!
//! 2026-11-10 由 lib.rs 拆出(只搬不改行为)。

use super::*;

/// 会话在**服务端**是否已经建立,以及建在哪个号上。
///
/// ## 为什么必须记账号
///
/// Cursor 的会话历史由服务端按 `conversation_id` 持有,而它**属于某一个账号**。
/// 换号 = 失忆:新号那边根本没有这个会话。所以「同一会话继续」的前提是
/// **同一个账号**;账号不同就必须降级成首轮,把历史整个重铺一遍。
///
/// ## 为什么是「成功后才记」
///
/// 请求失败(限流/截断/空回复)时服务端很可能没有落下这一轮。乐观登记的话,
/// 下一次会用 `Continuation` 只发新消息 —— 而服务端那边缺了上一轮,
/// 表现是模型答非所问,且**没有任何错误**。宁可多铺一次历史。
#[derive(Default)]
pub(crate) struct ConvRegistry {
    pub(super) inner: Mutex<HashMap<String, ConvEntry>>,
    /// 有状态会话是否启用。**构造时读一次**,不是每个请求读一次环境变量。
    ///
    /// 关闭时这张表**完全不写**:`confirm` 每次成功都插入 + 全表 `retain` 扫过期,
    /// 而关闭状态下没人会读它 —— 纯浪费,还把所有流的收尾串在同一把锁上。
    pub(super) stateful: bool,
}

pub(super) struct ConvEntry {
    pub(super) account_id: String,
    pub(super) at: Instant,
    /// CLI 形态专用:已发往服务端的逐轮指纹(见 `chat::history_fps`)。
    ///
    /// 服务端持史之后,调用方若在**下一轮改写了历史**(/compact、编辑重发),
    /// 服务端那份就与调用方看到的不一致 —— 继续只发增量等于让模型看两份
    /// 不同的历史。逐轮指纹做前缀校验,分叉即换新 conversation_id 重铺。
    /// IDE 形态不用它(历史每轮折叠重发,无所谓分叉)。
    pub(super) fps: Vec<u64>,
    /// 服务端实际持有的会话 id。**大多数情况等于本条目的 key**;分叉重铺后
    /// 不同:分叉轮换了新 id 发 Opening(旧 id 服务端还有旧历史,直接重铺
    /// 会粘),而登记落回**基础 id**(调用方亲和键派生、跨轮稳定)之下,
    /// 靠这个字段回指到真正的上游会话。没有它,分叉一次之后就永远查不到
    /// 重铺成果 —— 每轮 lookup 基础 id 看到的都是旧指纹,永久 Diverged,
    /// 每轮全量重铺、模拟缓存按每轮新 key 永不命中(2026-09-13 生产实病:
    /// pi 长会话 6 小时 527 次分叉、30 个会话全残)。
    pub(super) wire_id: String,
}

/// CLI 形态的会话判定结果(见 [`ConvRegistry::cli_lookup`])。
pub(crate) enum CliLookup {
    /// 服务端没有这个会话(或换了号):Opening。
    Fresh,
    /// 本地记录是调用方历史的严格前缀:Continuation,只发最后一条新消息。
    /// 携服务端实际持有的会话 id(通常等于查询用的基础 id;分叉重铺过的
    /// 会话是基础 id 条目里回指的新 id,见 [`ConvEntry::wire_id`])。
    Continue(String),
    /// 调用方改写了历史:必须换 conversation_id + Opening 重铺。
    Diverged,
}

impl ConvRegistry {
    /// `CURSOR_STATEFUL=1` 才启用。见 [`ConvRegistry::stateful`]。
    pub(super) fn from_env() -> Self {
        // 2026-08-08 起**默认开启**:后续轮只发新消息,历史由服务端按 `1.5` 自持。
        // 实测两个事实跨 4 轮全部记住,且缓存命中率 32.6% → 49.8%(单轮最高 98.7%)。
        // 之前默认关是因为后续轮会静默挂起,根因已定位(上下文声明被错误地挪到了
        // 会话级 `1.2.17`,见 PROTOCOL §17)。
        //
        // `CURSOR_STATEFUL=0` 退回每轮全量重铺 —— 留这条退路是因为
        // "服务端记不记得" 这件事我方无法验证,只能靠模型答得对不对间接判断;
        // 万一上游改了行为,关掉它至少还是正确的(只是贵)。
        let stateful = std::env::var("CURSOR_STATEFUL").as_deref() != Ok("0");
        if !stateful {
            tracing::warn!("CURSOR_STATEFUL=0:每轮重铺全量历史(正确但更贵,且吃不到上游缓存)");
        }
        Self {
            inner: Mutex::new(HashMap::new()),
            stateful,
        }
    }

    /// 锁:毒化即恢复。这张表是纯缓存,毒化不代表数据不可用;
    /// 而 `unwrap` 会让此后**每个** cursor 请求在必经路径上 panic = provider 整体下线。
    pub(super) fn map(&self) -> std::sync::MutexGuard<'_, HashMap<String, ConvEntry>> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// 会话记录的存活时长。超时即当作服务端已经忘了,降级重铺。
///
/// 取 2 小时是保守值:宁可多铺一次历史(代价=一次全量上下文),
/// 也不要拿着过期的记录发 `Continuation`(代价=模型看不到历史却毫无报错)。
pub(super) const CONV_TTL: Duration = Duration::from_secs(2 * 3600);

impl ConvRegistry {
    /// 判定本次该用哪种形态。**任何不确定都返回 `Opening`** —— 降级只多花 token,
    /// 而错判 `Continuation` 会让模型丢失上下文且不报错。
    pub(super) fn phase_for(&self, conversation_id: &str, account_id: &str) -> run::Phase {
        // 关掉时一律走 `Opening` + 历史折叠(见 `chat::fold_history`)——
        // 那条路更贵但同样正确。
        if !self.stateful {
            return run::Phase::Opening;
        }
        let map = self.map();
        match map.get(conversation_id) {
            Some(e) if e.account_id == account_id && e.at.elapsed() < CONV_TTL => {
                run::Phase::Continuation
            }
            Some(e) if e.account_id != account_id => {
                tracing::info!(
                    conversation_id,
                    old = %e.account_id,
                    new = %account_id,
                    "cursor 会话换号,降级重铺历史(服务端会话属于旧号)"
                );
                run::Phase::Opening
            }
            _ => run::Phase::Opening,
        }
    }

    /// 本轮**成功**收尾后登记:服务端现在持有这个会话了。
    /// `fps` 是 CLI 形态的逐轮指纹(见 [`ConvEntry::fps`]);IDE 形态传空表。
    ///
    /// 落库 key 一律用**基础 id**(调用方亲和键派生、跨轮稳定),
    /// `wire_id` 记本轮实际发给上游的会话 id —— 两者只在分叉重铺轮不同
    /// (分叉轮换了新 id;登记仍落基础 id,下轮 lookup 才能命中并经
    /// `wire_id` 回指到上游真正持有的会话,见 [`CliLookup::Continue`])。
    pub(super) fn confirm_with_fps(
        &self,
        conversation_id: &str,
        account_id: &str,
        fps: Vec<u64>,
        wire_id: &str,
    ) {
        // 关闭时没人读这张表,写它纯属浪费(且把所有流的收尾串在同一把锁上)。
        if !self.stateful {
            return;
        }
        let mut map = self.map();
        map.insert(
            conversation_id.to_string(),
            ConvEntry {
                account_id: account_id.to_string(),
                at: Instant::now(),
                fps,
                wire_id: wire_id.to_string(),
            },
        );
        // 顺手清过期项:这张表按会话增长,没人清就是内存泄漏。
        map.retain(|_, e| e.at.elapsed() < CONV_TTL);
    }

    /// CLI 形态的会话判定。`history_fps` 是本次请求**除最后一条新消息外**的
    /// 全部历史轮指纹。
    ///
    /// 判定不变式与 `phase_for` 相同:任何不确定都当 Opening(Fresh/Diverged),
    /// 宁可重铺不错续。
    pub(super) fn cli_lookup(
        &self,
        conversation_id: &str,
        account_id: &str,
        history_fps: &[u64],
    ) -> CliLookup {
        if !self.stateful {
            return CliLookup::Fresh;
        }
        let map = self.map();
        match map.get(conversation_id) {
            Some(e) if e.account_id == account_id && e.at.elapsed() < CONV_TTL => {
                if e.fps.len() <= history_fps.len()
                    && e.fps.iter().zip(history_fps).all(|(a, b)| a == b)
                {
                    // 回指上游真正持有的会话 id:分叉重铺过的会话,key 是基础 id,
                    // 但服务端的会话是 `wire_id`(见 `ConvEntry::wire_id`)。
                    CliLookup::Continue(e.wire_id.clone())
                } else {
                    tracing::info!(
                        conversation_id,
                        stored = e.fps.len(),
                        incoming = history_fps.len(),
                        "cursor CLI:调用方历史与已发记录分叉(/compact 或编辑重发?),换新会话重铺"
                    );
                    CliLookup::Diverged
                }
            }
            Some(e) if e.account_id != account_id => {
                tracing::info!(
                    conversation_id,
                    old = %e.account_id,
                    new = %account_id,
                    "cursor 会话换号,降级重铺历史(服务端会话属于旧号)"
                );
                CliLookup::Fresh
            }
            _ => CliLookup::Fresh,
        }
    }

    /// 本轮失败:服务端可能没落下这一轮,下次从首轮重来。
    pub(super) fn forget(&self, conversation_id: &str) {
        if !self.stateful {
            return;
        }
        self.map().remove(conversation_id);
    }
}
