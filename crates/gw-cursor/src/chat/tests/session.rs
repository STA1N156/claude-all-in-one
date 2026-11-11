//! chat 测试:会话键与请求头。

use super::*;

/// 亲和键锚点必须跳过注入的 system 消息:worker 是在 `chat()` 之前拿**原始**
/// body 调这个函数的,那时分流还没跑。锚点取到每轮都变的计数器 = 亲和归零。
#[test]
fn 亲和键锚点跳过注入的system消息() {
    let with_inject = json!({"messages": [
        {"role": "system", "content": "<total_tokens>15000000 tokens left</total_tokens>"},
        {"role": "user", "content": "锚点甲"},
    ]});
    let next_round = json!({"messages": [
        {"role": "system", "content": "<total_tokens>14000000 tokens left</total_tokens>"},
        {"role": "user", "content": "锚点甲"},
        {"role": "assistant", "content": "回"},
        {"role": "user", "content": "再问"},
    ]});
    let a = affinity_key_from_body(&with_inject);
    assert!(a.is_some());
    assert_eq!(a, affinity_key_from_body(&next_round), "同会话跨轮必须稳定");
    // 无注入时与旧行为一致。
    let plain = json!({"messages": [{"role": "user", "content": "锚点甲"}]});
    assert_eq!(a, affinity_key_from_body(&plain));
}

/// **会话键必须对 Claude Code 的滚动 billing 指纹免疫。**
///
/// 不剥那行的后果是连锁的:键每请求都变 → 账号钉扎失效 + `conversation_id` 每轮都变
/// → `phase_for` 永远给 `Opening`(环境块按首轮形态发)→ 顺带缓存指纹每轮都变、
/// 命中率恒为 0。**注意它治不了「模型绕圈」** —— `fold_history` 是无条件执行的,
/// 与 phase 无关(见 `extract_system` 的注释)。
#[test]
fn 会话键不受每请求变化的_cch_影响() {
    let body = |cch: &str| {
        serde_json::json!({
            "model": "grok-4.5",
            "system": format!(
                "x-anthropic-billing-header: cc_version=2.1.63.a43; cc_entrypoint=cli; cch={cch};\n\
                 You are Claude Code, Anthropic's official CLI for Claude.\n"
            ),
            "messages": [{"role": "user", "content": "帮我看下这个函数"}],
        })
    };
    let k1 = affinity_key_from_body(&body("ea527")).expect("要能派生出键");
    let k2 = affinity_key_from_body(&body("b91f0")).expect("要能派生出键");
    assert_eq!(
        k1, k2,
        "同一会话两轮的键必须相同,否则钉扎/续写/缓存三件事一起废"
    );

    // 顺带确认:发给上游的 system 里不能再留着那行随机数(它会毒化 prefix cache)。
    let sys = extract_system(&body("ea527"));
    assert!(
        !sys.contains("x-anthropic-billing-header"),
        "指纹行不该进上游报文"
    );
    assert!(sys.contains("You are Claude Code"), "正文必须保留");

    // 反向:换了真正的会话内容(system 正文或开场白)仍必须换键。
    let other = serde_json::json!({
        "model": "grok-4.5",
        "system": "x-anthropic-billing-header: cch=ea527;\n你是另一个 agent\n",
        "messages": [{"role": "user", "content": "帮我看下这个函数"}],
    });
    assert_ne!(k1, affinity_key_from_body(&other).unwrap());
}

#[test]
fn session_keys_are_stable_per_conversation_and_differ_by_purpose() {
    let a = session_key("tok", "conv-1", "blob");
    assert_eq!(a.len(), 64);
    assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    // 同会话稳定
    assert_eq!(a, session_key("tok", "conv-1", "blob"));
    // 换用途、换会话、换号都要不同
    assert_ne!(a, session_key("tok", "conv-1", "fs"));
    assert_ne!(a, session_key("tok", "conv-2", "blob"));
    assert_ne!(a, session_key("tok2", "conv-1", "blob"));
}

