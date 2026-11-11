use super::*;
use gw_core::provider::ChatUsage;
use gw_core::store::{UsageRecord, UsageSink};
use std::collections::BTreeMap;

/// 空 content 在入站被自愈而不是硬拒(2026-08-19 起):断流会在 Claude Code 会话历史
/// 留下空 text 块,硬拒 = 会话永久变砖。历史消息变空 → 按角色顶替:user 用「(空)」
/// (Kiro 拒纯空白 user content,2026-08-19 实测),assistant 用单空格(上游放行);
/// 消息条数不变,亲和键不漂移。
#[test]
fn empty_history_message_is_repaired_with_placeholder() {
    for (label, content) in [
        ("空串", serde_json::json!("")),
        ("纯空白", serde_json::json!("   \n ")),
        ("空数组", serde_json::json!([])),
        ("空 text 块", serde_json::json!([{"type": "text", "text": ""}])),
        (
            "text 块缺 text 字段",
            serde_json::json!([{"type": "text"}]),
        ),
        ("null", serde_json::Value::Null),
    ] {
        let mut body = serde_json::json!({"messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": content},
            {"role": "user", "content": "继续"}
        ]});
        let repairs =
            sanitize_message_contents(&mut body).unwrap_or_else(|e| panic!("{label} 应被修复而非拒绝: {e}"));
        assert!(!repairs.is_empty(), "{label} 应有修复记录");
        assert!(
            repairs.iter().any(|r| r.contains("messages[1]") && r.contains("占位符")),
            "{label} 应点名下标并记占位修复: {repairs:?}"
        );
        assert_eq!(
            body["messages"][1]["content"],
            serde_json::json!([{"type": "text", "text": " "}]),
            "{label} assistant 应用单空格顶替"
        );
        assert_eq!(body["messages"].as_array().unwrap().len(), 3, "{label} 消息条数不应变");
    }
    // 历史 user 消息变空:必须用「(空)」(纯空白 user content 会被 Kiro 拒)。
    let mut body = serde_json::json!({"messages": [
        {"role": "user", "content": "hi"},
        {"role": "assistant", "content": "好"},
        {"role": "user", "content": ""},
        {"role": "assistant", "content": "嗯"},
        {"role": "user", "content": "继续"}
    ]});
    sanitize_message_contents(&mut body).expect("历史空 user 应被修复");
    assert_eq!(
        body["messages"][2]["content"],
        serde_json::json!([{"type": "text", "text": "(空)"}]),
        "历史 user 应用「(空)」顶替"
    );
    // content 字段整个缺席,同理修复(assistant → 单空格)。
    let mut body = serde_json::json!({"messages": [
        {"role": "assistant"},
        {"role": "user", "content": "hi"}
    ]});
    let repairs = sanitize_message_contents(&mut body).expect("缺 content 应被修复");
    assert_eq!(repairs.len(), 1, "缺 content 应记一条修复: {repairs:?}");
    assert_eq!(
        body["messages"][0]["content"],
        serde_json::json!([{"type": "text", "text": " "}])
    );
}

/// 数组里混着空 text 块:只删空块,其余块(图片/tool_use/thinking)原样保留。
#[test]
fn empty_text_blocks_stripped_others_kept() {
    let mut body = serde_json::json!({"messages": [
        {"role": "user", "content": "hi"},
        {"role": "assistant", "content": [
            {"type": "thinking", "thinking": "想了一下", "signature": "s"},
            {"type": "text", "text": ""},
            {"type": "tool_use", "id": "tu_1", "name": "Bash", "input": {}},
            {"type": "text", "text": "  \n"}
        ]},
        {"role": "user", "content": "继续"}
    ]});
    let repairs = sanitize_message_contents(&mut body).expect("应修复");
    assert_eq!(repairs.len(), 1, "只记一条删块修复: {repairs:?}");
    assert!(repairs[0].contains("2 个空 text 块"), "应报删了 2 块: {repairs:?}");
    let blocks = body["messages"][1]["content"].as_array().unwrap();
    assert_eq!(blocks.len(), 2, "只剩 thinking + tool_use: {blocks:?}");
    assert_eq!(blocks[0]["type"], "thinking");
    assert_eq!(blocks[1]["type"], "tool_use");
}

/// 最后一条当前轮 user 消息为空:不能替用户编造输入,仍拒并点名下标。
#[test]
fn empty_final_user_message_still_rejected() {
    let mut body = serde_json::json!({
        "model": "claude-opus-5",
        "messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": "ok"},
            {"role": "user", "content": ""}
        ]
    });
    let err = sanitize_message_contents(&mut body).expect_err("当前轮空 user 应被拒");
    assert!(err.contains("messages[2]"), "应点名下标,实际: {err}");
    assert!(err.contains("role=user"), "应点名角色,实际: {err}");

    // 末尾空 assistant(prefill)则可以安全顶替 —— 一个空格的前缀不影响续写。
    let mut body = serde_json::json!({"messages": [
        {"role": "user", "content": "hi"},
        {"role": "assistant", "content": []}
    ]});
    let repairs = sanitize_message_contents(&mut body).expect("末尾空 assistant 应被修复");
    assert_eq!(repairs.len(), 1);
    assert_eq!(
        body["messages"][1]["content"],
        serde_json::json!([{"type": "text", "text": " "}])
    );
}

#[test]
fn non_empty_and_non_text_blocks_pass() {
    // 干净的报文:不动一个字节,修复记录为空。
    for (label, body) in [
        // 只带图片、没有文字的消息是合法的,不能误伤。
        ("纯图片", serde_json::json!({"messages": [{"role": "user", "content": [
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBOR"}}
        ]}]})),
        // tool_result / tool_use 同理。
        ("纯 tool_result", serde_json::json!({"messages": [{"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "tu_1", "content": "done"}
        ]}]})),
        // 正常文本 + 图片混排。
        ("文本+图片", serde_json::json!({"messages": [{"role": "user", "content": [
            {"type": "text", "text": "看这张图"},
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBOR"}}
        ]}]})),
        // 没有 messages 字段:不归本校验管,放行交给下游。
        ("缺 messages", serde_json::json!({"model": "claude-opus-5"})),
    ] {
        let mut b = body.clone();
        let repairs =
            sanitize_message_contents(&mut b).unwrap_or_else(|e| panic!("{label} 应放行: {e}"));
        assert!(repairs.is_empty(), "{label} 不该有修复记录: {repairs:?}");
        assert_eq!(b, body, "{label} 报文不应被改动");
    }
}

#[test]
fn switch_cap_benign_descends_full_ladder_risky_capped() {
    // 良性冷却类(限速/额度耗尽):解绑紧上限,沿优先级阶梯下探到组内所有号(cap=total),
    // 使高优先级层全限流时落到低优先级兜底池,而非把限流错误抛回客户端。
    assert_eq!(switch_cap(UpstreamErrorKind::RateLimited, 12, 2), 12);
    assert_eq!(switch_cap(UpstreamErrorKind::QuotaExhausted, 12, 2), 12);
    // ModelNotAvailable 亦良性:遍历组内找到有该模型的号(cap=total)。
    assert_eq!(switch_cap(UpstreamErrorKind::ModelNotAvailable, 12, 2), 12);
    // 风险类换号错误:仍守 general_cap(默认 2),防换号雪崩封号。
    assert_eq!(switch_cap(UpstreamErrorKind::ServerError, 12, 2), 2);
    assert_eq!(switch_cap(UpstreamErrorKind::TokenInvalid, 12, 2), 2);
    assert_eq!(switch_cap(UpstreamErrorKind::Network, 12, 2), 2);
    assert_eq!(switch_cap(UpstreamErrorKind::Other, 12, 2), 2);
    // total=0 兜底 max(1),不产生"0 上限=一次都不换"的退化。
    assert_eq!(switch_cap(UpstreamErrorKind::RateLimited, 0, 2), 1);
}

#[test]
fn region_correction_from_arn_returns_new_region_on_mismatch() {
    let arn = "arn:aws:codewhisperer:eu-central-1:663804501012:profile/YNQNP9NWWVCQ";
    assert_eq!(
        region_correction_from_arn(kiro_p().as_ref(), arn, Some("us-east-1")),
        Some("eu-central-1"),
        "号商导出写死 us-east-1,但 profileArn 揭示真实服务区是 eu-central-1 → 应予修正"
    );
}

#[test]
fn region_correction_from_arn_returns_none_when_matching() {
    let arn = "arn:aws:codewhisperer:us-east-1:881967719131:profile/Q4C9VNXVKREP";
    assert_eq!(
        region_correction_from_arn(kiro_p().as_ref(), arn, Some("us-east-1")),
        None,
        "已一致 → 不该产生多余的修正/持久化"
    );
}

#[test]
fn region_correction_from_arn_ignores_case_of_stored_region() {
    // admin PATCH extra 整块替换不做区域大小写归一,DB 里可能存进混合大小写值;
    // 与已归一的 discovered 值等价时不该误判"不同"触发多余重写(对抗审查 Medium)。
    let arn = "arn:aws:codewhisperer:us-east-1:881967719131:profile/Q4C9VNXVKREP";
    assert_eq!(region_correction_from_arn(kiro_p().as_ref(), arn, Some("US-EAST-1")), None);
    assert_eq!(region_correction_from_arn(kiro_p().as_ref(), arn, Some("Us-East-1")), None);
}

#[test]
fn region_correction_from_arn_returns_none_for_unknown_arn_region() {
    // ap-southeast-1 不在 STANDARD_PROFILE_REGIONS(us-east-1/eu-central-1)清单内,
    // 未知区不该污染已配置的已知区。
    let arn = "arn:aws:codewhisperer:ap-southeast-1:123456789012:profile/UNKNOWNXX";
    assert_eq!(region_correction_from_arn(kiro_p().as_ref(), arn, Some("us-east-1")), None);
}

#[test]
fn region_correction_from_arn_corrects_when_region_missing() {
    // 账号从未设过 region(极简号商导出场景)→ 只要 ARN 区域已知,也该补上。
    let arn = "arn:aws:codewhisperer:eu-central-1:663804501012:profile/YNQNP9NWWVCQ";
    assert_eq!(region_correction_from_arn(kiro_p().as_ref(), arn, None), Some("eu-central-1"));
}

#[test]
fn serialize_response_small_is_passthrough_json() {
    let msg = serde_json::json!({
        "type":"message","role":"assistant",
        "content":[{"type":"text","text":"hi"}]
    });
    let s = serialize_response_capped(&msg);
    // 正常体量:原样序列化,可被重新解析(详情页"格式化"视图据此渲染)。
    let v: serde_json::Value = serde_json::from_str(&s).expect("应是合法 JSON");
    assert_eq!(v["role"], "assistant");
    assert_eq!(v["content"][0]["text"], "hi");
}

#[test]
fn serialize_response_oversized_yields_valid_truncation_marker() {
    // 超 MAX_LOG_PAYLOAD_BYTES:不能存半截非法 JSON,必须是合法占位 + _truncated 标记。
    let huge = "x".repeat(MAX_LOG_PAYLOAD_BYTES + 1024);
    let msg = serde_json::json!({
        "type":"message","role":"assistant",
        "content":[{"type":"text","text": huge}]
    });
    let s = serialize_response_capped(&msg);
    assert!(s.len() <= MAX_LOG_PAYLOAD_BYTES, "占位体量应远小于上限");
    let v: serde_json::Value = serde_json::from_str(&s).expect("占位必须是合法 JSON");
    assert_eq!(v["_truncated"], true);
    assert_eq!(v["role"], "assistant");
}

#[test]
fn jittered_secs_within_bounds_and_degenerate() {
    // 退化区间(min>=max)恒返回 min。
    assert_eq!(jittered_secs(300, 300), 300);
    assert_eq!(jittered_secs(300, 200), 300);
    // 正常区间:多次采样都落在 [min, max] 内。
    for _ in 0..256 {
        let s = jittered_secs(QUOTA_POLL_MIN_SECS, QUOTA_POLL_MAX_SECS);
        assert!(
            (QUOTA_POLL_MIN_SECS..=QUOTA_POLL_MAX_SECS).contains(&s),
            "{s} 越界 [{QUOTA_POLL_MIN_SECS}, {QUOTA_POLL_MAX_SECS}]"
        );
    }
}

#[tokio::test]
async fn pending_writes_wait_idle_tracks_guards() {
    let pw = PendingWrites::new();
    assert!(
        pw.wait_idle(std::time::Duration::from_millis(10)).await,
        "无在途应立即 idle"
    );

    let guard = pw.enter();
    assert!(
        !pw.wait_idle(std::time::Duration::from_millis(50)).await,
        "guard 未释放应超时返回 false"
    );
    drop(guard);
    assert!(pw.wait_idle(std::time::Duration::from_millis(10)).await);

    // 异步任务持有 guard:完成(drop)应唤醒等待者。
    let g = pw.enter();
    let task = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        drop(g);
    });
    assert!(
        pw.wait_idle(std::time::Duration::from_secs(2)).await,
        "guard drop 应唤醒 wait_idle"
    );
    task.await.unwrap();
}

#[tokio::test]
async fn flush_dirty_extras_persists_rolled_token_and_clears_dirty() {
    let store = SqliteStore::open_in_memory().unwrap();
    store
        .create_account(
            "acc-1",
            "G0",
            "kiro",
            1,
            r#"{"refresh_token":"rt-old","region":"us-east-1"}"#,
        )
        .unwrap();
    // 内存里已是刷新后的新 token,但 DB 回写曾失败 → 标脏。
    let mut acc = Account {
        account_id: "acc-1".into(),
        provider: "kiro".into(),
        max_concurrency: 1,
        disabled: false,
        created_at: 0,
        extra: BTreeMap::new(),
    };
    acc.extra
        .insert("refresh_token".into(), serde_json::Value::String("rt-old".into()));
    let scheduler = AccountScheduler::new(vec![Arc::new(acc)], &Default::default());
    scheduler.queue_extra(&scheduler.account("acc-1").unwrap(),
        [("refresh_token".into(), serde_json::json!("rt-new"))].into());

    let locks: RefreshLocks = parking_lot::Mutex::new(std::collections::HashMap::new());
    flush_dirty_extras(&scheduler, &store, &locks, "停机排空").await;

    let row = store.get_account("acc-1").unwrap().unwrap();
    assert!(row.extra.contains("rt-new"), "rolling token 应已落盘: {}", row.extra);
    assert!(row.extra.contains("us-east-1"), "merge 语义:未带字段保留原值: {}", row.extra);
    assert!(scheduler.dirty_accounts().is_empty(), "落盘成功后应清脏位");

    // 并发刷新已自行落库清脏的账号:flush 锁内重查脏位后跳过,不回滚 DB。
    store
        .merge_account_extra("acc-1", r#"{"refresh_token":"rt-newer"}"#)
        .unwrap();
    flush_dirty_extras(&scheduler, &store, &locks, "二次排空").await;
    let row = store.get_account("acc-1").unwrap().unwrap();
    assert!(
        row.extra.contains("rt-newer"),
        "非脏账号不得被旧快照回滚: {}",
        row.extra
    );
}

/// mock provider:携带一批待上报的外部 token 轮换(cursor CLI 自刷新捕获)。
struct TokenUpdateMockProvider {
    updates: std::sync::Mutex<Vec<(String, BTreeMap<String, serde_json::Value>)>>,
}

#[async_trait::async_trait]
impl Provider for TokenUpdateMockProvider {
    fn family(&self) -> &'static str {
        "cursor"
    }
    fn account_schema(&self) -> &'static [gw_core::account::FieldSpec] {
        &[]
    }
    async fn list_models(&self) -> Result<Vec<gw_core::model::ModelInfo>, UpstreamError> {
        Ok(vec![])
    }
    async fn chat(
        &self,
        _req: ChatRequest,
        _ctx: &CallCtx,
    ) -> Result<gw_core::provider::ChatStream, UpstreamError> {
        unreachable!("本测试不打 chat")
    }
    async fn refresh_auth(&self, account: &Account) -> Result<Account, UpstreamError> {
        Ok(account.clone())
    }
    fn poll_token_updates(&self) -> Vec<(String, BTreeMap<String, serde_json::Value>)> {
        std::mem::take(&mut *self.updates.lock().unwrap())
    }
}

struct RacedProfileProvider {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl Provider for RacedProfileProvider {
    fn family(&self) -> &'static str { "kiro" }
    fn account_schema(&self) -> &'static [gw_core::account::FieldSpec] { &[] }
    async fn list_models(&self) -> Result<Vec<gw_core::model::ModelInfo>, UpstreamError> { Ok(vec![]) }
    async fn chat(&self, _: ChatRequest, _: &CallCtx) -> Result<gw_core::provider::ChatStream, UpstreamError> { unreachable!() }
    async fn refresh_auth(&self, account: &Account) -> Result<Account, UpstreamError> { Ok(account.clone()) }
    async fn discover_profile_arn(&self, _: &Account) -> Result<Option<String>, UpstreamError> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(Some("arn:aws:codewhisperer:eu-west-1:123456789012:profile/old-identity".into()))
    }
    async fn force_discover_profile_arn(&self, account: &Account) -> Result<Option<String>, UpstreamError> {
        self.discover_profile_arn(account).await
    }
}

