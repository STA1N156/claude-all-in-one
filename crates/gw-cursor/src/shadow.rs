//! 续轮描述符影子表(DescriptorShadow)。
//!
//! 2026-11-10 由 lib.rs 拆出(只搬不改行为)。

use super::*;

/// 续轮描述符影子表(2026-08-23 影子模式,**纯观测**)。
///
/// 背景:官方客户端的跨轮状态 = 服务端在 Run 响应尾部把「续轮描述符」(顶层 `.3`)
/// 当不透明字节回声给客户端,客户端下轮原样回放(见 `run::descriptor_field3`)。
/// 唯一的消费者是日志 —— **绝不参与任何出站请求**。回放是下一阶段的事:
/// 等影子数据回答「同一条会话跨轮是否同号/同模型、描述符怎么变」再拍板。
///
/// 结构 = conversation_id → **一桶**血脉条目(codex 三轮 Finding 2 后重做):
/// 同一 conv 可因内容碰撞并存多条血脉,(conv, account) 键既挡不住同号血脉
/// 互覆,又会把「这号以前出现过吗」误当「相邻轮换没换号」。血脉判据与
/// 与已删除的 CLI 驱动(`clidrv::CliConversations::lookup`)同族:**条目 fps 是当前请求 history fps
/// 的前缀**,同号池内最长优先(同长取最近),同号没有才看全桶 —— 全桶最长
/// 若异号就是「换号」;并行血脉 fps 不同,永不互染。TTL 取 [`CONV_TTL`]。
#[derive(Default, Debug)]
pub(crate) struct DescriptorShadow {
    pub(super) inner: Mutex<HashMap<String, Vec<ShadowEntry>>>,
}

#[derive(Debug)]
pub(super) struct ShadowEntry {
    pub(super) account_id: String,
    pub(super) model: String,
    /// **完整前序指纹链** = 提交那一轮的请求历史(含本轮 user)+ 本轮 assistant 输出。
    ///
    /// 也就是「下一轮请求的历史(减末条 user)应该长什么样」。lookup 要求与它
    /// **精确相等**,见 [`DescriptorShadow::lookup`]。
    pub(super) fps: Vec<u64>,
    /// 本轮 assistant 输出的指纹(`chat::turn_fp(false, 渲染文本)`)。
    ///
    /// `None` = 这一轮没能算出输出指纹(失败轮 / 工具轮)。普通条目因此链残、
    /// 永不匹配;**工具轮条目**(`tool_turn`)另有一套宽松匹配,见
    /// [`DescriptorShadow::lookup`]。
    pub(super) assistant_fp: Option<u64>,
    /// 工具轮条目(2026-09-04 toolcont 探针路线)。
    ///
    /// 工具轮的 assistant 输出是一次 `tool_use`,其渲染含参数 JSON 的调用方再
    /// 序列化(键序/空白不由我方掌控),指纹算不准 → `assistant_fp` 恒 None,
    /// 链 = 工具轮请求历史(含当时末条 user)。下一轮(工具结果轮)的历史
    /// 减末条(tool_result)后,尾部正是那条 tool_use 一条 —— lookup 对这类
    /// 条目改用「链 == 历史减两条」的宽松匹配(结构性校验由调用方在进 CLI
    /// 形态前做过:末条 tool_result + 上一条 assistant tool_use)。
    pub(super) tool_turn: bool,
    /// 尾部 `.3` 的原始字节(不透明,永不解析修改)。
    pub(super) desc: Vec<u8>,
    /// `.1` 32B blob 引用个数(捕获时数好,读侧不再解)。
    #[allow(dead_code)] // 目前是测试与将来回放阶段才读;入库即日志打完
    pub(super) refs: usize,
    pub(super) at: Instant,
}

/// 单桶血脉上限(参照已删除的 CLI 驱动 `clidrv` 的 BUCKET_CAP):超了按最旧提交淘汰。
pub(super) const SHADOW_BUCKET_CAP: usize = 8;