#[test]
fn headers_match_protocol_section_2() {
    let c = reqwest::Client::new();
    let req = apply_headers(c.post("https://example.invalid/x"), &ctx())
        .build()
        .unwrap();
    let h = req.headers();
    let get = |k: &str| h.get(k).map(|v| v.to_str().unwrap().to_string());

    // §2.1 静态 —— 含两条旧代码发反了的
    assert_eq!(get("x-ghost-mode").as_deref(), Some("true"));
    assert_eq!(get("x-new-onboarding-completed").as_deref(), Some("false"));
    assert_eq!(get("connect-content-encoding").as_deref(), Some("gzip"));
    assert_eq!(get("x-cursor-streaming").as_deref(), Some("true"));
    assert_eq!(get("x-cursor-remote-type").as_deref(), Some("none"));
    assert_eq!(
        get("x-cursor-retryinterceptor-enabled").as_deref(),
        Some("true")
    );

    // §2.2 —— client-type 是 glass 不是 ide;两条必须**不存在**
    assert_eq!(get("x-cursor-client-type").as_deref(), Some("glass"));
    assert_eq!(get("x-cursor-client-layout").as_deref(), Some("glass"));
    assert!(
        get("x-cursor-client-commit").is_none(),
        "真请求不发 client-commit"
    );
    assert!(
        get("x-cursor-client-os-version").is_none(),
        "真请求不发 client-os-version"
    );

    // §2.3 派生
    assert_eq!(get("authorization").as_deref(), Some("Bearer tok"));
    assert_eq!(get("x-client-key").unwrap().len(), 64);
    assert_eq!(get("x-session-id").unwrap().len(), 36);
    // checksum = b64url(6B) + 64 + "/" + 64 = 137
    assert_eq!(get("x-cursor-checksum").unwrap().len(), 137);

    // §2.5 每会话
    assert_eq!(get("x-blob-encryption-key").unwrap().len(), 64);
    assert_eq!(get("x-fs-client-key").unwrap().len(), 64);
    assert_ne!(get("x-blob-encryption-key"), get("x-fs-client-key"));

    // 真请求发空 cookie 头
    assert_eq!(get("cookie").as_deref(), Some(""));

    // §2.6 trace:original == request,amzn 带 Root=
    let rid = get("x-request-id").unwrap();
    assert_eq!(get("x-original-request-id").as_deref(), Some(rid.as_str()));
    assert_eq!(get("x-amzn-trace-id"), Some(format!("Root={rid}")));
    // 两组 traceparent 独立随机,且 **flags 不同**(抓包实物 -00 / -01)
    assert_ne!(get("traceparent"), get("backend-traceparent"));
    assert!(get("traceparent").unwrap().starts_with("00-"));
    assert!(
        get("traceparent").unwrap().ends_with("-00"),
        "traceparent 未采样"
    );
    assert!(
        get("backend-traceparent").unwrap().ends_with("-01"),
        "backend 已采样"
    );
}

/// 模拟缓存的键 = 账号 + **稳定会话 ID**,不能用发给 Cursor 的会话 ID。
///
/// 2026-11-10:会话在两个号之间来回跳 → 回到原号时被判分叉 → provider 每轮按全部历史
/// 指纹重算一个新的 Cursor 会话 ID。键里用它的话每轮都是新键、模拟永远判 0
/// (同号连续 5 轮报 0,而上游每轮读 14~17 万缓存,客户被按全价收)。上游缓存只按号
/// 隔离,与 Cursor 会话 ID 无关(Cursor 账单实证),所以键里换成稳定会话 ID、保留账号。
#[test]
fn 模拟缓存键用稳定会话_不随分叉换的会话id变() {
    let mut a = ctx();
    a.conversation_id = "wire-turn-1".into();
    a.sim_conversation_id = "base-conv".into();
    let mut b = ctx();
    b.conversation_id = "wire-turn-2-diverged".into();
    b.sim_conversation_id = "base-conv".into();
    assert_eq!(sim_session_key(&a), sim_session_key(&b), "分叉换 Cursor 会话 ID 不能让模拟键变");

    // 不同号仍隔离(上游缓存按号隔离:换号真的会整段重写)
    let mut c = b.clone();
    c.account_id = "acc-2".into();
    assert_ne!(sim_session_key(&b), sim_session_key(&c));

    // 端到端:第一轮按 a 的键提交,第二轮 b(Cursor 会话 ID 已变)读得到前缀
    let fps = crate::cache_sim::fingerprints_from_context("sys", &[], &[
        run::Turn { is_user: true, text: "你好".repeat(2000) },
    ], est_text_tokens);
    let key_a = sim_session_key(&a);
    let (_, gen) = crate::cache_sim::peek(&key_a, "claude-x", &fps);
    crate::cache_sim::commit(&key_a, "claude-x", fps.clone(), gen);
    let (hit, _) = crate::cache_sim::peek(&sim_session_key(&b), "claude-x", &fps);
    assert!(hit.cache_read_tokens > 0, "同号同稳定会话:第二轮应命中第一轮提交的前缀");
}