#[tokio::test]
async fn pending_extra_profile_discovery_keeps_original_identity_baseline() {
    for force in [false, true] {
        for sync_before_completion in [false, true] {
            let provider = Arc::new(RacedProfileProvider { entered: Default::default(), release: Default::default() });
            let mut base = acct(&[("access_token", "at-A"), ("refresh_token", "rt-A"),
                ("subscription_title", "PAID"), ("region", "us-east-1")]);
            base.provider = "kiro".into();
            let base = Arc::new(base);
            let store = Arc::new(SqliteStore::open_in_memory().unwrap());
            store.create_account(&base.account_id, "G0", "kiro", 1, &serde_json::to_string(&base.extra).unwrap()).unwrap();
            let mut state = catalog_state_with_accounts("kiro", vec![base.clone()]);
            let mutable = Arc::get_mut(&mut state).unwrap();
            mutable.provider = provider.clone();
            mutable.store = Some(store.clone());
            let worker = state.clone();
            let query_base = base.clone();
            let query = tokio::spawn(async move {
                if force { worker.discover_paid_profile_arn(&query_base, "竞态测试").await; }
                else { worker.ensure_profile_arn(query_base).await; }
            });
            provider.entered.notified().await;
            let mut replacement = (*base).clone();
            replacement.extra.insert("access_token".into(), serde_json::json!("at-B"));
            replacement.extra.insert("refresh_token".into(), serde_json::json!("rt-B"));
            store.update_account_extra(&base.account_id, &serde_json::to_string(&replacement.extra).unwrap()).unwrap();
            if sync_before_completion { state.scheduler.sync_accounts(vec![Arc::new(replacement)]); }
            provider.release.notify_one();
            query.await.unwrap();
            let actual: serde_json::Value = serde_json::from_str(&store.get_account(&base.account_id).unwrap().unwrap().extra).unwrap();
            assert_eq!(actual["access_token"], "at-B");
            assert!(actual.get("profile_arn").is_none(), "旧 ARN 不得污染替换身份");
            assert_eq!(actual["region"], "us-east-1", "拒绝 ARN 时也拒绝派生服务区");
            assert!(actual.get("api_region").is_none());
            let memory = state.scheduler.account(&base.account_id).unwrap();
            assert_eq!(memory.extra_str("access_token"), Some("at-B"));
            assert!(memory.extra_str("profile_arn").is_none());
        }
    }
}

/// CLI 自刷新捕获的轮换:新 token 要进 scheduler + 落库;更旧的回声不得覆盖
/// (CAS 两道:同 token 跳过、expires_at 不更新跳过)。
#[tokio::test]
async fn adopt_provider_token_updates_persists_and_rejects_stale_echo() {
    let store = SqliteStore::open_in_memory().unwrap();
    store
        .create_account(
            "acc-c",
            "G0",
            "cursor",
            1,
            r#"{"access_token":"at-old","refresh_token":"rt-old","expires_at":"2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();
    let mut extra = BTreeMap::new();
    extra.insert("access_token".into(), serde_json::json!("at-old"));
    extra.insert("refresh_token".into(), serde_json::json!("rt-old"));
    extra.insert("expires_at".into(), serde_json::json!("2026-01-01T00:00:00Z"));
    let acc = Arc::new(Account {
        account_id: "acc-c".into(),
        provider: "cursor".into(),
        max_concurrency: 1,
        disabled: false,
        created_at: 0,
        extra,
    });
    let scheduler = AccountScheduler::new(vec![acc], &Default::default());
    let locks: RefreshLocks = parking_lot::Mutex::new(std::collections::HashMap::new());

    // ① 正常轮换:采纳 + 落库 + 清脏。
    let mut delta = BTreeMap::new();
    delta.insert("access_token".into(), serde_json::json!("at-new"));
    delta.insert("refresh_token".into(), serde_json::json!("rt-new"));
    delta.insert("expires_at".into(), serde_json::json!("2027-01-01T00:00:00Z"));
    let provider: Arc<dyn Provider> = Arc::new(TokenUpdateMockProvider {
        updates: std::sync::Mutex::new(vec![("acc-c".into(), delta)]),
    });
    adopt_provider_token_updates(&provider, &scheduler, &store, &locks).await;
    assert_eq!(
        scheduler.account("acc-c").unwrap().extra_str("access_token"),
        Some("at-new"),
        "scheduler 应换上捕获的新 token"
    );
    let row = store.get_account("acc-c").unwrap().unwrap();
    assert!(row.extra.contains("at-new"), "新 token 应落库: {}", row.extra);
    assert!(row.extra.contains("rt-new"), "新 rt 应落库: {}", row.extra);
    assert!(scheduler.dirty_accounts().is_empty(), "落库成功应清脏位");

    // ② 旧回声(expires_at 更老):不得覆盖刚采纳的新 token。
    let mut stale = BTreeMap::new();
    stale.insert("access_token".into(), serde_json::json!("at-stale"));
    stale.insert("expires_at".into(), serde_json::json!("2026-06-01T00:00:00Z"));
    let provider: Arc<dyn Provider> = Arc::new(TokenUpdateMockProvider {
        updates: std::sync::Mutex::new(vec![("acc-c".into(), stale)]),
    });
    adopt_provider_token_updates(&provider, &scheduler, &store, &locks).await;
    assert_eq!(
        scheduler.account("acc-c").unwrap().extra_str("access_token"),
        Some("at-new"),
        "旧回声不得覆盖新 token"
    );
    let row = store.get_account("acc-c").unwrap().unwrap();
    assert!(row.extra.contains("at-new"), "DB 也不得被旧回声覆盖: {}", row.extra);
}

#[tokio::test]
async fn pending_extra_failed_write_preserves_admin_fields_and_mixed_update() {
    let store = SqliteStore::open_in_memory().unwrap();
    let extra: BTreeMap<String, serde_json::Value> = serde_json::from_value(serde_json::json!({
        "access_token":"at-old", "refresh_token":"rt-old", "expires_at":"2026-01-01T00:00:00Z",
        "proxy":"old-proxy", "region":"old-region"
    })).unwrap();
    let base = Arc::new(Account { account_id: "delta".into(), provider: "cursor".into(),
        max_concurrency: 1, disabled: false, created_at: 0, extra });
    // 损坏行使条件写失败；随后管理员修复并更改无关配置。
    store.create_account("delta", "G0", "cursor", 1, "null").unwrap();
    let scheduler = AccountScheduler::new(vec![base.clone()], &Default::default());
    scheduler.queue_extra(&base, [("access_token".into(), serde_json::json!("at-new")),
        ("refresh_token".into(), serde_json::json!("rt-new")),
        ("expires_at".into(), serde_json::json!("2027-01-01T00:00:00Z"))].into());
    persist_pending_extra(&scheduler, Some(&store), "delta", "故障注入");
    assert!(scheduler.is_extra_dirty("delta"));
    let mut repaired = base.extra.clone();
    repaired.insert("proxy".into(), serde_json::json!("admin-proxy"));
    repaired.insert("region".into(), serde_json::json!("admin-region"));
    store.update_account_extra("delta", &serde_json::to_string(&repaired).unwrap()).unwrap();
    let locks: RefreshLocks = Default::default();
    for (at, exp, agent) in [("at-new", "2027-01-01T00:00:00Z", "agent-1"),
        ("at-stale", "2026-01-01T00:00:00Z", "agent-2")] {
        let delta = [("access_token".into(), serde_json::json!(at)),
            ("refresh_token".into(), serde_json::json!("rt-stale")),
            ("expires_at".into(), serde_json::json!(exp)),
            ("sand_agent_id".into(), serde_json::json!(agent))].into();
        let provider: Arc<dyn Provider> = Arc::new(TokenUpdateMockProvider {
            updates: std::sync::Mutex::new(vec![("delta".into(), delta)]),
        });
        adopt_provider_token_updates(&provider, &scheduler, &store, &locks).await;
        let actual: serde_json::Value = serde_json::from_str(&store.get_account("delta").unwrap().unwrap().extra).unwrap();
        assert_eq!(actual["refresh_token"], "rt-new");
        assert_eq!(actual["sand_agent_id"], agent);
        assert_eq!(actual["proxy"], "admin-proxy");
        assert_eq!(actual["region"], "admin-region");
    }
    assert!(!scheduler.is_extra_dirty("delta"));
    // 删除不是写成功；不得清掉待写状态并记录虚假的持久化成功。
    store.delete_account("delta").unwrap();
    scheduler.queue_extra(&scheduler.account("delta").unwrap(),
        [("sand_agent_id".into(), serde_json::json!("agent-3"))].into());
    persist_pending_extra(&scheduler, Some(&store), "delta", "删除竞态");
    assert!(scheduler.is_extra_dirty("delta"));
    scheduler.sync_accounts(vec![]);
    assert!(scheduler.dirty_accounts().is_empty());
}

/// 不带 access_token 的增量(如 sandchat 的 `sand_agent_id`):跳过 token CAS,
/// 纯 merge 进 scheduler + 落库,绝不碰凭据字段。
#[tokio::test]
async fn adopt_provider_token_updates_merges_plain_extra_delta() {
    let store = SqliteStore::open_in_memory().unwrap();
    store
        .create_account(
            "acc-s",
            "G0",
            "cursor",
            1,
            r#"{"access_token":"at-keep","refresh_token":"rt-keep"}"#,
        )
        .unwrap();
    let mut extra = BTreeMap::new();
    extra.insert("access_token".into(), serde_json::json!("at-keep"));
    extra.insert("refresh_token".into(), serde_json::json!("rt-keep"));
    let acc = Arc::new(Account {
        account_id: "acc-s".into(),
        provider: "cursor".into(),
        max_concurrency: 1,
        disabled: false,
        created_at: 0,
        extra,
    });
    let scheduler = AccountScheduler::new(vec![acc], &Default::default());
    let locks: RefreshLocks = parking_lot::Mutex::new(std::collections::HashMap::new());

    let mut delta = BTreeMap::new();
    delta.insert("sand_agent_id".into(), serde_json::json!("agent-42"));
    let provider: Arc<dyn Provider> = Arc::new(TokenUpdateMockProvider {
        updates: std::sync::Mutex::new(vec![("acc-s".into(), delta)]),
    });
    adopt_provider_token_updates(&provider, &scheduler, &store, &locks).await;

    let acc = scheduler.account("acc-s").unwrap();
    assert_eq!(acc.extra_str("sand_agent_id"), Some("agent-42"), "scheduler 应收下增量");
    assert_eq!(acc.extra_str("access_token"), Some("at-keep"), "凭据字段不得被动");
    let row = store.get_account("acc-s").unwrap().unwrap();
    assert!(row.extra.contains("agent-42"), "增量应落库: {}", row.extra);
    assert!(row.extra.contains("at-keep"), "merge 语义:凭据保留: {}", row.extra);
    assert!(scheduler.dirty_accounts().is_empty(), "落库成功应清脏位");
}

/// 记录到内存的假 sink,断言 finalize_usage 的落库决策。
struct FakeSink {
    rows: std::sync::Mutex<Vec<UsageRecord>>,
}
#[async_trait::async_trait]
impl UsageSink for FakeSink {
    async fn record(&self, usage: UsageRecord) -> anyhow::Result<()> {
        self.rows.lock().unwrap().push(usage);
        Ok(())
    }
}
fn fake_sink() -> Arc<FakeSink> {
    Arc::new(FakeSink {
        rows: std::sync::Mutex::new(vec![]),
    })
}

#[tokio::test]
async fn finalize_usage_records_when_usage_present() {
    let sink = fake_sink();
    let dyn_sink: Arc<dyn UsageSink> = sink.clone();
    let usage = ChatUsage {
        input_tokens: 100,
        output_tokens: 20,
        cache_read_tokens: 50,
        cache_creation_tokens: 7,
        ..Default::default()
    };
    finalize_usage(Some(&dyn_sink), "acct-1", "claude-x", "sk-cust", Some(&usage), true).await;
    let rows = sink.rows.lock().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].account_id, "acct-1");
    assert_eq!(rows[0].model, "claude-x");
    assert_eq!(rows[0].input_tokens, 100);
    assert_eq!(rows[0].output_tokens, 20);
    assert_eq!(rows[0].cache_read_tokens, 50);
    assert_eq!(rows[0].cache_creation_tokens, 7, "cache_creation 不得丢失");
    assert!(rows[0].success);
    assert_eq!(rows[0].client_key_id, "sk-cust", "客户 key 应归属落库");
}

#[tokio::test]
async fn finalize_usage_skips_when_no_usage() {
    let sink = fake_sink();
    let dyn_sink: Arc<dyn UsageSink> = sink.clone();
    finalize_usage(Some(&dyn_sink), "acct-1", "m", "", None, true).await;
    assert_eq!(sink.rows.lock().unwrap().len(), 0, "无 usage 不应落库");
}

#[tokio::test]
async fn finalize_usage_no_sink_is_noop() {
    // sink=None(库打开失败降级)→ 不 panic、不记录。
    let usage = ChatUsage::default();
    finalize_usage(None, "acct-1", "m", "", Some(&usage), false).await;
}

#[tokio::test]
async fn finalize_usage_propagates_failure_flag() {
    let sink = fake_sink();
    let dyn_sink: Arc<dyn UsageSink> = sink.clone();
    let usage = ChatUsage {
        output_tokens: 5,
        ..Default::default()
    };
    finalize_usage(Some(&dyn_sink), "a", "m", "", Some(&usage), false).await;
    let rows = sink.rows.lock().unwrap();
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].success, "失败流应记 success=false");
}

fn one_account_scheduler() -> AccountScheduler {
    AccountScheduler::new(vec![Arc::new(acct(&[]))], &Default::default())
}

fn chat_stream(
    items: Vec<Result<StreamItem, UpstreamError>>,
) -> gw_core::provider::ChatStream {
    Box::pin(futures::stream::iter(items))
}

/// 请求日志捕获参数下的最小 ChatRequest(model="m";store=None 时不会被实际使用)。
fn req_model_m() -> ChatRequest {
    ChatRequest::from_anthropic_body(serde_json::json!({"model":"m","messages":[]}))
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn collect_response_folds_nonstream_message_and_persists_usage() {
    let sched = one_account_scheduler();
    let lease = sched.acquire(Some("s")).await.unwrap();
    let sink = fake_sink();
    let dyn_sink: Arc<dyn UsageSink> = sink.clone();
    let items = vec![
        Ok(StreamItem::Sse(SseEvent::new(
            "message_start",
            serde_json::json!({"message":{"id":"msg_1","type":"message","role":"assistant","model":"m","content":[],"usage":{"input_tokens":4,"output_tokens":0}}}),
        ))),
        Ok(StreamItem::Sse(SseEvent::new(
            "content_block_start",
            serde_json::json!({"index":0,"content_block":{"type":"text","text":""}}),
        ))),
        Ok(StreamItem::Sse(SseEvent::new(
            "content_block_delta",
            serde_json::json!({"index":0,"delta":{"type":"text_delta","text":"hi"}}),
        ))),
        Ok(StreamItem::Sse(SseEvent::new("content_block_stop", serde_json::json!({"index":0})))),
        Ok(StreamItem::Sse(SseEvent::new(
            "message_delta",
            serde_json::json!({"delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),
        ))),
        Ok(StreamItem::Sse(SseEvent::new("message_stop", serde_json::json!({})))),
        Ok(StreamItem::Usage(ChatUsage {
            input_tokens: 4,
            output_tokens: 2,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            ..Default::default()
        })),
    ];
    let resp =
        collect_response(&sched, Some(&dyn_sink), None, None, lease, chat_stream(items), req_model_m(), String::new(), Arc::new(acct(&[])), std::time::Instant::now(), &kiro_p(), Wire::Anthropic, None, false).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["content"][0]["text"], "hi", "非流式应折叠成单个 Messages JSON");
    assert_eq!(v["stop_reason"], "end_turn");
    assert_eq!(v["usage"]["output_tokens"], 2);
    let rows = sink.rows.lock().unwrap();
    assert_eq!(rows.len(), 1, "非流式也应落库 usage");
    assert_eq!(rows[0].output_tokens, 2);
    assert!(rows[0].success);
}

#[tokio::test]
async fn collect_response_maps_sse_error_to_bad_gateway() {
    let sched = one_account_scheduler();
    let lease = sched.acquire(Some("s")).await.unwrap();
    let items = vec![
        Ok(StreamItem::Sse(SseEvent::new(
            "message_start",
            serde_json::json!({"message":{"id":"m","content":[]}}),
        ))),
        Ok(StreamItem::Sse(SseEvent::new(
            "error",
            serde_json::json!({"type":"error","error":{"type":"overloaded_error","message":"x"}}),
        ))),
    ];
    let resp = collect_response(&sched, None, None, None, lease, chat_stream(items), req_model_m(), String::new(), Arc::new(acct(&[])), std::time::Instant::now(), &kiro_p(), Wire::Anthropic, None, false).await;
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY, "SSE error 应回非流式错误");
    let v = body_json(resp).await;
    assert_eq!(v["error"]["type"], "overloaded_error");
}

#[tokio::test]
async fn collect_response_fold_failure_persists_failure() {
    // 折叠失败(缺 message_start)→ 502,且 usage 必须记 success=false(审查 #1:
    // 不能在折叠失败前抢报成功 / 记 success=true)。
    let sched = one_account_scheduler();
    let lease = sched.acquire(Some("s")).await.unwrap();
    let sink = fake_sink();
    let dyn_sink: Arc<dyn UsageSink> = sink.clone();
    let items = vec![
        Ok(StreamItem::Sse(SseEvent::new("content_block_stop", serde_json::json!({"index":0})))),
        Ok(StreamItem::Usage(ChatUsage {
            input_tokens: 5,
            output_tokens: 1,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            ..Default::default()
        })),
    ];
    let resp =
        collect_response(&sched, Some(&dyn_sink), None, None, lease, chat_stream(items), req_model_m(), String::new(), Arc::new(acct(&[])), std::time::Instant::now(), &kiro_p(), Wire::Anthropic, None, false).await;
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    let rows = sink.rows.lock().unwrap();
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].success, "折叠失败的请求 usage 应记 success=false");
}