/// **全表条目总量上限**(codex 审查 #6)。原先只有单桶上限与 TTL:桶数无界,
/// 高并发多会话下条目量只受 TTL 约束,而每份描述符是 500B~数 KB 的 `Vec<u8>` ——
/// 一万条活跃会话就是几十 MB 常驻。超限按最旧提交全表淘汰。
pub(super) const SHADOW_TOTAL_CAP: usize = 4096;

/// 一次提交的对比结果(供日志):与**同血脉最新一份**相比。
pub(crate) struct ShadowCommit {
    /// 内容指纹(变化对比的前缀来源)。
    pub fp: u64,
    /// 字节是否变化(与同血脉命中条目比)。`None` = 新血脉首份(无可比)。
    pub changed: Option<bool>,
    /// 相邻轮是否同号:同号池有前缀命中 → 同号;同号没有但全桶最长前缀
    /// 异号 → 换号;新血脉 → `None`。
    pub same_account: Option<bool>,
    /// 是否同模型(与同血脉命中条目比)。`None` = 新血脉首份。
    pub same_model: Option<bool>,
}

/// 流循环侧的影子捕获句柄:影子表 + 本轮的账号 id 与 history fps
/// (血脉判据,减末轮口径与 lookup 一致)。捆成一个参数传进
/// `stream_to_anthropic`,测试不关心的路径给 `None`。
pub(crate) struct ShadowFeed {
    pub map: std::sync::Arc<DescriptorShadow>,
    pub account_id: String,
    /// 本轮请求历史的**完整**逐轮指纹(含末条 user)。提交时链 = 它 ++ [assistant_fp]。
    /// 注意与 lookup 用的那份(减末条)不是同一个值,见 `DescriptorShadow::commit`。
    pub fps: Vec<u64>,
}