fn acct(extra: &[(&str, &str)]) -> Account {
    let mut map = BTreeMap::new();
    for (k, v) in extra {
        map.insert((*k).to_string(), serde_json::Value::String((*v).to_string()));
    }
    Account {
        account_id: "a".into(),
        provider: "kiro".into(),
        max_concurrency: 1,
        disabled: false,
        created_at: 0,
        extra: map,
    }
}

/// stream_response 测试的 CallCtx 快捷构造(无亲和键)。
fn test_ctx(extra: &[(&str, &str)]) -> CallCtx {
    CallCtx {
        account: Arc::new(acct(extra)),
        session_id: String::new(),
        cache_key: String::new(),
    }
}

#[test]
fn loopback_listen_detection() {
    assert!(is_loopback_listen("127.0.0.1:9000"));
    assert!(is_loopback_listen("localhost:9000"));
    assert!(is_loopback_listen("[::1]:9000"));
    assert!(!is_loopback_listen("0.0.0.0:9000"));
    assert!(!is_loopback_listen("203.0.113.10:9000"));
    assert!(!is_loopback_listen("nonsense"));
}

#[test]
fn parse_rfc3339_known_values() {
    // 与 token.rs format_unix_utc 互逆。
    assert_eq!(parse_rfc3339_unix("2026-06-04T00:00:00Z"), Some(1_780_531_200));
    assert_eq!(parse_rfc3339_unix("1970-01-01T00:00:00Z"), Some(0));
}

#[test]
fn parse_rfc3339_rejects_garbage() {
    assert_eq!(parse_rfc3339_unix("not-a-date"), None);
    assert_eq!(parse_rfc3339_unix(""), None);
}

/// 用注册表构造真实的各家 provider(与线上 worker 同一入口),测它们声明的处理特征。
fn real_provider(family: &str) -> Arc<dyn Provider> {
    crate::registry::Registry::with_builtins()
        .build(family, &serde_json::Value::Null, reqwest::Client::new())
        .unwrap_or_else(|e| panic!("构造 {family} provider 失败: {e}"))
}

fn kiro_p() -> Arc<dyn Provider> {
    real_provider("kiro")
}

/// 连带回声不扣账号健康;同一个错误不打标记时照常上报(各 provider 默认都不打)。
#[test]
fn spared_echo_failures_are_not_reported() {
    let a = acct(&[("refresh_token", "rt")]);
    for family in ["kiro", "cursor"] {
        let p = real_provider(family);
        let err = UpstreamError::new(UpstreamErrorKind::ServerError, "连接没收到任何字节");
        assert!(should_report_inference_failure(p.as_ref(), &a, &err), "{family}:普通报错照常上报");
        assert!(
            !should_report_inference_failure(p.as_ref(), &a, &err.as_collateral_failure()),
            "{family}:连带回声不上报"
        );
    }
}

#[test]
fn no_token_is_not_fresh() {
    assert!(!has_fresh_token(kiro_p().as_ref(), &acct(&[])));
    assert!(!has_fresh_token(kiro_p().as_ref(), &acct(&[("access_token", "")])));
}

#[test]
fn dario_accounts_are_refresh_exempt() {
    // 令牌已交接给该号的 dario 进程:即使 caio 侧 token 早已过期/被清空,也绝不触发刷新。
    let mut a = acct(&[("access_token", "stale"), ("expires_at", "2020-01-01T00:00:00Z"), ("refresh_token", "")]);
    assert!(!has_fresh_token(kiro_p().as_ref(), &a), "非 dario 号:过期 token 照常需要刷新");
    let dario = real_provider("claude-dario");
    a.provider = "claude-dario".into();
    assert!(dario.refresh_exempt(&a));
    assert!(has_fresh_token(dario.as_ref(), &a));
    let mut empty = acct(&[]);
    empty.provider = "claude-dario".into();
    assert!(has_fresh_token(dario.as_ref(), &empty));
    // kiro 的 ksk_ API Key 号同样不可刷新;普通 kiro 号可以。
    let kiro = kiro_p();
    assert!(kiro.refresh_exempt(&acct(&[("kiro_api_key", "ksk_abcdef")])));
    assert!(!kiro.refresh_exempt(&acct(&[("refresh_token", "rt")])));
    // dario 号的 TokenInvalid(只来自管理器判定需要重新登录)直接上报,不做「刷新后重试」。
    let err = UpstreamError::new(UpstreamErrorKind::TokenInvalid, "needs relogin")
        .with_delivery(gw_core::error::RequestDelivery::Unknown);
    assert!(should_report_inference_failure(dario.as_ref(), &a, &err));
}

#[test]
fn token_without_expiry_is_fresh() {
    // 无 expires_at → 当作有效(靠 403 兜底)。
    assert!(has_fresh_token(kiro_p().as_ref(), &acct(&[("access_token", "t")])));
}

#[test]
fn needs_profile_discovery_paid_missing_arn() {
    // 付费(非 FREE)+ 无 profile_arn → 需要强制发现真实 ARN。
    assert!(needs_profile_discovery(&acct(&[(
        "subscription_title",
        "KIRO POWER"
    )])));
    // 空白 profile_arn 视同缺失。
    assert!(needs_profile_discovery(&acct(&[
        ("subscription_title", "KIRO PRO"),
        ("profile_arn", "   "),
    ])));
}

#[test]
fn needs_profile_discovery_free_or_present_arn_skips() {
    // 免费号:即便结构同构(带 client_secret、无 arn)也不发现,维持共享 ARN。
    assert!(!needs_profile_discovery(&acct(&[
        ("subscription_title", "KIRO FREE"),
        ("client_secret", "cs"),
    ])));
    // 订阅档未回填(缺 subscription_title)→ 保守跳过,绝不误污染健康免费号。
    assert!(!needs_profile_discovery(&acct(&[("client_secret", "cs")])));
    // 已有真实 profile_arn → 绝不覆盖。
    assert!(!needs_profile_discovery(&acct(&[
        ("subscription_title", "KIRO POWER"),
        (
            "profile_arn",
            "arn:aws:codewhisperer:us-east-1:1:profile/X"
        ),
    ])));
}


#[test]
fn expired_token_is_not_fresh() {
    // 过去时刻 → 需刷新。
    assert!(!has_fresh_token(kiro_p().as_ref(), &acct(&[
        ("access_token", "t"),
        ("expires_at", "2000-01-01T00:00:00Z"),
    ])));
}

#[test]
fn far_future_token_is_fresh() {
    assert!(has_fresh_token(kiro_p().as_ref(), &acct(&[
        ("access_token", "t"),
        ("expires_at", "2099-01-01T00:00:00Z"),
    ])));
}

#[test]
fn api_key_account_is_always_fresh() {
    // apikey 长期有效:即便标了过去的 expires_at,也不触发刷新。
    assert!(has_fresh_token(kiro_p().as_ref(), &acct(&[
        ("kiro_api_key", "ksk_abc"),
        ("access_token", "ksk_abc"),
        ("expires_at", "2000-01-01T00:00:00Z"),
    ])));
}

#[test]
fn prepare_extracts_image_blob_keeps_text() {
    // Anthropic 图片块:source.data 抽到 blob,报文里换 blob 引用;text 正文原样保留。
    let big = "A".repeat(2048);
    let raw = serde_json::json!({
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": "看这张图,顺便说下 a+b/c=d 是什么"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": big}}
            ]
        }]
    })
    .to_string();
    let (out, blobs) = prepare_log_payload(raw);
    assert_eq!(blobs.len(), 1, "应抽出 1 个图片 blob");
    assert_eq!(blobs[0].media_type, "image/png");
    assert!(out.contains("看这张图") && out.contains("a+b/c=d"), "正文完整保留");
    assert!(out.contains(&format!("blob:{}", blobs[0].hash)), "data 换成 blob 引用");
    assert!(!out.contains(&"A".repeat(2048)), "原始 base64 不入报文");
}

#[test]
fn prepare_passes_through_non_json() {
    // 非 JSON(如转换错误串)原样保留、无 blob(短串不触顶截断)。
    let raw = "<转换失败: bad request>".to_string();
    let (out, blobs) = prepare_log_payload(raw.clone());
    assert_eq!(out, raw);
    assert!(blobs.is_empty());
}

// ──────────────────────────────────────────────────────────────────────
// Phase 6 regression: write_request_log 守卫按 worker family 而非 account.provider
// ──────────────────────────────────────────────────────────────────────

/// 辅助:用内存 SQLite 调 write_request_log,返回落库的 kiro_payload。
fn log_kiro_payload_with_family(family: &str) -> String {
    use gw_core::store::RequestLogFilter;

    let store = Arc::new(SqliteStore::open_in_memory().unwrap());
    // 空 provider 字段:模拟 filter_by_provider 放行的 kiro 账号(或 dario 账号),
    // 守卫不应依赖此字段。
    let account = Arc::new(Account {
        account_id: "test-acc".into(),
        provider: String::new(), // 故意留空
        max_concurrency: 1,
        disabled: false,
        created_at: 0,
        extra: BTreeMap::new(),
    });
    let req = req_model_m();
    write_request_log(
        store.clone(),
        req,
        Some(account),
        "sk-test".into(),
        false,
        true,
        Some(200),
        None,
        Some(10),
        None,
        None,
        ResponseLog::None,
        real_provider(family).as_ref(),
    );
    let rows = store
        .list_request_logs(&RequestLogFilter::default(), 10)
        .unwrap();
    assert_eq!(rows.len(), 1, "应落库恰好 1 条");
    let detail = store.get_request_log(rows[0].id).unwrap().unwrap();
    detail.kiro_payload
}

#[test]
fn write_request_log_kiro_family_renders_kiro_payload_even_with_empty_provider() {
    // family="kiro" 时 kiro_payload 必须非空,且 account.provider 为空不影响渲染
    // (防止 filter_by_provider 放行的空-provider kiro 号丢日志)。
    let kp = log_kiro_payload_with_family("kiro");
    assert!(
        !kp.is_empty(),
        "family=kiro 时 kiro_payload 应非空,实际为空"
    );
    // render_kiro_payload 对最简请求(model=m, messages=[]) 应产出合法 JSON
    assert!(
        kp.starts_with('{') || kp.starts_with('<'),
        "kiro_payload 应是 JSON 对象或错误占位,实际: {kp}"
    );
}

#[test]
fn write_request_log_non_kiro_family_skips_kiro_payload() {
    // family="claude-dario" 时 kiro_payload 必须为空串
    // (不应把 Kiro 格式强行渲染给 dario/其他 provider)。
    let kp = log_kiro_payload_with_family("claude-dario");
    assert!(
        kp.is_empty(),
        "family=claude-dario 时 kiro_payload 应为空串,实际: {kp}"
    );
}

/// 造一个只用于窗口判定的调度器(不发请求,只读写 model_overloaded)。
fn window_sched() -> AccountScheduler {
    let acc = Arc::new(Account {
        account_id: "a".into(),
        provider: "kiro".into(),
        max_concurrency: 4,
        disabled: false,
        created_at: 0,
        extra: Default::default(),
    });
    AccountScheduler::new(vec![acc], &gw_core::config::SchedulerConfig::default())
}

#[test]
fn generic_5xx_upgraded_only_inside_overload_window() {
    use UpstreamErrorKind as K;
    let s = window_sched();

    // 窗口外:通用 5xx 必须保持 ServerError(仍换号 + 仍记账号失败),绝不靠猜升级。
    assert_eq!(
        correct_overload_kind(&s, "claude-opus-5", K::ServerError),
        K::ServerError,
        "没有上游显式信号时不得把通用 5xx 当过载"
    );

    // 收到显式过载 → 开窗,且自身仍是 Overloaded。
    assert_eq!(
        correct_overload_kind(&s, "claude-opus-5", K::Overloaded),
        K::Overloaded
    );
    // 窗口内:同一模型的通用 5xx 升级为 Overloaded。
    assert_eq!(
        correct_overload_kind(&s, "claude-opus-5", K::ServerError),
        K::Overloaded,
        "窗口内的通用 5xx 应按过载处理"
    );
    // **窗口是按模型隔离的**:opus-5 过载不能让 opus-4-8 的 5xx 也免罚,
    // 否则一个模型抖动会掩盖另一个模型的真故障。
    assert_eq!(
        correct_overload_kind(&s, "claude-opus-4-8", K::ServerError),
        K::ServerError,
        "过载窗口不得跨模型泄漏"
    );
}

#[test]
fn overload_window_never_touches_other_kinds() {
    use UpstreamErrorKind as K;
    let s = window_sched();
    correct_overload_kind(&s, "m", K::Overloaded); // 开窗
    // 窗口内也只升级 ServerError;别的 kind 各有自己的处置(禁号/冷却/400),不能被吞掉。
    for k in [K::TokenInvalid, K::RateLimited, K::TemporarilyBlocked, K::QuotaExhausted,
              K::Network, K::BadRequest, K::ModelNotAvailable, K::EmptyResponse, K::ContentFiltered, K::Other] {
        assert_eq!(correct_overload_kind(&s, "m", k), k, "{k:?} 不应被窗口改写");
    }
}

#[test]
fn overloaded_maps_to_529_others_unchanged() {
    use UpstreamErrorKind as K;
    assert_eq!(upstream_status(K::Overloaded).as_u16(), 529, "过载须走 Anthropic 的 529");
    // 既有映射一个都不能变。
    assert_eq!(upstream_status(K::BadRequest), StatusCode::BAD_REQUEST);
    assert_eq!(upstream_status(K::ModelNotAvailable), StatusCode::BAD_REQUEST);
    // 确定性内容级空流 → 400(止住重试风暴,文案亮给用户;见 upstream_status 注释)。
    assert_eq!(upstream_status(K::EmptyResponse), StatusCode::BAD_REQUEST);
    // 上游分类器拒答同理 → 400。
    assert_eq!(upstream_status(K::ContentFiltered), StatusCode::BAD_REQUEST);
    for k in [K::ServerError, K::Network, K::RateLimited, K::QuotaExhausted,
              K::TokenInvalid, K::TemporarilyBlocked, K::Other] {
        assert_eq!(upstream_status(k), StatusCode::BAD_GATEWAY, "{k:?} 应保持 502");
    }
}

#[tokio::test]
async fn overloaded_response_carries_anthropic_error_type() {
    async fn body_json(resp: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    let e = UpstreamError::new(UpstreamErrorKind::Overloaded, "high load").with_status(500);
    let resp = upstream_error_response(&e);
    assert_eq!(resp.status().as_u16(), 529);
    // 客户端(Claude Code / SDK)按 error.type == "overloaded_error" 判定可重试。
    let v = body_json(resp).await;
    assert_eq!(v["error"]["type"], "overloaded_error");
    assert_eq!(v["type"], "error");

    // ⚠️ **这里原先断言的是「非过载错误不带 error.type」——那条断言把一个 bug 钉住了。**
    //
    // Anthropic 的错误体里 `error.type` 是必填。少了它,按 schema 严格校验的客户端
    // (opencode 用 zod)会在**解析错误体时**就失败,把真正的 message 吞掉,
    // 客户只看到一句 "Invalid input: expected string, received undefined"
    // (2026-08-07 实测)。原来的顾虑是"造错 type 反而误导客户端重试" —— 顾虑本身
    // 合理,但结论反了:根本没法解析比 type 不够精确糟得多。`api_error` 是最保守的
    // 取值(服务端问题、可重试),而且与我们已经返回的 502 状态码一致。
    let se = UpstreamError::new(UpstreamErrorKind::ServerError, "boom").with_status(500);
    let sresp = upstream_error_response(&se);
    assert_eq!(sresp.status(), StatusCode::BAD_GATEWAY);
    let sv = body_json(sresp).await;
    assert_eq!(sv["error"]["type"], "api_error", "502 那一类必须给 api_error");
    assert!(sv["error"]["message"].is_string());
}

/// 上游身份指纹**一个字都不许**出现在对外响应里 —— 客户看不出这条渠道背后是谁,
/// 定价才谈得下去。三条对外出口(非流式响应 / 流内 error 事件 / 选号失败)全覆盖。
#[tokio::test]
async fn client_facing_errors_carry_no_upstream_fingerprint() {
    /// 真实报文(逐字取自 139 `caio-worker0` 日志)——脱敏前客户就是收到这一串。
    const REAL: &str = concat!(
        "kiro generateAssistantResponse 失败: 429 ",
        r#"{"message":"Too many requests, please wait before trying again.","#,
        r#""reason":"USER_REQUEST_RATE_EXCEEDED"}"#
    );
    const FINGERPRINTS: [&str; 5] = [
        "kiro",
        "Kiro",
        "generateAssistantResponse",
        "USER_REQUEST_RATE_EXCEEDED",
        "rate_limited", // 内部 kind 标签也别外泄
    ];

    let e = UpstreamError::new(UpstreamErrorKind::RateLimited, REAL).with_status(429);

    // ① 非流式 / 首包前:HTTP 响应体。
    let resp = upstream_error_response(&e);
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let http_body = String::from_utf8(bytes.to_vec()).unwrap();

    // ② 流内:SSE error 事件的 data(与 sse_error_event 下发的是同一份)。
    let sse_data = sse_error_payload(&e).to_string();

    // ③ 选号失败:AcquireError 的对外文案。
    let acquire = [
        scheduler::AcquireError::AllDisabled,
        scheduler::AcquireError::AllBusy,
        scheduler::AcquireError::AllRpmLimited,
        scheduler::AcquireError::Empty,
        scheduler::AcquireError::GroupEmpty,
        scheduler::AcquireError::NoModelSupport,
    ]
    .iter()
    .map(|a| a.client_message())
    .collect::<Vec<_>>()
    .join(" | ");

    for (label, text) in [("http", &http_body), ("sse", &sse_data), ("acquire", &acquire)] {
        for fp in FINGERPRINTS {
            assert!(!text.contains(fp), "{label} 出口泄露了 `{fp}`: {text}");
        }
        // 账号池形态同样不外露(分组/账号/worker 这类词是渠道形态的直接线索)。
        for fp in ["账号", "分组", "worker", "订阅"] {
            assert!(!text.contains(fp), "{label} 出口泄露了池形态 `{fp}`: {text}");
        }
    }
    // 反向:运维面必须**仍能**看到原文,否则导号/排查就瞎了。
    let admin = admin_error_response(&e);
    let ab = axum::body::to_bytes(admin.into_body(), 64 * 1024).await.unwrap();
    let admin_body = String::from_utf8(ab.to_vec()).unwrap();
    assert!(
        admin_body.contains("generateAssistantResponse") && admin_body.contains("kiro"),
        "运维面不该脱敏: {admin_body}"
    );
}

/// 上游把错误当**普通 SSE 事件**发来时(dario 直透 Anthropic 流),这条路绕开
/// `UpstreamError`。对抗评审两个镜头都点了它 —— 闸门必须也盖在这里。
#[test]
fn upstream_error_event_payload_is_rewritten_but_keeps_retry_type() {
    let leaky = serde_json::json!({
        "type": "error",
        "error": {
            "type": "overloaded_error",
            "message": r#"kiro generateAssistantResponse 失败: {"reason":"MODEL_TEMPORARILY_UNAVAILABLE"}"#
        }
    });
    let out = sanitize_upstream_error_payload(&leaky);
    let s = out.to_string();
    for fp in ["kiro", "generateAssistantResponse", "MODEL_TEMPORARILY_UNAVAILABLE"] {
        assert!(!s.contains(fp), "上游 error 事件仍泄露 `{fp}`: {s}");
    }
    // **type 必须原样保留**:客户端 SDK 按它判可重试,改了就是改重试语义(本次不做)。
    assert_eq!(out["error"]["type"], "overloaded_error");
    assert_eq!(out["type"], "error");
    assert_eq!(out["error"]["message"], "服务繁忙,请稍后重试");

    // 上游没给 type 的载荷:**必须补一个**,而不是留空。
    //
    // 这条原先断言"不该凭空造 error.type",理由是"造错反而误导客户端重试"。
    // 实测推翻了它:留空的后果是客户端连错误体都解析不了(见上面
    // `overloaded_response_carries_anthropic_error_type` 里的说明)。补 `api_error`
    // 是保守解 —— 语义是"服务端出问题、可重试",不指认客户的 key/请求有问题。
    let untyped = serde_json::json!({"error":{"message":"kiro boom"}});
    let out2 = sanitize_upstream_error_payload(&untyped);
    assert_eq!(out2["error"]["type"], "api_error", "缺 type 必须补,否则客户端解析不了");
    assert!(!out2.to_string().contains("kiro"));

    // 各 type 映射到语义相符的中性文案(反向:别全塞同一句)。
    let by_type = |t: &str| {
        sanitize_upstream_error_payload(&serde_json::json!({"error":{"type":t,"message":"x"}}))
            ["error"]["message"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(by_type("rate_limit_error"), "请求过于频繁,请稍后重试");
    assert_eq!(by_type("invalid_request_error"), "请求无效,请检查请求体后重试");
    assert_ne!(by_type("rate_limit_error"), by_type("overloaded_error"));
}

/// 登记过的本地文案要照发到客户端 —— 脱敏不能把"你请求体太大"这类可自助的信息也吃掉。
#[tokio::test]
async fn locally_generated_detail_still_reaches_client() {
    let e = UpstreamError::bad_request_visible("请求体 100 字节超出体积上限 50 字节");
    let resp = upstream_error_response(&e);
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let body = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(body.contains("超出体积上限"), "本地可见文案被误吞: {body}");
}

#[test]
fn overload_backoff_is_bounded_and_jittered_upward() {
    // 退避梯度必须递增且总时长有界:上界过大会让客户端干等,不如直接吐 529。
    assert!(OVERLOAD_BACKOFF_MS.windows(2).all(|w| w[0] < w[1]), "退避应递增");
    let total: u64 = OVERLOAD_BACKOFF_MS.iter().sum();
    assert!(total <= 4_000, "总退避 {total}ms 超过 4s,客户端体感太差");

    // 抖动:永不小于基值(不能把退避抖没了),且不超过 +MAX_JITTER_PCT%。
    // 取多次样本以覆盖 uuid 熵的分布,同时验证确实产生了不同的值。
    let hi = 1_000 + 1_000 * MAX_JITTER_PCT / 100;
    let mut seen = std::collections::BTreeSet::new();
    for _ in 0..200 {
        let d = jittered(1_000).as_millis() as u64;
        assert!((1_000..=hi).contains(&d), "抖动后 {d}ms 越界(应在 1000..={hi})");
        seen.insert(d);
    }
    // 最坏情况总退避(全部打满抖动)也要有界,否则客户端可能干等太久。
    let worst: u64 = OVERLOAD_BACKOFF_MS.iter()
        .map(|b| b + b * MAX_JITTER_PCT / 100)
        .sum();
    assert!(worst <= 5_000, "最坏总退避 {worst}ms 超过 5s");
    assert!(seen.len() > 1, "抖动没起作用(200 次采样只有一个值),并发请求会同步重撞");
}

// ───────── 分组头解析 ─────────

fn hdrs(v: Option<&str>) -> axum::http::HeaderMap {
    let mut h = axum::http::HeaderMap::new();
    if let Some(v) = v {
        h.insert(crate::GROUP_HEADER, v.parse().unwrap());
    }
    h
}

/// 头缺席 = 未分组请求:必须是 None,调用方据此走与本次重构之前完全相同的路径
/// (全量池)。滚动升级期间旧 router 不发这个头,靠的就是这条。
#[test]
fn group_header_absent_is_none() {
    assert_eq!(parse_group_header(&hdrs(None)).unwrap(), None);
}

/// 正常解析:组名原样取出,首尾空白裁掉。
#[test]
fn group_header_parses_name() {
    assert_eq!(parse_group_header(&hdrs(Some("GECO"))).unwrap().as_deref(), Some("GECO"));
    assert_eq!(parse_group_header(&hdrs(Some("  G0 "))).unwrap().as_deref(), Some("G0"));
}

/// 空头必须**报错而不是当成缺席** —— 当成缺席等于把一个本该受成员集限制的请求
/// 以全量池放行,静默提权且无任何痕迹。
#[test]
fn group_header_empty_is_error_not_ignored() {
    for bad in ["", "   "] {
        assert!(
            parse_group_header(&hdrs(Some(bad))).is_err(),
            "空分组头 {bad:?} 必须拒绝,绝不能静默按未分组放行"
        );
    }
}

/// 会话亲和键必须按分组分命名空间,否则低价流量能改写正常客户的账号钉扎
/// (`select_id` 的"primary 不合格 → 改选并当场转正、永不迁回")。
#[test]
fn affinity_key_is_namespaced_per_group() {
    let plain = group_scoped_affinity_key(None, None, Some("sess-1".into()));
    let scoped = group_scoped_affinity_key(Some("GECO"), None, Some("sess-1".into()));
    assert_eq!(plain.as_deref(), Some("sess-1"), "未分组请求的亲和键必须原样不变");
    assert_ne!(plain, scoped, "同一会话键在两个组下必须落到不同的亲和条目");
    assert_eq!(scoped.as_deref(), Some("GECO\u{0}sess-1"));
    // 两个不同的组也必须互不干扰。
    assert_ne!(
        group_scoped_affinity_key(Some("G0"), None, Some("sess-1".into())),
        scoped,
        "不同分组的同名会话不得共用一个钉扎"
    );
    // 无会话键(无亲和记忆)时都是 None,不得凭空造出一个键。
    assert_eq!(group_scoped_affinity_key(Some("GECO"), None, None), None);
}

/// **跨客户串话的闸门。**
///
/// cursor 的 conversation_id 是纯内容派生的,而 `ConvRegistry` 在
/// (conversation_id, account) 双命中时会走服务端续写 —— 两个不同客户拿到同一个键,
/// 后来那位就会续在前一位的会话上。所以客户维度必须进命名空间。
#[test]
fn cursor_的亲和键按客户_key_再分一层() {
    assert!(real_provider("cursor").affinity_scoped_by_client());
    // 没有服务端续写的家族不分层:分了只会无谓打散主链路的缓存亲和。
    for f in ["kiro", "claude-dario", "claude-subprocess"] {
        assert!(!real_provider(f).affinity_scoped_by_client(), "{f} 不该按客户分层");
    }

    // 同组、同内容键、**不同客户** → 必须是两个不同的亲和条目。
    let a = group_scoped_affinity_key(Some("CUR"), Some("key-a"), Some("sess-1".into()));
    let b = group_scoped_affinity_key(Some("CUR"), Some("key-b"), Some("sess-1".into()));
    assert_ne!(a, b, "同一开场白的两个客户不得共用 conversation_id");
    // 同一个客户的同一会话必须稳定(否则每轮换 conversation_id,续写全废)。
    assert_eq!(
        a,
        group_scoped_affinity_key(Some("CUR"), Some("key-a"), Some("sess-1".into()))
    );
    // 客户 key 缺席(未分组 / 无 router 头)→ 退回旧行为,不凭空多加分隔符。
    assert_eq!(
        group_scoped_affinity_key(Some("CUR"), Some(""), Some("sess-1".into())).as_deref(),
        Some("CUR\u{0}sess-1")
    );
}

/// 保活帧必须**始终**用 `content_block_delta` 这个事件名。
///
/// 这是整个保活机制成立的前提,单独锁一个测试:下游 NewAPI 的 Claude→OpenAI 转换器
/// 只识别 message_start / content_block_{start,delta} / message_{delta,stop} 五种事件,
/// **且没有 default 兜底**。谁要是"顺手简化"成官方的 `ping`,保活帧会在 NewAPI 处
/// 静默消失、一个下游事件都不产生,客户端照样卡死——而且不会有任何报错提示你。
#[test]
fn keepalive_frames_use_an_event_newapi_can_convert() {
    for open in [
        OpenBlock::Text(0),
        OpenBlock::Thinking(3),
        OpenBlock::ToolUse(7),
    ] {
        let f = keepalive_frame(Some(open));
        assert_eq!(
            f.event, "content_block_delta",
            "{open:?} 的保活帧事件名必须是 content_block_delta,否则会被下游丢弃"
        );
    }
}

#[test]
fn keepalive_frame_matches_open_block_kind_and_is_a_noop() {
    // 帧形态必须贴合当前块类型:往 tool_use 块塞 text_delta 是非法 SSE,客户端会错乱。
    // 且承载的增量一律是空串 —— 客户端逐段拼接,拼空串等于没拼,对内容零影响。
    let cases = [
        (OpenBlock::Text(0), "text_delta", "text"),
        (OpenBlock::Thinking(3), "thinking_delta", "thinking"),
        (OpenBlock::ToolUse(7), "input_json_delta", "partial_json"),
    ];
    for (open, delta_type, field) in cases {
        let f = keepalive_frame(Some(open));
        let idx = match open {
            OpenBlock::Text(i) | OpenBlock::Thinking(i) | OpenBlock::ToolUse(i) => i,
        };
        assert_eq!(f.data["index"], idx, "保活帧必须打在当前块的索引上");
        assert_eq!(f.data["delta"]["type"], delta_type);
        // 字段必须**存在且为字符串**。NewAPI 的转换器对 input_json_delta 是
        // `*claudeResponse.Delta.PartialJson` 直接解引用、**不做 nil 检查**——
        // 缺字段或给 null 会让下游网关 panic,不是只丢一帧那么简单。
        assert_eq!(
            f.data["delta"][field].as_str(),
            Some(""),
            "{delta_type} 的 {field} 必须是空字符串(不能缺失、更不能是 null)"
        );
    }
}

#[test]
fn keepalive_without_open_block_falls_back_to_ping() {
    // 没有开着的块时零增量无处附着,只能退回官方 ping(对原生 Anthropic 客户端有效)。
    let f = keepalive_frame(None);
    assert_eq!(f.event, "ping");
    assert_eq!(f.data["type"], "ping");
}

#[test]
fn track_open_block_follows_block_lifecycle() {
    let start = |idx: u64, ty: &str| {
        SseEvent::new(
            "content_block_start",
            serde_json::json!({"type":"content_block_start","index":idx,
                "content_block":{"type":ty}}),
        )
    };
    let plain = |name: &str| SseEvent::new(name, serde_json::json!({"type":name}));

    assert_eq!(
        track_open_block(None, &start(0, "text")),
        Some(OpenBlock::Text(0))
    );
    assert_eq!(
        track_open_block(None, &start(1, "tool_use")),
        Some(OpenBlock::ToolUse(1))
    );
    assert_eq!(
        track_open_block(None, &start(2, "thinking")),
        Some(OpenBlock::Thinking(2))
    );
    // 没有安全零增量形态的块(如 redacted_thinking)→ 视为无块可附着,回退 ping。
    assert_eq!(track_open_block(None, &start(3, "redacted_thinking")), None);
    // 关块 / 整条消息结束都要清状态,否则会往已关闭的块发 delta(非法 SSE)。
    let open = Some(OpenBlock::ToolUse(1));
    assert_eq!(track_open_block(open, &plain("content_block_stop")), None);
    assert_eq!(track_open_block(open, &plain("message_stop")), None);
    // 无关事件不改变状态(delta 期间必须保持当前块)。
    assert_eq!(track_open_block(open, &plain("content_block_delta")), open);
    assert_eq!(track_open_block(open, &plain("message_delta")), open);
}

#[test]
fn idle_thresholds_leave_margin_under_the_observed_client_timeout() {
    // 2026-07-26 取证:四次卡死的静默时长 120.5/123.3/124.8/126.0 秒 → 客户端阈值约 120s。
    const OBSERVED_CLIENT_IDLE_TIMEOUT: std::time::Duration =
        std::time::Duration::from_secs(120);
    assert!(
        STREAM_IDLE_KEEPALIVE * 4 <= OBSERVED_CLIENT_IDLE_TIMEOUT,
        "保活间隔 {:?} 对客户端 {:?} 的阈值余量不足 4 倍,上游抖动稍变就又会被砍",
        STREAM_IDLE_KEEPALIVE,
        OBSERVED_CLIENT_IDLE_TIMEOUT
    );
    // 硬上限必须显著大于保活间隔,否则还没来得及保活就先中止了。
    assert!(
        STREAM_IDLE_ABORT >= STREAM_IDLE_KEEPALIVE * 4,
        "硬上限 {:?} 太接近保活间隔 {:?}",
        STREAM_IDLE_ABORT,
        STREAM_IDLE_KEEPALIVE
    );
    // 上限也要大于观察到的客户端阈值,否则"上游只是慢"的情形永远等不到恢复,
    // 也就永远回答不了"慢还是死"。
    assert!(
        STREAM_IDLE_ABORT > OBSERVED_CLIENT_IDLE_TIMEOUT,
        "硬上限不该早于客户端自己的阈值,那样这次改动等于没做"
    );
}

// ───── 审查复审 [高] 回归:profileArn 修复验证被 RPM 拦不得误杀账号 ─────

/// mock provider:chat 恒 403 TokenInvalid(记录真实发出的调用次数),
/// refresh 成功(换新 at),强制发现 profileArn 成功。
struct HealGateMockProvider {
    chat_calls: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl Provider for HealGateMockProvider {
    fn family(&self) -> &'static str {
        "kiro"
    }
    fn account_schema(&self) -> &'static [gw_core::account::FieldSpec] {
        &[]
    }
    async fn list_models(
        &self,
    ) -> Result<Vec<gw_core::model::ModelInfo>, UpstreamError> {
        Ok(vec![])
    }
    async fn chat(
        &self,
        _req: ChatRequest,
        _ctx: &CallCtx,
    ) -> Result<gw_core::provider::ChatStream, UpstreamError> {
        self.chat_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(UpstreamError::new(UpstreamErrorKind::TokenInvalid, "mock 403"))
    }
    async fn refresh_auth(&self, account: &Account) -> Result<Account, UpstreamError> {
        let mut a = account.clone();
        a.extra.insert("access_token".into(), serde_json::json!("at-new"));
        a.extra
            .insert("expires_at".into(), serde_json::json!("2099-01-01T00:00:00Z"));
        Ok(a)
    }
    async fn force_discover_profile_arn(
        &self,
        _account: &Account,
    ) -> Result<Option<String>, UpstreamError> {
        Ok(Some(
            "arn:aws:codewhisperer:us-east-1:123456789012:profile/TESTPROFILE".into(),
        ))
    }
}

/// profileArn 发现成功、但修复后的验证调用被 RPM 闸门拦住(暖机一期 RPM=2 的
/// 确定性路径:首发占 1 格 → 刷新重试占第 2 格 → 验证调用达限):
/// **不得**把修复前的 e2 按 TokenInvalid 上报 —— 那会把刚修好的号永久误禁
/// (invalid_refresh_token)。被拦 ≠ 修复失败:换号/透错,不上报、不禁用。
#[tokio::test]
async fn heal_retry_blocked_by_rpm_does_not_report_or_disable_account() {
    let mut extra = BTreeMap::new();
    extra.insert("access_token".into(), serde_json::json!("at-old"));
    extra.insert("refresh_token".into(), serde_json::json!("rt-1"));
    extra.insert("expires_at".into(), serde_json::json!("2099-01-01T00:00:00Z"));
    // 付费非 FREE + 无 profile_arn → needs_profile_discovery 成立,走强制发现。
    extra.insert("subscription_title".into(), serde_json::json!("KIRO PRO"));
    // 有效 RPM = 2:首发预留 + 刷新重试各占一格,验证调用必被拦。
    extra.insert("rpm_limit".into(), serde_json::json!(2));
    let acc = Arc::new(Account {
        account_id: "a".into(),
        provider: "kiro".into(),
        max_concurrency: 2,
        disabled: false,
        created_at: 0,
        extra,
    });
    let chat_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let provider: Arc<dyn Provider> =
        Arc::new(HealGateMockProvider { chat_calls: chat_calls.clone() });
    let st = Arc::new(WorkerState {
        instance: 0,
        egress_desc: String::new(),
        group: String::new(),
        provider,
        settings_sync: parking_lot::RwLock::new(SettingsSync::default()),
        scheduler: AccountScheduler::new(
            vec![acc],
            &gw_core::config::SchedulerConfig::default(),
        ),
        refresh_locks: parking_lot::Mutex::new(std::collections::HashMap::new()),
        rejection_refresh_at: parking_lot::Mutex::new(std::collections::HashMap::new()),
        usage_sink: None,
        pending_writes: PendingWrites::new(),
        store: None,
        quota_cache: parking_lot::Mutex::new(std::collections::HashMap::new()),
        quota_inflight: parking_lot::Mutex::new(std::collections::HashSet::new()),
        quota_sem: Arc::new(tokio::sync::Semaphore::new(1)),
        sync_lock: tokio::sync::Mutex::new(()),
        group_views: parking_lot::RwLock::new(std::collections::HashMap::new()),
        _client: reqwest::Client::new(),
    });
    let body = serde_json::json!({
        "model": "claude-opus-5",
        "max_tokens": 16,
        "stream": true,
        "messages": [{"role": "user", "content": "hi"}],
    });
    let resp = messages(State(st.clone()), HeaderMap::new(), Json(body)).await;
    // 单号池:attempts(1) 已触 switch_cap → 透原始 403 的对外映射(502)。
    // 关键不是状态码,而是下面两条:没发第三次调用、号没被上报禁用。
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        chat_calls.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "首发 + 刷新重试 = 2 次真实调用;被拦的验证调用绝不能发出"
    );
    let snap = st.scheduler.status_snapshot();
    let a = snap.iter().find(|x| x.account_id == "a").unwrap();
    assert!(!a.disabled, "被 RPM 拦 ≠ 修复失败,账号不得被禁用");
    assert_eq!(a.reason, "", "不得按 TokenInvalid 上报(invalid_refresh_token)");
    assert_eq!(a.rpm_used, 2, "被拦的验证调用不得记账");
}

/// 流内 401 测试桩:第一次 chat 产「首项即 Err(TokenInvalid)」的流(cursor 系
/// 真实形态 —— 401 以流内错误帧到达),第二次起产正常文本流;refresh_auth 按
/// refresh_ok 决定换新 token 还是 invalid_grant。
struct Instream401Provider {
    chat_calls: Arc<std::sync::atomic::AtomicUsize>,
    refresh_ok: bool,
}

struct DeliveryProvider {
    calls: Arc<std::sync::atomic::AtomicUsize>,
    refreshes: Arc<std::sync::atomic::AtomicUsize>,
    delivery: gw_core::error::RequestDelivery,
    kind: UpstreamErrorKind,
    in_stream: bool,
}

#[tokio::test]
async fn missing_openai_marker_is_only_set_by_rejection_endpoint() {
    use tower::ServiceExt;
    let app = Router::new()
        .route("/v1/chat/completions", post(unsupported_openai_endpoint))
        .route("/v1/responses", post(unsupported_openai_endpoint));
    for path in ["/v1/chat/completions", "/v1/responses", "/unknown"] {
        let response = app.clone().oneshot(axum::http::Request::builder().method("POST")
            .uri(path).body(axum::body::Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(response.headers().get(crate::ENDPOINT_UNAVAILABLE_HEADER).is_some(), path != "/unknown");
    }
}

#[async_trait::async_trait]
impl Provider for DeliveryProvider {
    fn family(&self) -> &'static str { "cursor" }
    fn account_schema(&self) -> &'static [gw_core::account::FieldSpec] { &[] }
    async fn list_models(&self) -> Result<Vec<gw_core::model::ModelInfo>, UpstreamError> { Ok(vec![]) }
    async fn chat(&self, _: ChatRequest, _: &CallCtx) -> Result<gw_core::provider::ChatStream, UpstreamError> {
        if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            let e = UpstreamError::new(self.kind, "提交确认丢失").with_delivery(self.delivery);
            if self.in_stream { Ok(chat_stream(vec![Err(e)])) } else { Err(e) }
        } else { Ok(text_reply_stream()) }
    }
    async fn refresh_auth(&self, account: &Account) -> Result<Account, UpstreamError> {
        self.refreshes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut account = account.clone();
        account.extra.insert("access_token".into(), serde_json::json!("at-new"));
        Ok(account)
    }
}

#[tokio::test]
async fn delivery_unknown_stops_switch_and_overload_backoff() {
    use gw_core::error::RequestDelivery as D;
    for delivery in [D::Unknown, D::Submitted, D::NotSubmitted] {
        for kind in [UpstreamErrorKind::ServerError, UpstreamErrorKind::Overloaded, UpstreamErrorKind::TokenInvalid] {
            for stream in [true, false] {
              for in_stream in [true, false] {
                let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let refreshes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let provider = Arc::new(DeliveryProvider { calls: calls.clone(), refreshes: refreshes.clone(), delivery, kind, in_stream });
                let (state, account) = instream_401_state(provider);
                let mut b = (*account).clone(); b.account_id = "b".into();
                state.scheduler.sync_accounts(vec![account, Arc::new(b)]);
                let response = tokio::time::timeout(std::time::Duration::from_secs(5), handle_chat(
                    state.clone(), HeaderMap::new(), serde_json::json!({"model":"m","stream":stream,
                        "max_tokens":10,"messages":[{"role":"user","content":"probe"}]}), Wire::Anthropic,
                )).await.unwrap();
                let status = response.status();
                let _ = axum::body::to_bytes(response.into_body(), 100_000).await.unwrap();
                let may_retry = delivery == D::NotSubmitted && (!in_stream || kind == UpstreamErrorKind::TokenInvalid);
                let expected = if may_retry { 2 } else { 1 };
                assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), expected, "{delivery:?} {kind:?}");
                assert_eq!(refreshes.load(std::sync::atomic::Ordering::SeqCst), usize::from(may_retry && kind == UpstreamErrorKind::TokenInvalid));
                assert_eq!(status == StatusCode::OK, may_retry);
                if kind == UpstreamErrorKind::TokenInvalid && delivery.may_have_executed() {
                    assert!(state.scheduler.status_snapshot().iter().all(|a| !a.disabled),
                        "提交后的续收鉴权失败不能证明 refresh token 永久失效");
                }
              }
            }
        }
    }
}