impl DescriptorShadow {
    /// 锁:毒化即恢复(同 [`ConvRegistry`] 的理由:纯观测缓存,不能变成 panic 点)。
    pub(super) fn map(&self) -> std::sync::MutexGuard<'_, HashMap<String, Vec<ShadowEntry>>> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// 干净收尾时提交本轮尾部 `.3`,并把这条血脉的指纹链推进一轮。
    ///
    /// `request_fps` = 本轮请求历史的逐轮指纹(**含本轮 user**,与 `chat::history_fps`
    /// 同口径);`assistant_fp` = 本轮我方 assistant 输出的指纹
    /// (`chat::turn_fp(false, 渲染文本)`,与 clidrv 的 `append_assistant_fp` 共用函数
    /// 以保证口径不漂移)。入库的链 = `request_fps ++ [assistant_fp]`,正是
    /// 「下一轮请求历史(减末条 user)应该长什么样」。
    ///
    /// 入库策略:找到**精确前序**(链 == `request_fps` 去掉末条)就**替换它**
    /// (同一条血脉每轮只留最新一条,与 clidrv 桶「每轮接替旧槽」同义);
    /// 找不到就追加(新血脉 / 并行分支互不覆盖)。桶满按最旧提交淘汰。
    ///
    /// 调用方(见 chat.rs 捕获处)保证只在流**干净收尾**时走到这里 —— 中途捕获的
    /// `.3` 可能装着未完结轮次的状态,提交错描述符 = 回放时上下文缺一块 = 模型静默变傻。
    pub(crate) fn commit(
        &self,
        conv: &str,
        account_id: &str,
        model: &str,
        request_fps: &[u64],
        assistant_fp: Option<u64>,
        tool_turn: bool,
        desc: &[u8],
        refs: usize,
    ) -> ShadowCommit {
        let fp = fnv1a64(desc);
        // 本轮入库的完整链。assistant_fp 缺失时链是残的 —— 照样入库(留痕),
        // 但 lookup 会硬性拒绝它,见 ShadowEntry::assistant_fp。
        let mut chain = request_fps.to_vec();
        if let Some(a) = assistant_fp {
            chain.push(a);
        }
        // 精确前序 = 本轮请求历史去掉末条 user。
        let predecessor = &request_fps[..request_fps.len().saturating_sub(1)];

        let mut map = self.map();
        // 顺手全表清过期:过期条目摘除,空桶摘键(同 AssetStore 的摊销式清扫)。
        map.retain(|_, bucket| {
            bucket.retain(|e| e.at.elapsed() < CONV_TTL);
            !bucket.is_empty()
        });
        let bucket = map.entry(conv.to_string()).or_default();
        let hit = bucket
            .iter()
            .position(|e| e.account_id == account_id && e.fps == predecessor);
        let out = match hit {
            Some(i) => {
                let e = &bucket[i];
                ShadowCommit {
                    fp,
                    changed: Some(e.desc != desc),
                    same_account: Some(true),
                    same_model: Some(e.model == model),
                }
            }
            None => ShadowCommit {
                fp,
                changed: None,
                same_account: None,
                same_model: None,
            },
        };
        match hit {
            // 同血脉推进:接替旧槽。
            Some(i) => {
                let e = &mut bucket[i];
                e.model = model.to_string();
                e.fps = chain;
                e.assistant_fp = assistant_fp;
                e.tool_turn = tool_turn;
                e.desc = desc.to_vec();
                e.refs = refs;
                e.at = Instant::now();
            }
            // 新血脉 / 并行分支:追加。
            None => {
                while bucket.len() >= SHADOW_BUCKET_CAP {
                    let idx = bucket
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, e)| e.at)
                        .map(|(i, _)| i)
                        .expect("桶非空必有最旧者");
                    bucket.remove(idx);
                }
                bucket.push(ShadowEntry {
                    account_id: account_id.to_string(),
                    model: model.to_string(),
                    fps: chain,
                    assistant_fp,
                    tool_turn,
                    desc: desc.to_vec(),
                    refs,
                    at: Instant::now(),
                });
            }
        }
        Self::enforce_total_cap(&mut map);
        out
    }

    pub(super) fn enforce_total_cap(map: &mut HashMap<String, Vec<ShadowEntry>>) {
        let mut total: usize = map.values().map(|b| b.len()).sum();
        while total > SHADOW_TOTAL_CAP {
            // 找全表最旧的那条。表被 TTL 与本闸压着,线性扫可接受。
            let Some((key, idx)) = map
                .iter()
                .filter_map(|(k, b)| {
                    b.iter()
                        .enumerate()
                        .min_by_key(|(_, e)| e.at)
                        .map(|(i, e)| (k.clone(), i, e.at))
                })
                .min_by_key(|(_, _, at)| *at)
                .map(|(k, i, _)| (k, i))
            else {
                break;
            };
            if let Some(b) = map.get_mut(&key) {
                b.remove(idx);
                if b.is_empty() {
                    map.remove(&key);
                }
            }
            total -= 1;
        }
    }

    /// 作废该会话在该账号下的全部描述符条目,返回摘掉的条数。
    ///
    /// 用在「服务端要求补传内容分节」之后:那说明我方回放的描述符指向的内容
    /// 服务端手里没有,留着它下一轮会再撞一次同样的墙,而每次撞墙都可能是一次
    /// 裸态(累计 3 次/60s 就让这个号冷却)。作废后下一轮走首轮形态重铺 ——
    /// 代价是这条会话不再省钱,但正确性回到已知可用的那条路上。
    ///
    /// 只摘同号的:同一条 conv 上别的账号的血脉与本次失败无关(描述符是
    /// per-account 的服务端状态)。
    pub(crate) fn invalidate(&self, conv: &str, account_id: &str) -> usize {
        let mut map = self.map();
        let Some(bucket) = map.get_mut(conv) else {
            return 0;
        };
        let before = bucket.len();
        bucket.retain(|e| e.account_id != account_id);
        let dropped = before - bucket.len();
        if bucket.is_empty() {
            map.remove(conv);
        }
        dropped
    }

    /// 回放读侧:取本轮该走的续轮描述符(直接就是请求 `1.1` 的原样字节)。
    ///
    /// `history_fps` = 本轮请求历史**减末条 user** 的逐轮指纹。返回 `None` = 没料,
    /// 调用方走首轮形态全量重铺。
    ///
    /// ## ⭐ 为什么这里比 clidrv 严:要求**精确相等**,不接受前缀
    ///
    /// clidrv 的 `CliConversations::lookup` 容忍「条目链是请求历史的严格前缀」——
    /// 落后几轮的条目照样 Resume。那对它是安全的:CLI **子进程自己持有真实历史**,
    /// 续上一个落后的进程不会丢轮。
    ///
    /// 描述符没有这条腿。条目链是严格前缀,意味着存在这份描述符不覆盖的轮次;
    /// 而续轮请求 `1.2` 只带本轮增量消息 —— 服务端按这张缺轮的清单取上下文,
    /// 模型看不到中间那几轮却照样回答,**没有任何错误信号**。所以判据必须是
    /// 链 == `history_fps` 精确相等,落后一条就重铺。
    ///
    /// 其余语义照搬 clidrv(同一套桶,不另发明规则):
    /// - **歧义即 Fresh**:多条精确相等的条目(并行分支撞上同一段历史)绝不瞎挑;
    /// - **跨号即 Fresh**:命中在别的账号上就重铺,不回退同号的短祖先
    ///   (那会静默丢中间轮);描述符本身也是 per-account 的服务端状态。
    /// - **换模型即 Fresh**:预算表带着上下文上限(实测 `.5.2` = 256000),
    ///   跨模型回放等于拿 A 模型的上下文账本给 B 用。
    /// - **链残即不匹配**:`assistant_fp` 为 `None` 的条目(失败轮/工具轮)硬性跳过。
    pub(crate) fn lookup(
        &self,
        conv: &str,
        account_id: &str,
        model: &str,
        history_fps: &[u64],
    ) -> Option<Vec<u8>> {
        let mut map = self.map();
        let bucket = map.get_mut(conv)?;
        // 读侧也顺手清过期(与 commit 同一摊销式清扫)。
        bucket.retain(|e| e.at.elapsed() < CONV_TTL);

        // 全桶扫描:先按「链精确相等」筛,再看账号/模型 —— 顺序重要。
        // 先筛账号会把「精确命中在别号」误判成「本号无命中」,那就退化成
        // 「回退同号短祖先」= clidrv 明确拒绝的那种静默丢轮。
        let exact: Vec<&ShadowEntry> = bucket
            .iter()
            .filter(|e| {
                if e.assistant_fp.is_some() && e.fps == history_fps {
                    return true;
                }
                // 工具轮条目(toolcont 路线):链 = 工具轮请求历史(含当时末条 user),
                // 没有 assistant 指纹(tool_use 块的调用方再序列化算不准,见
                // ShadowEntry::tool_turn)。本轮(工具结果轮)历史减末条 tool_result
                // 后尾部恰好多出那条 assistant tool_use —— 匹配 = 我方链 == 它再减
                // 这一条。「那一条确实是 tool_use」的结构校验在进 CLI 形态前已做
                // (chat::prev_assistant_has_tool_use),这里只看链。
                if e.tool_turn && history_fps.len() == e.fps.len() + 1 {
                    return e.fps[..] == history_fps[..history_fps.len() - 1];
                }
                false
            })
            .collect();
        match exact.len() {
            0 => None,
            1 => {
                let e = exact[0];
                if e.account_id != account_id {
                    tracing::info!(
                        conversation_id = conv,
                        bucket_len = bucket.len(),
                        "cursor wire:精确前序在别号(账号轮转),不回退短祖先,重铺"
                    );
                    return None;
                }
                if e.model != model {
                    tracing::info!(
                        conversation_id = conv,
                        stored = %e.model,
                        want = model,
                        "cursor wire:精确前序换了模型,重铺"
                    );
                    return None;
                }
                Some(e.desc.clone())
            }
            n => {
                tracing::info!(
                    conversation_id = conv,
                    tied = n,
                    bucket_len = bucket.len(),
                    "cursor wire:多条精确前序(并行分支同历史),歧义不挑,重铺"
                );
                None
            }
        }
    }

    /// 测试用:读出该会话某账号**最近提交**的条目(ref 个数 + 字节)。
    #[cfg(test)]
    pub(crate) fn get_for_test(&self, conv: &str, account_id: &str) -> Option<(usize, Vec<u8>)> {
        self.map().get(conv).and_then(|bucket| {
            bucket
                .iter()
                .filter(|e| e.account_id == account_id)
                .max_by_key(|e| e.at)
                .map(|e| (e.refs, e.desc.clone()))
        })
    }
}