#[async_trait::async_trait]
impl Provider for Instream401Provider {
    fn family(&self) -> &'static str {
        "cursor"
    }
    fn account_schema(&self) -> &'static [gw_core::account::FieldSpec] {
        &[]
    }
    async fn list_models(&self) -> Result<Vec<gw_core::model::ModelInfo>, UpstreamError> {
        Ok(vec![])
    }
    async fn chat(
        &self,
        _req: ChatRequest,
        _ctx: &CallCtx,
    ) -> Result<gw_core::provider::ChatStream, UpstreamError> {
        let n = self
            .chat_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if n == 0 {
            Ok(chat_stream(vec![Err(UpstreamError::new(
                UpstreamErrorKind::TokenInvalid,
                "in-stream unauthenticated",
            ))]))
        } else {
            Ok(text_reply_stream())
        }
    }
    async fn refresh_auth(&self, account: &Account) -> Result<Account, UpstreamError> {
        if !self.refresh_ok {
            return Err(UpstreamError::new(UpstreamErrorKind::TokenInvalid, "invalid_grant"));
        }
        let mut a = account.clone();
        a.extra
            .insert("access_token".into(), serde_json::json!("at-new"));
        a.extra
            .insert("expires_at".into(), serde_json::json!("2099-01-01T00:00:00Z"));
        Ok(a)
    }
}

/// 流内 401 测试夹具:账号带 at-old/rt-1 + 宽裕 RPM,WorkerState 挂指定 provider。
fn instream_401_state(
    provider: Arc<dyn Provider>,
) -> (Arc<WorkerState>, Arc<Account>) {
    let mut extra = BTreeMap::new();
    extra.insert("access_token".into(), serde_json::json!("at-old"));
    extra.insert("refresh_token".into(), serde_json::json!("rt-1"));
    extra.insert("expires_at".into(), serde_json::json!("2099-01-01T00:00:00Z"));
    extra.insert("rpm_limit".into(), serde_json::json!(100));
    let acc = Arc::new(Account {
        account_id: "a".into(),
        provider: "cursor".into(),
        max_concurrency: 1,
        disabled: false,
        created_at: 0,
        extra,
    });
    let st = Arc::new(WorkerState {
        instance: 0,
        egress_desc: String::new(),
        group: String::new(),
        provider,
        settings_sync: parking_lot::RwLock::new(SettingsSync::default()),
        scheduler: AccountScheduler::new(
            vec![acc.clone()],
            &gw_core::config::SchedulerConfig::default(),
        ),
        refresh_locks: parking_lot::Mutex::new(std::collections::HashMap::new()),
        rejection_refresh_at: parking_lot::Mutex::new(std::collections::HashMap::new()),
        usage_sink: None,
        pending_writes: PendingWrites::new(),
        store: None,
        quota_cache: parking_lot::Mutex::new(std::collections::HashMap::new()),
        quota_inflight: parking_lot::Mutex::new(std::collections::HashSet::new()),
        quota_sem: Arc::new(tokio::sync::Semaphore::new(1)),
        sync_lock: tokio::sync::Mutex::new(()),
        group_views: parking_lot::RwLock::new(std::collections::HashMap::new()),
        _client: reqwest::Client::new(),
    });
    (st, acc)
}

/// 2026-09-08 ultra-test 误杀事故回归:cursor 系 401 以流内错误帧到达,必须先
/// 同号刷新重试;RT 活着时一次流内 401 绝不得把号永久禁掉。
#[tokio::test]
async fn instream_token_invalid_refreshes_and_retries_instead_of_disabling() {
    let chat_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let provider: Arc<dyn Provider> = Arc::new(Instream401Provider {
        chat_calls: chat_calls.clone(),
        refresh_ok: true,
    });
    let (st, acc) = instream_401_state(provider);
    let ctx = CallCtx {
        account: acc,
        session_id: "s".into(),
        cache_key: "s".into(),
    };
    let first = st.provider.chat(req_model_m(), &ctx).await.unwrap();
    let lease = st.scheduler.acquire(Some("s")).await.unwrap();
    let resp = stream_response(
        st.clone(),
        lease,
        first,
        req_model_m(),
        String::new(),
        ctx,
        std::time::Instant::now(),
        Wire::Anthropic,
        false,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "刷新重试应透明救回请求");
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let body = String::from_utf8_lossy(&bytes).to_string();
    assert!(body.contains("hi"), "重试后的正常内容应送达客户端");
    assert_eq!(
        chat_calls.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "首发 + 刷新后重试 = 2 次上游调用"
    );
    let snap = st.scheduler.status_snapshot();
    let a = snap.iter().find(|x| x.account_id == "a").unwrap();
    assert!(!a.disabled, "RT 活着时流内 401 不得禁号");
}

/// 同一事故的另一面:刷新也失败(RT 真死)才永久禁号 —— 不能反过来变成
/// 永远不敢禁用(那样真死号会一直在池里吃请求)。
#[tokio::test]
async fn instream_token_invalid_still_disables_when_refresh_fails() {
    let chat_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let provider: Arc<dyn Provider> = Arc::new(Instream401Provider {
        chat_calls: chat_calls.clone(),
        refresh_ok: false,
    });
    let (st, acc) = instream_401_state(provider);
    let ctx = CallCtx {
        account: acc,
        session_id: "s".into(),
        cache_key: "s".into(),
    };
    let first = st.provider.chat(req_model_m(), &ctx).await.unwrap();
    let lease = st.scheduler.acquire(Some("s")).await.unwrap();
    let resp = stream_response(
        st.clone(),
        lease,
        first,
        req_model_m(),
        String::new(),
        ctx,
        std::time::Instant::now(),
        Wire::Anthropic,
        false,
    )
    .await;
    assert_ne!(resp.status(), StatusCode::OK);
    assert_eq!(
        chat_calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "刷新失败不重试,不放大上游调用"
    );
    let snap = st.scheduler.status_snapshot();
    let a = snap.iter().find(|x| x.account_id == "a").unwrap();
    assert!(a.disabled, "刷新失败 = RT 真死,必须永久禁用");
    assert_eq!(a.reason, "invalid_refresh_token");
}

/// upstream_cut(静默掐流前兆)收尾纪律:只喂软冷却 + 请求日志 error_kind,
/// 客户端 finale 原样送达,健康/禁用体系零变化(2026-07-25 事故 + 评审#2)。
#[tokio::test]
async fn upstream_cut_stream_feeds_soft_drain_only() {
    let chat_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let provider: Arc<dyn Provider> = Arc::new(HealGateMockProvider { chat_calls });
    let st = Arc::new(WorkerState {
        instance: 0,
        egress_desc: String::new(),
        group: String::new(),
        provider,
        settings_sync: parking_lot::RwLock::new(SettingsSync::default()),
        scheduler: AccountScheduler::new(
            vec![Arc::new(acct(&[]))],
            &gw_core::config::SchedulerConfig::default(),
        ),
        refresh_locks: parking_lot::Mutex::new(std::collections::HashMap::new()),
        rejection_refresh_at: parking_lot::Mutex::new(std::collections::HashMap::new()),
        usage_sink: None,
        pending_writes: PendingWrites::new(),
        store: None,
        quota_cache: parking_lot::Mutex::new(std::collections::HashMap::new()),
        quota_inflight: parking_lot::Mutex::new(std::collections::HashSet::new()),
        quota_sem: Arc::new(tokio::sync::Semaphore::new(1)),
        sync_lock: tokio::sync::Mutex::new(()),
        group_views: parking_lot::RwLock::new(std::collections::HashMap::new()),
        _client: reqwest::Client::new(),
    });
    // 模拟 Kiro 掐流:payload 已出,provider 报 UpstreamCut 后仍合成完整 finale。
    let cut_stream = || {
        chat_stream(vec![
            Ok(StreamItem::Sse(SseEvent::new(
                "message_start",
                serde_json::json!({"message":{"id":"m","content":[]}}),
            ))),
            Ok(StreamItem::Sse(SseEvent::new(
                "content_block_delta",
                serde_json::json!({"index":0,"delta":{"type":"text_delta","text":"半截"}}),
            ))),
            Ok(StreamItem::UpstreamCut),
            Ok(StreamItem::Sse(SseEvent::new(
                "message_delta",
                serde_json::json!({"delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),
            ))),
            Ok(StreamItem::Sse(SseEvent::new("message_stop", serde_json::json!({})))),
            Ok(StreamItem::Usage(ChatUsage {
                input_tokens: 4,
                output_tokens: 2,
                ..Default::default()
            })),
        ])
    };
    // 第一次掐流:未达阈值(默认 2),不进 draining;客户端仍收到完整 finale。
    let lease = st.scheduler.acquire(Some("s")).await.unwrap();
    let resp = stream_response(
        st.clone(),
        lease,
        cut_stream(),
        req_model_m(),
        String::new(),
        test_ctx(&[]),
        std::time::Instant::now(),
        Wire::Anthropic,
    false,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let body = String::from_utf8_lossy(&bytes).to_string();
    assert!(body.contains("message_stop"), "合成 finale 应照常转发客户端");
    assert!(!body.contains("upstream_cut"), "UpstreamCut 绝不泄漏给客户端");
    assert!(!st.scheduler.is_draining("a"), "单次 cut 未达阈值,不进 draining");
    let snap = st.scheduler.status_snapshot();
    let a = snap.iter().find(|x| x.account_id == "a").unwrap();
    assert!(!a.disabled, "掐流绝不进禁用体系");
    // 第二次掐流:窗口内达阈值 → draining;但单号池 fail-open 仍能拿到 lease。
    let lease = st.scheduler.acquire(Some("s")).await.unwrap();
    let resp = stream_response(
        st.clone(),
        lease,
        cut_stream(),
        req_model_m(),
        String::new(),
        test_ctx(&[]),
        std::time::Instant::now(),
        Wire::Anthropic,
    false,
    )
    .await;
    let _ = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    assert!(st.scheduler.is_draining("a"), "窗口内两次 cut 应进软冷却");
    let snap = st.scheduler.status_snapshot();
    let a = snap.iter().find(|x| x.account_id == "a").unwrap();
    assert!(!a.disabled, "draining 不等于禁用,健康面板不受影响");
    let lease = st.scheduler.acquire(None).await.unwrap();
    assert_eq!(lease.account_id(), "a", "normal 全空时 fail-open:draining 号仍服务");
}

// ───────────────────────── OpenAI 线缆(cursor 专属)─────────────────────────

#[test]
fn openai_线缆只挂给_cursor_家族() {
    assert!(real_provider("cursor").openai_wire());
    // kiro / ccmax 明确不做:它们的主链路全程 Anthropic、零转换,那是刻意保住的资产。
    for f in ["kiro", "claude-dario", "claude-subprocess"] {
        assert!(!real_provider(f).openai_wire(), "{f} 不该挂 OpenAI 入口");
    }
}

/// 只为 `/v1/models` 形状测试存在的 provider:家族名可控 + 目录里真有一项。
struct CatalogMockProvider(&'static str);

#[async_trait::async_trait]
impl Provider for CatalogMockProvider {
    fn family(&self) -> &'static str {
        self.0
    }
    // 与真实实现同口径:只有 cursor 挂 OpenAI 入口(见 CursorProvider::openai_wire)。
    fn openai_wire(&self) -> bool {
        self.0 == "cursor"
    }
    // 纯 bot 池裁剪:与 CursorProvider 共用同一份实现。
    fn models_for_accounts(
        &self,
        list: Vec<gw_core::model::ModelInfo>,
        accounts: &[Arc<Account>],
    ) -> Vec<gw_core::model::ModelInfo> {
        if self.0 == "cursor" { gw_cursor::models_for_pool(list, accounts) } else { list }
    }
    fn account_schema(&self) -> &'static [gw_core::account::FieldSpec] {
        &[]
    }
    async fn list_models(&self) -> Result<Vec<gw_core::model::ModelInfo>, UpstreamError> {
        Ok(vec![gw_core::model::ModelInfo {
            display_name: Some("Grok".into()),
            ..gw_core::model::ModelInfo::new("grok-4.5")
        }])
    }
    async fn chat(
        &self,
        _req: ChatRequest,
        _ctx: &CallCtx,
    ) -> Result<gw_core::provider::ChatStream, UpstreamError> {
        Err(UpstreamError::new(UpstreamErrorKind::Other, "未用到"))
    }
    async fn refresh_auth(&self, a: &Account) -> Result<Account, UpstreamError> {
        Ok(a.clone())
    }
}

fn catalog_state(family: &'static str) -> Arc<WorkerState> {
    catalog_state_with_accounts(family, vec![Arc::new(acct(&[]))])
}

fn catalog_state_with_accounts(
    family: &'static str,
    accounts: Vec<Arc<Account>>,
) -> Arc<WorkerState> {
    let provider: Arc<dyn Provider> = Arc::new(CatalogMockProvider(family));
    Arc::new(WorkerState {
        instance: 0,
        egress_desc: String::new(),
        group: String::new(),
        provider,
        settings_sync: parking_lot::RwLock::new(SettingsSync::default()),
        scheduler: AccountScheduler::new(
            accounts,
            &gw_core::config::SchedulerConfig::default(),
        ),
        refresh_locks: parking_lot::Mutex::new(std::collections::HashMap::new()),
        rejection_refresh_at: parking_lot::Mutex::new(std::collections::HashMap::new()),
        usage_sink: None,
        pending_writes: PendingWrites::new(),
        store: None,
        quota_cache: parking_lot::Mutex::new(std::collections::HashMap::new()),
        quota_inflight: parking_lot::Mutex::new(std::collections::HashSet::new()),
        quota_sem: Arc::new(tokio::sync::Semaphore::new(1)),
        sync_lock: tokio::sync::Mutex::new(()),
        group_views: parking_lot::RwLock::new(std::collections::HashMap::new()),
        _client: reqwest::Client::new(),
    })
}

/// 带真实库 + 非空归属组的 worker,用于验证配额回填不改成员边。
fn tier_state(store: Arc<gw_store::SqliteStore>, accounts: Vec<Arc<Account>>) -> Arc<WorkerState> {
    let mut st = catalog_state_with_accounts("kiro", accounts);
    let inner = Arc::get_mut(&mut st).expect("刚建出来,独占");
    inner.group = "G0".into();
    inner.store = Some(store);
    st
}

/// 2026-10-28:配额回填只写档位,**绝不改组内优先级**。覆盖旧逻辑会改的全部方向:
/// 个人 / 企业 POWER 在低层不得被抬高、非 POWER 在高层不得被压低、
/// 被撤的边不得重建。
#[tokio::test]
async fn backfill_never_touches_member_priority() {
    let store = Arc::new(gw_store::SqliteStore::open_in_memory().unwrap());
    store.create_group("G0", "", "").unwrap();
    // 身份与档位同时进库和 worker 内存:按号型分支的归位(哪怕只升企业号)也必须被抓到。
    let cases = [
        ("s1", "google", 100, "KIRO POWER"),
        ("e1", "enterprise", 100, "KIRO POWER"),
        ("m1", "google", 0, "KIRO PRO MAX"),
        ("f1", "", 7, "KIRO FREE"),
    ];
    let mut accounts = Vec::new();
    for (id, idp, pri, title) in cases {
        let mut kv = vec![("subscription_title", title)];
        if !idp.is_empty() {
            kv.push(("kiro_provider", idp));
        }
        let extra: serde_json::Map<String, serde_json::Value> =
            kv.iter().map(|(k, v)| (k.to_string(), serde_json::json!(v))).collect();
        store
            .create_account(id, "G0", "kiro", 3, &serde_json::Value::Object(extra).to_string())
            .unwrap();
        store.upsert_membership(id, "G0", pri).unwrap();
        let mut a = acct(&kv);
        a.account_id = id.into();
        accounts.push(Arc::new(a));
    }
    store.create_account("k1", "G0", "kiro", 3, "{}").unwrap();
    store.remove_membership("k1", "G0").unwrap();
    let mut k = acct(&[]);
    k.account_id = "k1".into();
    accounts.push(Arc::new(k));
    let st = tier_state(store.clone(), accounts);

    // 两轮:档位与库里已有值相同的「未变化」回填同样不得动边。
    for _ in 0..2 {
        for (id, _, _, title) in cases {
            st.backfill_subscription_title(id, title).await;
        }
        st.backfill_subscription_title("k1", "KIRO POWER").await;
    }

    let mut got = store.list_group_members("G0").unwrap();
    got.sort();
    let mut want: Vec<(String, i64)> =
        cases.iter().map(|(id, _, pri, _)| (id.to_string(), *pri)).collect();
    want.sort();
    assert_eq!(got, want, "回填不得改任何成员边,也不得重建被撤的边");
}

/// `/v1/models` 是**共享**端点:kiro / dario 也走它。OpenAI 字段只能加在开了
/// OpenAI 入口的 worker 上,否则就是为迁就一种协议去改另一种协议的共享响应
/// (按 schema 严校验的客户端、对响应体做快照的代理都会看到不兼容变化)。
#[tokio::test]
async fn models_端点的_openai_字段只出现在_cursor_worker() {
    let kiro = body_json(models(State(catalog_state("kiro"))).await).await;
    assert!(kiro.get("object").is_none(), "非 cursor 家族不该出现 object:list");
    let item = &kiro["data"][0];
    assert_eq!(item["type"], "model", "Anthropic 字段必须还在");
    assert_eq!(item["id"], "grok-4.5");
    assert!(item.get("created_at").is_some());
    for k in ["object", "created", "owned_by"] {
        assert!(item.get(k).is_none(), "非 cursor 家族不该出现 {k}");
    }

    let cursor = body_json(models(State(catalog_state("cursor"))).await).await;
    assert_eq!(cursor["object"], "list", "NewAPI 的获取模型列表认这个键");
    let item = &cursor["data"][0];
    assert_eq!(item["object"], "model");
    assert_eq!(item["owned_by"], "cursor");
    assert!(item["created"].is_i64());
    // 两套字段并存:Anthropic 侧一个都不能少。
    assert_eq!(item["type"], "model");
    assert_eq!(item["display_name"], "Grok");
}

/// 纯 bot 池(grokbot 0.39 面无模型字段)的 `/v1/models` 只留 grok_bot_auto
/// 一项;混有任何非 bot 号、无 pool 钉、非 cursor 家族 → 维持 provider 全量目录。
#[tokio::test]
async fn models_端点_纯bot池_只出单模型() {
    let bot_a = |id: &str| {
        let mut a = acct(&[("pool", "bot")]);
        a.account_id = id.into();
        Arc::new(a)
    };
    // 纯 bot 池:所有账号钉 pool=bot → 单项目录。
    let st = catalog_state_with_accounts("cursor", vec![bot_a("a1"), bot_a("a2")]);
    let body = body_json(models(State(st)).await).await;
    let data = body["data"].as_array().unwrap();
    assert_eq!(data.len(), 1, "纯 bot 池只能看到 grok_bot_auto: {body}");
    assert_eq!(data[0]["id"], gw_cursor::bot_pool_list()[0].id);
    assert_eq!(data[0]["owned_by"], "cursor");

    // 混有一个非 bot 号 → 维持全量(provider 目录原样)。
    let mut api = acct(&[("pool", "api")]);
    api.account_id = "b".into();
    let st = catalog_state_with_accounts("cursor", vec![bot_a("a1"), Arc::new(api)]);
    let body = body_json(models(State(st)).await).await;
    assert_eq!(body["data"].as_array().unwrap().len(), 1, "mock 目录只有一项,不裁时应原样返回");
    assert_eq!(body["data"][0]["id"], "grok-4.5");

    // 无 pool 钉(未钉号)→ 不裁。
    let st = catalog_state_with_accounts("cursor", vec![Arc::new(acct(&[]))]);
    let body = body_json(models(State(st)).await).await;
    assert_eq!(body["data"][0]["id"], "grok-4.5");

    // 非 cursor 家族即使账号钉了 pool=bot 也不裁(kiro 不该认识 pool 语义)。
    let st = catalog_state_with_accounts("kiro", vec![bot_a("a1")]);
    let body = body_json(models(State(st)).await).await;
    assert_eq!(body["data"][0]["id"], "grok-4.5");
}

/// 一段典型的 Anthropic 回复流:文本 + 用量 + 正常收尾。
fn text_reply_stream() -> gw_core::provider::ChatStream {
    chat_stream(vec![
        Ok(StreamItem::Sse(SseEvent::new(
            "message_start",
            serde_json::json!({"message":{"model":"grok-4.5","usage":{"input_tokens":7}}}),
        ))),
        Ok(StreamItem::Sse(SseEvent::new(
            "content_block_start",
            serde_json::json!({"index":0,"content_block":{"type":"text","text":""}}),
        ))),
        Ok(StreamItem::Sse(SseEvent::new(
            "content_block_delta",
            serde_json::json!({"index":0,"delta":{"type":"text_delta","text":"hi"}}),
        ))),
        Ok(StreamItem::Sse(SseEvent::new(
            "content_block_stop",
            serde_json::json!({"index":0}),
        ))),
        Ok(StreamItem::Sse(SseEvent::new(
            "message_delta",
            serde_json::json!({"delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),
        ))),
        Ok(StreamItem::Sse(SseEvent::new("message_stop", serde_json::json!({})))),
    ])
}

fn wire_state() -> Arc<WorkerState> {
    let provider: Arc<dyn Provider> = Arc::new(HealGateMockProvider {
        chat_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    });
    Arc::new(WorkerState {
        instance: 0,
        egress_desc: String::new(),
        group: String::new(),
        provider,
        settings_sync: parking_lot::RwLock::new(SettingsSync::default()),
        scheduler: AccountScheduler::new(
            vec![Arc::new(acct(&[]))],
            &gw_core::config::SchedulerConfig::default(),
        ),
        refresh_locks: parking_lot::Mutex::new(std::collections::HashMap::new()),
        rejection_refresh_at: parking_lot::Mutex::new(std::collections::HashMap::new()),
        usage_sink: None,
        pending_writes: PendingWrites::new(),
        store: None,
        quota_cache: parking_lot::Mutex::new(std::collections::HashMap::new()),
        quota_inflight: parking_lot::Mutex::new(std::collections::HashSet::new()),
        quota_sem: Arc::new(tokio::sync::Semaphore::new(1)),
        sync_lock: tokio::sync::Mutex::new(()),
        group_views: parking_lot::RwLock::new(std::collections::HashMap::new()),
        _client: reqwest::Client::new(),
    })
}

async fn sse_body(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    String::from_utf8_lossy(&bytes).to_string()
}

#[tokio::test]
async fn chat_线缆的流式响应以_done_收尾_用量帧在它之前() {
    let st = wire_state();
    let lease = st.scheduler.acquire(Some("s")).await.unwrap();
    let resp = stream_response(
        st.clone(),
        lease,
        text_reply_stream(),
        req_model_m(),
        String::new(),
        test_ctx(&[]),
        std::time::Instant::now(),
        Wire::OpenAiChat { include_usage: true },
    false,
    )
    .await;
    let body = sse_body(resp).await;
    // ChatCompletions 不写 event 行 —— 写了会让只认 data: 的解析器丢帧。
    assert!(!body.contains("event: "), "chat 线缆不该有 event 行:\n{body}");
    assert!(body.contains("chat.completion.chunk"));
    assert!(body.contains("\"content\":\"hi\""));
    let done = body.rfind("data: [DONE]").expect("必须以 [DONE] 收尾");
    let usage = body.rfind("\"usage\"").expect("include_usage=true 必须发用量帧");
    // 顺序反了 NewAPI 读到终止哨兵就收工,这次请求在它那边记 0 用量。
    assert!(usage < done, "用量帧必须在 [DONE] 之前:\n{body}");
    assert!(body.contains("\"prompt_tokens\":7"));
    assert!(body.contains("\"completion_tokens\":2"));
    // Anthropic 的事件名一个都不该漏出去。
    assert!(!body.contains("message_stop"));
    assert!(!body.contains("content_block_delta"));
}

#[tokio::test]
async fn responses_线缆的流式响应以_response_completed_收尾() {
    let st = wire_state();
    let lease = st.scheduler.acquire(Some("s")).await.unwrap();
    let resp = stream_response(
        st.clone(),
        lease,
        text_reply_stream(),
        req_model_m(),
        String::new(),
        test_ctx(&[]),
        std::time::Instant::now(),
        Wire::OpenAiResponses,
    false,
    )
    .await;
    let body = sse_body(resp).await;
    // Responses 每帧都必须带 event 名,客户端按它分发。
    assert!(body.contains("event: response.created"));
    assert!(body.contains("event: response.output_text.delta"));
    assert!(body.contains("event: response.completed"));
    assert!(!body.contains("[DONE]"), "Responses 没有 [DONE] 这个概念");
    assert!(!body.contains("message_stop"));
}

/// 首项窥探(2026-08-24 用户报障):流首项即上游错误 → 直接回真实错误状态+文案,
/// 不再是 200 空流(Claude Code 只会说 "check any proxy")。
#[tokio::test]
async fn 首项即错误_窥探后回真实错误状态() {
    let st = wire_state();
    let lease = st.scheduler.acquire(Some("s")).await.unwrap();
    let resp = stream_response(
        st.clone(),
        lease,
        chat_stream(vec![Err(UpstreamError::new(
            UpstreamErrorKind::EmptyResponse,
            "上游空响应(零内容产出)".to_string(),
        ))]),
        req_model_m(),
        String::new(),
        test_ctx(&[]),
        std::time::Instant::now(),
        Wire::Anthropic,
    false,
    )
    .await;
    // EmptyResponse → 400(止住对同一被拒内容的重试风暴),文案含可操作提示。
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = sse_body(resp).await;
    assert!(body.contains("invalid_request_error"), "{body}");
    assert!(body.contains("安全审核"), "文案必须点出疑 guardrail: {body}");
    assert!(!body.contains("零内容产出"), "内部原文不外泄: {body}");
}

/// 零项流(上游零事件结束)→ 按 EmptyResponse 回显 400,而非 200 空 SSE。
#[tokio::test]
async fn 零项流_窥探后按空响应回显() {
    let st = wire_state();
    let lease = st.scheduler.acquire(Some("s")).await.unwrap();
    let resp = stream_response(
        st.clone(),
        lease,
        chat_stream(vec![]),
        req_model_m(),
        String::new(),
        test_ctx(&[]),
        std::time::Instant::now(),
        Wire::Anthropic,
    false,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = sse_body(resp).await;
    assert!(body.contains("invalid_request_error"), "{body}");
}

/// 前导帧不证明有内容:kiro 空响应的真实形态是 message_start 急发**之后**才 Err
/// (gw-kiro chat.rs:885/1175)——窥探必须压过前导帧,不能被 message_start 骗提交
/// (2026-08-24 codex 审查 blocker)。
#[tokio::test]
async fn 前导message_start后错误_仍按首包前失败回显() {
    let st = wire_state();
    let lease = st.scheduler.acquire(Some("s")).await.unwrap();
    let resp = stream_response(
        st.clone(),
        lease,
        chat_stream(vec![
            Ok(StreamItem::Sse(SseEvent::new(
                "message_start",
                serde_json::json!({"message":{"model":"grok-4.5","usage":{"input_tokens":7}}}),
            ))),
            Err(UpstreamError::new(
                UpstreamErrorKind::EmptyResponse,
                "上游空响应(零内容产出)".to_string(),
            )),
        ]),
        req_model_m(),
        String::new(),
        test_ctx(&[]),
        std::time::Instant::now(),
        Wire::Anthropic,
    false,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = sse_body(resp).await;
    assert!(body.contains("invalid_request_error"), "{body}");
    assert!(body.contains("安全审核"), "{body}");
}

/// Usage 也是前导(dario 零 SSE 流会先产内部 Usage,gw-dario chat.rs:536):
/// [Usage, 流尽] → 按 EmptyResponse 400。
#[tokio::test]
async fn 前导usage后流尽_按空响应回显() {
    let st = wire_state();
    let lease = st.scheduler.acquire(Some("s")).await.unwrap();
    let resp = stream_response(
        st.clone(),
        lease,
        chat_stream(vec![Ok(StreamItem::Usage(ChatUsage {
            input_tokens: 7,
            output_tokens: 0,
            ..Default::default()
        }))]),
        req_model_m(),
        String::new(),
        test_ctx(&[]),
        std::time::Instant::now(),
        Wire::Anthropic,
    false,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// 正常流:message_start/Usage 只是前导,内容帧到来才提交 200;缓冲拼回后事件
/// 顺序与原文逐字节一致(message_start 不得丢/不得重复,Usage 不丢给收尾计费)。
#[tokio::test]
async fn 正常流_前导缓冲拼回_顺序不变() {
    let st = wire_state();
    let lease = st.scheduler.acquire(Some("s")).await.unwrap();
    // 前导帧里夹一份 Usage(dario 形态),验证它随缓冲拼回不丢、且不误触发 Commit。
    let mut items = vec![
        Ok(StreamItem::Sse(SseEvent::new(
            "message_start",
            serde_json::json!({"message":{"model":"grok-4.5","usage":{"input_tokens":7}}}),
        ))),
        Ok(StreamItem::Usage(ChatUsage {
            input_tokens: 7,
            output_tokens: 0,
            ..Default::default()
        })),
    ];
    // 拼上正文段(复用现成 fixture 的内容部分,跳过它自己的 message_start)。
    let mut rest: Vec<_> = {
        let mut buf = Vec::new();
        let mut s = text_reply_stream();
        while let Some(it) = s.next().await {
            buf.push(it);
        }
        buf.drain(1..).collect()
    };
    items.append(&mut rest);
    let resp = stream_response(
        st.clone(),
        lease,
        chat_stream(items),
        req_model_m(),
        String::new(),
        test_ctx(&[]),
        std::time::Instant::now(),
        Wire::Anthropic,
    false,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = sse_body(resp).await;
    assert_eq!(
        body.matches("message_start").count(),
        1,
        "message_start 恰好一次,不得丢也不得重复: {body}"
    );
    assert!(body.contains("message_stop"));
}

/// dario 形态:上游 error 帧被包成 Ok(Sse)(gw-dario chat.rs:510)——窥探期首内容
/// 即 error 时不得 Commit 成 200,按 kind 回真实状态(2026-08-24 codex blocker#1)。
#[tokio::test]
async fn 窥探期error事件_按kind回真实状态() {
    let st = wire_state();
    let lease = st.scheduler.acquire(Some("s")).await.unwrap();
    let resp = stream_response(
        st.clone(),
        lease,
        chat_stream(vec![
            Ok(StreamItem::Sse(SseEvent::new(
                "message_start",
                serde_json::json!({"message":{"model":"m","usage":{"input_tokens":3}}}),
            ))),
            Ok(StreamItem::Sse(SseEvent::new(
                "error",
                serde_json::json!({"type":"error","error":{"type":"rate_limit_error","message":"上游原文"}}),
            ))),
        ]),
        req_model_m(),
        String::new(),
        test_ctx(&[]),
        std::time::Instant::now(),
        Wire::Anthropic,
    false,
    )
    .await;
    // rate_limit_error → RateLimited → 502(可重试),绝不是 200。
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    let body = sse_body(resp).await;
    assert!(body.contains("请求过于频繁"), "{body}");
    assert!(!body.contains("上游原文"), "上游原文不外泄: {body}");
}

#[tokio::test]
async fn 两种_openai_线缆的非流式响应各是各的形状() {
    let st = wire_state();

    let lease = st.scheduler.acquire(Some("s")).await.unwrap();
    let chat = collect_response(
        &st.scheduler,
        None,
        None,
        None,
        lease,
        text_reply_stream(),
        req_model_m(),
        String::new(),
        Arc::new(acct(&[])),
        std::time::Instant::now(),
        &real_provider("cursor"),
        Wire::OpenAiChat { include_usage: false },
    None,
    false,
    )
    .await;
    assert_eq!(chat.status(), StatusCode::OK);
    let v = body_json(chat).await;
    assert_eq!(v["object"], "chat.completion");
    assert_eq!(v["choices"][0]["message"]["content"], "hi");
    assert_eq!(v["choices"][0]["finish_reason"], "stop");
    assert_eq!(v["usage"]["prompt_tokens"], serde_json::json!(7));

    let lease = st.scheduler.acquire(Some("s")).await.unwrap();
    let responses = collect_response(
        &st.scheduler,
        None,
        None,
        None,
        lease,
        text_reply_stream(),
        req_model_m(),
        String::new(),
        Arc::new(acct(&[])),
        std::time::Instant::now(),
        &real_provider("cursor"),
        Wire::OpenAiResponses,
    None,
    false,
    )
    .await;
    let v = body_json(responses).await;
    assert_eq!(v["object"], "response");
    assert_eq!(v["status"], "completed");
    assert_eq!(v["output"][0]["content"][0]["text"], "hi");
    assert_eq!(v["usage"]["input_tokens"], serde_json::json!(7));
}

#[tokio::test]
async fn anthropic_线缆的非流式响应一个字节都没变() {
    let st = wire_state();
    let lease = st.scheduler.acquire(Some("s")).await.unwrap();
    let resp = collect_response(
        &st.scheduler,
        None,
        None,
        None,
        lease,
        text_reply_stream(),
        req_model_m(),
        String::new(),
        Arc::new(acct(&[])),
        std::time::Instant::now(),
        &real_provider("kiro"),
        Wire::Anthropic,
    None,
    false,
    )
    .await;
    let v = body_json(resp).await;
    // 还是 Anthropic Messages:没有 object/choices,有 content 数组与 stop_reason。
    assert!(v.get("object").is_none());
    assert!(v.get("choices").is_none());
    assert_eq!(v["content"][0]["text"], "hi");
    assert_eq!(v["stop_reason"], "end_turn");
}

#[tokio::test]
async fn 入站转换失败回_openai_形状的_400_并点名字段() {
    let st = wire_state();
    // 缺 messages:不占账号、不打上游,直接在门口挡掉。
    let resp = chat_completions(
        State(st.clone()),
        HeaderMap::new(),
        axum::body::Bytes::from(serde_json::json!({"model": "grok-4.5"}).to_string()),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v = body_json(resp).await;
    assert_eq!(v["error"]["type"], "invalid_request_error");
    assert_eq!(v["error"]["param"], "messages");
    // Anthropic 的错误外壳不该出现。
    assert!(v.get("type").is_none());

    let resp = responses(
        State(st),
        HeaderMap::new(),
        axum::body::Bytes::from(
            serde_json::json!({"model":"m","input":"hi","previous_response_id":"resp_x"})
                .to_string(),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v = body_json(resp).await;
    assert_eq!(v["error"]["param"], "previous_response_id");
}

#[tokio::test]
async fn 选号失败在_openai_线缆上也换成_openai_错误体() {
    // 空池 → AcquireError,走的是 `error_response` 那条自算状态码的路。
    let provider: Arc<dyn Provider> = Arc::new(HealGateMockProvider {
        chat_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    });
    let st = Arc::new(WorkerState {
        instance: 0,
        egress_desc: String::new(),
        group: String::new(),
        provider,
        settings_sync: parking_lot::RwLock::new(SettingsSync::default()),
        scheduler: AccountScheduler::new(
            Vec::new(),
            &gw_core::config::SchedulerConfig::default(),
        ),
        refresh_locks: parking_lot::Mutex::new(std::collections::HashMap::new()),
        rejection_refresh_at: parking_lot::Mutex::new(std::collections::HashMap::new()),
        usage_sink: None,
        pending_writes: PendingWrites::new(),
        store: None,
        quota_cache: parking_lot::Mutex::new(std::collections::HashMap::new()),
        quota_inflight: parking_lot::Mutex::new(std::collections::HashSet::new()),
        quota_sem: Arc::new(tokio::sync::Semaphore::new(1)),
        sync_lock: tokio::sync::Mutex::new(()),
        group_views: parking_lot::RwLock::new(std::collections::HashMap::new()),
        _client: reqwest::Client::new(),
    });
    let resp = chat_completions(
        State(st.clone()),
        HeaderMap::new(),
        axum::body::Bytes::from(
            serde_json::json!({
                "model": "grok-4.5",
                "messages": [{"role": "user", "content": "hi"}]
            })
            .to_string(),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let v = body_json(resp).await;
    assert_eq!(v["error"]["type"], "server_error");
    assert!(v.get("type").is_none(), "不能漏出 Anthropic 的错误外壳");

    // 同一条路径在 Anthropic 线缆上必须保持旧形状。
    let resp = messages(
        State(st),
        HeaderMap::new(),
        Json(serde_json::json!({
            "model": "grok-4.5",
            "messages": [{"role": "user", "content": "hi"}]
        })),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let v = body_json(resp).await;
    assert_eq!(v["type"], "error");
    assert_eq!(v["error"]["type"], "overloaded_error");
}

// ================= 最优控制调度器接线 =================

/// kiro 系模拟 provider:恒成功回一段文本,提供线程钉号键。
struct CiWireProvider;

#[async_trait::async_trait]
impl Provider for CiWireProvider {
    fn family(&self) -> &'static str { "kiro" }
    fn account_schema(&self) -> &'static [gw_core::account::FieldSpec] { &[] }
    async fn list_models(&self) -> Result<Vec<gw_core::model::ModelInfo>, UpstreamError> { Ok(vec![]) }
    async fn chat(&self, _: ChatRequest, _: &CallCtx) -> Result<gw_core::provider::ChatStream, UpstreamError> {
        Ok(text_reply_stream())
    }
    async fn refresh_auth(&self, account: &Account) -> Result<Account, UpstreamError> { Ok(account.clone()) }
    fn thread_key(&self, _req: &ChatRequest) -> Option<String> { Some("s/sess/thr".into()) }
}

fn ci_wire_state() -> Arc<WorkerState> {
    let mut extra = BTreeMap::new();
    extra.insert("access_token".into(), serde_json::json!("at"));
    extra.insert("refresh_token".into(), serde_json::json!("rt"));
    extra.insert("expires_at".into(), serde_json::json!("2099-01-01T00:00:00Z"));
    extra.insert("profile_arn".into(), serde_json::json!("arn:aws:codewhisperer:us-east-1:1:profile/X"));
    let acc = Arc::new(Account {
        account_id: "a".into(),
        provider: "kiro".into(),
        max_concurrency: 2,
        disabled: false,
        created_at: 0,
        extra,
    });
    Arc::new(WorkerState {
        instance: 0,
        egress_desc: String::new(),
        group: String::new(),
        provider: Arc::new(CiWireProvider),
        settings_sync: parking_lot::RwLock::new(SettingsSync::default()),
        scheduler: AccountScheduler::new(vec![acc], &gw_core::config::SchedulerConfig::default())
            .with_cost_index(|_| cost_index::PriceClass::Personal),
        refresh_locks: parking_lot::Mutex::new(std::collections::HashMap::new()),
        rejection_refresh_at: parking_lot::Mutex::new(std::collections::HashMap::new()),
        usage_sink: None,
        pending_writes: PendingWrites::new(),
        store: None,
        quota_cache: parking_lot::Mutex::new(std::collections::HashMap::new()),
        quota_inflight: parking_lot::Mutex::new(std::collections::HashSet::new()),
        quota_sem: Arc::new(tokio::sync::Semaphore::new(1)),
        sync_lock: tokio::sync::Mutex::new(()),
        group_views: parking_lot::RwLock::new(std::collections::HashMap::new()),
        _client: reqwest::Client::new(),
    })
}

/// 自动模式分类器走一遍真实的 `handle_chat`(入口级回归,2026-10-27)。
///
/// 锁住两件 10-23 重建时丢过的事:① 带计费头的生产形态与二阶段复核请求都本地放行、
/// 不碰上游;② 本地放行排在「畸形分组头拒绝」之后 —— 分组头坏了照样拒,不能因为
/// 本地直答就绕过提权保护。开关是进程级全局;gw-app 里只有这一个测试碰它,
/// 且只设成内置默认 allow。
#[tokio::test]
async fn automode_allow_through_handle_chat_respects_group_header() {
    use gates::automode::{set_config, SYSTEM_PREFIX};
    use gw_core::config::automode::{AutoModeConfig, AutoModeMode};
    set_config(&AutoModeConfig { mode: AutoModeMode::Allow, ..Default::default() }).unwrap();
    let system = serde_json::json!([
        {"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.311.b83; cc_entrypoint=cli;"},
        {"type": "text", "text": format!("{SYSTEM_PREFIX}\n\n## Context\n…")}
    ]);
    let stage1 = serde_json::json!({
        "model": "claude-sonnet-5", "max_tokens": 64, "stream": false,
        "thinking": {"type": "disabled"}, "stop_sequences": ["</block>"], "system": system,
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": "<transcript>ls</transcript>\n"},
            {"type": "text", "text": "Your ENTIRE response MUST begin with <block>."}]}]
    });
    let mut stage2 = stage1.clone();
    stage2.as_object_mut().unwrap().remove("stop_sequences");
    stage2["max_tokens"] = serde_json::json!(8192);
    stage2["messages"][0]["content"][1]["text"] =
        serde_json::json!("Review the classification process… Use <thinking> before responding with <block>.");
    let call = |headers: HeaderMap, body: serde_json::Value| async move {
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            handle_chat(ci_wire_state(), headers, body, Wire::Anthropic),
        )
        .await
        .unwrap()
    };

    for (name, body) in [("stage1", &stage1), ("stage2", &stage2)] {
        let resp = call(HeaderMap::new(), body.clone()).await;
        assert_eq!(resp.status(), StatusCode::OK, "{name}");
        let v = body_json(resp).await;
        assert!(
            v["id"].as_str().unwrap_or("").starts_with("msg_caio_automode_"),
            "{name}:必须本地放行,不能打到上游: {v}"
        );
        assert_eq!(v["content"][0]["text"], "<block>no</block>", "{name}");
        assert_eq!(v["stop_reason"], "end_turn", "{name}");
        assert!(v["usage"]["input_tokens"].as_u64().unwrap() > 0, "{name}:新输入不能报 0");
    }

    // 分组头畸形:先拒,不本地放行。
    let mut bad = HeaderMap::new();
    bad.insert(crate::GROUP_HEADER, axum::http::HeaderValue::from_static("   "));
    let resp = call(bad, stage1.clone()).await;
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body_json(resp).await["type"], "error");

    // 普通请求照常走上游。
    let normal = serde_json::json!({"model": "m", "stream": false, "max_tokens": 10,
        "messages": [{"role": "user", "content": "hello"}]});
    let v = body_json(call(HeaderMap::new(), normal).await).await;
    assert!(!v["id"].as_str().unwrap_or("").starts_with("msg_caio_automode_"), "{v}");
}

/// 逐条筛查必须接在请求入口上(10-23 重建时整段丢失、5 天没人发现,这条锁住接线)。
/// 只用默认的 observe:改全局模式成 enforce 会误伤并行跑的其他测试(它们也发单轮小请求)。
/// enforce 的拒绝语义由 `gates::request_screen` 的单测覆盖。
#[tokio::test]
async fn request_screen_is_wired_into_handle_chat() {
    use gates::request_screen::snapshot;
    let mut keyed = HeaderMap::new();
    keyed.insert(crate::CLIENT_KEY_HEADER, axum::http::HeaderValue::from_static("sk-test"));
    let call = |headers: HeaderMap, body: serde_json::Value| async move {
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            handle_chat(ci_wire_state(), headers, body, Wire::Anthropic),
        )
        .await
        .unwrap()
    };
    let probe = serde_json::json!({"model": "m", "stream": false, "max_tokens": 8,
        "messages": [{"role": "user", "content": "ok"}]});
    let before = snapshot().probe_hits;
    let resp = call(keyed.clone(), probe).await;
    assert!(snapshot().probe_hits > before, "探针没有经过逐条筛查");
    // observe:命中也放行,照常走上游。
    assert_ne!(resp.status(), StatusCode::BAD_REQUEST);

    let big = serde_json::json!({"model": "m", "stream": false, "max_tokens": 1000,
        "messages": [{"role": "user", "content": "a".repeat(6_400_000)}]});
    let before = snapshot().oversize_hits;
    let _ = call(keyed, big).await;
    assert!(snapshot().oversize_hits > before, "超大输入没有经过逐条筛查");
}

/// enforce 下探针与超大输入必须在入口直接回 400(我方文案),不得落到号上。
/// 模式用线程级覆盖,不改全局(见 `request_screen::override_mode`)。
#[tokio::test]
async fn request_screen_enforce_rejects_before_upstream() {
    use gates::request_screen::{override_mode, OVERSIZE_MESSAGE, PROBE_MESSAGE};
    let _enforce = override_mode(gw_core::config::batch_guard::BatchGuardMode::Enforce);
    let mut keyed = HeaderMap::new();
    keyed.insert(crate::CLIENT_KEY_HEADER, axum::http::HeaderValue::from_static("sk-test"));
    let probe = serde_json::json!({"model": "m", "stream": false, "max_tokens": 8,
        "messages": [{"role": "user", "content": "ok"}]});
    let big = serde_json::json!({"model": "m", "stream": false, "max_tokens": 1000,
        "messages": [{"role": "user", "content": "a".repeat(6_400_000)}]});
    for (name, body, msg) in [("探针", &probe, PROBE_MESSAGE), ("超大输入", &big, OVERSIZE_MESSAGE)] {
        let resp = handle_chat(ci_wire_state(), keyed.clone(), body.clone(), Wire::Anthropic).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{name}");
        let v = body_json(resp).await;
        assert_eq!(v["error"]["message"], msg, "{name}:必须是筛查文案,说明没走到上游: {v}");
    }
    // 没有客户 key 的内部请求不筛。
    let resp = handle_chat(ci_wire_state(), HeaderMap::new(), probe.clone(), Wire::Anthropic).await;
    assert_ne!(resp.status(), StatusCode::BAD_REQUEST);
    // 正常的单轮首句(max_tokens 大)照常走上游。
    let normal = serde_json::json!({"model": "m", "stream": false, "max_tokens": 4096,
        "messages": [{"role": "user", "content": "hello"}]});
    let resp = handle_chat(ci_wire_state(), keyed, normal, Wire::Anthropic).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn cost_index_success_writes_thread_warmth_for_stream_and_non_stream() {
    for stream in [true, false] {
        let st = ci_wire_state();
        assert_eq!(st.scheduler.active_algorithm(), "cost_index");
        let resp = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            handle_chat(
                st.clone(),
                HeaderMap::new(),
                serde_json::json!({"model":"m","stream":stream,"max_tokens":10,
                    "messages":[{"role":"user","content":"hello"}]}),
                Wire::Anthropic,
            ),
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // 流式:消费完正文,StreamCtx 才 Drop 并回写。
        let _ = axum::body::to_bytes(resp.into_body(), 1_000_000).await.unwrap();
        let q = st.scheduler.queue_stats();
        assert_eq!(q.ci_decisions, 1, "stream={stream}:走的是最优控制调度器");
        assert_eq!(q.ci_threads, 1, "stream={stream}:成功收尾后线程热度已回写");
    }
}

#[test]
fn cost_index_only_for_kiro_family() {
    for f in ["cursor", "claude-dario", "claude-subprocess"] {
        assert!(!cost_index_allowed(real_provider(f).as_ref()), "{f} 不该用代价指数");
    }
    // 测试进程不设 KIRO_COST_INDEX → kiro 允许。
    if std::env::var("KIRO_COST_INDEX").is_err() {
        assert!(cost_index_allowed(kiro_p().as_ref()));
    }
    // 其余处理特征逐家核对(与收回前的 family 分支逐一对应)。
    let kiro = kiro_p();
    assert!(kiro.suspend_lifecycle() && !kiro.validate_on_load() && !kiro.openai_wire());
    let cursor = real_provider("cursor");
    assert!(!cursor.suspend_lifecycle() && cursor.validate_on_load());
    let dario = real_provider("claude-dario");
    assert!(!dario.suspend_lifecycle() && dario.validate_on_load());
    let sub = real_provider("claude-subprocess");
    assert!(!sub.suspend_lifecycle() && !sub.validate_on_load() && !sub.refresh_exempt(&acct(&[])));
}

// ================= 拒答记忆接线(gates::refusal_guard) =================

/// 内容里带 `forbidden` 就零产出拒答(流内 Err,与 Kiro 分类器同形),否则正常回复。
/// 记下每次调用落在哪个号上。
struct RefusingProvider {
    calls: parking_lot::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl Provider for RefusingProvider {
    fn family(&self) -> &'static str { "kiro" }
    fn account_schema(&self) -> &'static [gw_core::account::FieldSpec] { &[] }
    async fn list_models(&self) -> Result<Vec<gw_core::model::ModelInfo>, UpstreamError> { Ok(vec![]) }
    async fn chat(&self, req: ChatRequest, ctx: &CallCtx) -> Result<gw_core::provider::ChatStream, UpstreamError> {
        self.calls.lock().push(ctx.account.account_id.clone());
        if req.body.to_string().contains("forbidden") {
            let e = UpstreamError::content_filtered("CYBER", "The selected model cannot continue this conversation.");
            Ok(chat_stream(vec![Err(e)]))
        } else {
            Ok(text_reply_stream())
        }
    }
    async fn refresh_auth(&self, account: &Account) -> Result<Account, UpstreamError> { Ok(account.clone()) }
}

fn refusal_account(id: &str, disabled: bool) -> Arc<Account> {
    let mut extra = BTreeMap::new();
    extra.insert("access_token".into(), serde_json::json!("at"));
    extra.insert("refresh_token".into(), serde_json::json!("rt"));
    extra.insert("expires_at".into(), serde_json::json!("2099-01-01T00:00:00Z"));
    extra.insert("profile_arn".into(), serde_json::json!("arn:aws:codewhisperer:us-east-1:1:profile/X"));
    Arc::new(Account { account_id: id.into(), provider: "kiro".into(), max_concurrency: 2, disabled, created_at: 0, extra })
}

fn refusal_state(provider: Arc<RefusingProvider>) -> Arc<WorkerState> {
    Arc::new(WorkerState {
        instance: 0,
        egress_desc: String::new(),
        group: String::new(),
        provider,
        settings_sync: parking_lot::RwLock::new(SettingsSync::default()),
        scheduler: AccountScheduler::new(
            vec![refusal_account("a", false), refusal_account("b", false)],
            &gw_core::config::SchedulerConfig::default(),
        ),
        refresh_locks: parking_lot::Mutex::new(std::collections::HashMap::new()),
        rejection_refresh_at: parking_lot::Mutex::new(std::collections::HashMap::new()),
        usage_sink: None,
        pending_writes: PendingWrites::new(),
        store: None,
        quota_cache: parking_lot::Mutex::new(std::collections::HashMap::new()),
        quota_inflight: parking_lot::Mutex::new(std::collections::HashSet::new()),
        quota_sem: Arc::new(tokio::sync::Semaphore::new(1)),
        sync_lock: tokio::sync::Mutex::new(()),
        group_views: parking_lot::RwLock::new(std::collections::HashMap::new()),
        _client: reqwest::Client::new(),
    })
}

/// 恢复内置默认(observe),免得把 enforce 留给并行跑的其它测试。
struct RefusalGuardRestore;
impl Drop for RefusalGuardRestore {
    fn drop(&mut self) {
        let _ = gates::refusal_guard::set_config(&Default::default());
        gates::refusal_guard::reset_for_test();
    }
}

fn set_refusal_guard(mode: gw_core::config::refusal_guard::RefusalGuardMode, retries: u32) {
    gates::refusal_guard::set_config(&gw_core::config::refusal_guard::RefusalGuardConfig {
        mode,
        retries,
        ttl_secs: 3600,
    })
    .unwrap();
    gates::refusal_guard::reset_for_test();
}

async fn refusal_call(st: &Arc<WorkerState>, model: &str, text: &str, stream: bool) -> (StatusCode, String) {
    let body = serde_json::json!({"model": model, "stream": stream, "max_tokens": 1000,
        "messages": [{"role": "user", "content": [{"type": "text", "text": text}]}]});
    let resp = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        handle_chat(st.clone(), HeaderMap::new(), body, Wire::Anthropic),
    )
    .await
    .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000).await.unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// 被拒一次 → 原样重发只落在原号(另一个号空着也不用)→ 再被拒 → 第三次本地回拒答、不碰上游;
/// 换模型照常放行。流式与非流式两条收尾路径都要记得上账。
#[tokio::test]
// 单线程测试运行时,跨 await 持同步锁不会死锁;锁的作用是和本模块单测串行。
#[allow(clippy::await_holding_lock)]
async fn refusal_guard_pins_then_blocks_through_handle_chat() {
    use gw_core::config::refusal_guard::RefusalGuardMode;
    let _serial = gates::refusal_guard::TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let _restore = RefusalGuardRestore;
    set_refusal_guard(RefusalGuardMode::Enforce, 1);
    for stream in [false, true] {
        let provider = Arc::new(RefusingProvider { calls: parking_lot::Mutex::new(Vec::new()) });
        let st = refusal_state(provider.clone());
        let text = format!("forbidden disasm stream={stream}");

        let (status, _) = refusal_call(&st, "m", &text, stream).await;
        assert_ne!(status, StatusCode::OK, "stream={stream}:第一次是上游拒答");
        let first = provider.calls.lock()[0].clone();

        // 钉号:连发几次干净请求把 LRU 推到另一个号上,再重发被拒的那条 —— 仍只能落在原号。
        for i in 0..3 {
            let (status, _) = refusal_call(&st, "m", &format!("hello {i}"), stream).await;
            assert_eq!(status, StatusCode::OK);
        }
        let before = provider.calls.lock().len();
        let (_, _) = refusal_call(&st, "m", &text, stream).await;
        let calls = provider.calls.lock().clone();
        assert_eq!(calls.len(), before + 1, "stream={stream}:还允许再发 1 次上游");
        assert_eq!(calls[before], first, "stream={stream}:重发必须钉回第一次被拒的号");

        // 超次数:本地回拒答,上游调用数不变,文案带上游原文 + 网关说明。
        let (status, body) = refusal_call(&st, "m", &text, stream).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "stream={stream}");
        assert!(body.contains("CYBER") && body.contains("网关"), "stream={stream}: {body}");
        assert_eq!(provider.calls.lock().len(), before + 1, "stream={stream}:不得再碰上游");

        // 换模型是上游建议的出路:照常放行(会再被拒一次,但那是新请求)。
        let (_, _) = refusal_call(&st, "m2", &text, stream).await;
        assert_eq!(provider.calls.lock().len(), before + 2, "stream={stream}:换模型照常走上游");
    }
}

/// 钉的号用不了(被禁用)→ 本地回拒答,另一个健康号零调用。
#[tokio::test]
// 单线程测试运行时,跨 await 持同步锁不会死锁;锁的作用是和本模块单测串行。
#[allow(clippy::await_holding_lock)]
async fn refusal_guard_never_spreads_when_pinned_account_is_gone() {
    use gw_core::config::refusal_guard::RefusalGuardMode;
    let _serial = gates::refusal_guard::TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let _restore = RefusalGuardRestore;
    set_refusal_guard(RefusalGuardMode::Enforce, 1);
    let provider = Arc::new(RefusingProvider { calls: parking_lot::Mutex::new(Vec::new()) });
    let st = refusal_state(provider.clone());
    let (status, _) = refusal_call(&st, "m", "forbidden pinned-gone", false).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let first = provider.calls.lock()[0].clone();
    let other = if first == "a" { "b" } else { "a" };
    st.scheduler.sync_accounts(vec![refusal_account(&first, true), refusal_account(other, false)]);

    let (status, body) = refusal_call(&st, "m", "forbidden pinned-gone", false).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("网关"), "是本地拒答而不是选号失败: {body}");
    assert_eq!(provider.calls.lock().len(), 1, "不得换到别的号");
}

/// observe:照常记账,但重发照常选号、照常走上游。
#[tokio::test]
// 单线程测试运行时,跨 await 持同步锁不会死锁;锁的作用是和本模块单测串行。
#[allow(clippy::await_holding_lock)]
async fn refusal_guard_observe_changes_nothing_on_the_wire() {
    use gw_core::config::refusal_guard::RefusalGuardMode;
    let _serial = gates::refusal_guard::TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let _restore = RefusalGuardRestore;
    set_refusal_guard(RefusalGuardMode::Observe, 0);
    let provider = Arc::new(RefusingProvider { calls: parking_lot::Mutex::new(Vec::new()) });
    let st = refusal_state(provider.clone());
    for _ in 0..3 {
        let (status, body) = refusal_call(&st, "m", "forbidden observe", false).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(!body.contains("网关"), "observe 不本地拒答: {body}");
    }
    assert_eq!(provider.calls.lock().len(), 3, "每次都走了上游");
    let snap = gates::refusal_guard::snapshot(std::time::Instant::now());
    assert_eq!(snap["recorded_total"], 3, "observe 照常记账: {snap}");
    assert_eq!(snap["blocked_total"], 0);
}
