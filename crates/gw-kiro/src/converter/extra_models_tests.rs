//! `kiro_extra_models` 热追加的测试。热追加是进程全局状态:每个改它的测试都持 [`TEST_LOCK`]、
//! 进出各清一次,避免与「数公告条数」等测试互相污染。

use super::*;
use crate::converter::model_map::{
    advertised_models, clamp_effort_for_model, denies_free, get_context_window_size, map_model,
    resolve_base, signature_id_for, tokenizer_for_kiro_model, SignatureId,
};
use crate::text_tokens::Tokenizer;

/// 持锁 + 进出清空。
struct Fresh(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

impl Fresh {
    fn new() -> Self {
        let g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_for_test();
        Fresh(g)
    }
}

impl Drop for Fresh {
    fn drop(&mut self) {
        reset_for_test();
    }
}

fn spec(id: &str, kiro_model: &str, like: &str) -> KiroExtraModelSpec {
    serde_json::from_value(serde_json::json!({"id": id, "kiro_model": kiro_model, "like": like})).unwrap()
}

#[test]
fn pro_only_defaults_to_true_when_omitted() {
    assert!(spec("claude-x-9", "claude-x.9", "claude-sonnet-5-5").pro_only);
    let open: KiroExtraModelSpec = serde_json::from_value(serde_json::json!(
        {"id": "a-1", "kiro_model": "a.1", "like": "claude-sonnet-4-5", "pro_only": false}
    ))
    .unwrap();
    assert!(!open.pro_only);
}

#[test]
fn applied_row_is_visible_to_every_derived_lookup() {
    let _f = Fresh::new();
    let mut s = spec("claude-haiku-6", "claude-haiku-6", "claude-sonnet-5-5");
    s.display_name = Some("Claude Haiku 6".into());
    s.effort_levels = Some(vec!["low".into(), "medium".into(), "high".into()]);
    s.default_effort = Some("medium".into());
    assert_eq!(set_extra_models(&[s]), ApplyOutcome::Applied);

    // 路由:plain / -thinking / 日期快照形态都不该 None。
    assert_eq!(map_model("claude-haiku-6").as_deref(), Some("claude-haiku-6"));
    assert_eq!(map_model("claude-haiku-6-thinking").as_deref(), Some("claude-haiku-6"));
    // 身份短名缺省 = 展示名去掉 `Claude `。
    assert_eq!(resolve_base("claude-haiku-6").map(|m| m.identity_short), Some("Haiku 6"));
    // 公告目录。
    let ids: Vec<String> = advertised_models().into_iter().map(|m| m.id).collect();
    assert!(ids.contains(&"claude-haiku-6".to_string()));
    assert!(ids.contains(&"claude-haiku-6-thinking".to_string()));
    // 档位夹取:自己的档位表(没有 max → 夹到 high)。
    assert_eq!(clamp_effort_for_model("claude-haiku-6", None), Some("medium"));
    assert_eq!(clamp_effort_for_model("claude-haiku-6", Some("max")), Some("high"));
    // 行为字段继承自 like(sonnet-5.5)。
    assert_eq!(signature_id_for("claude-haiku-6"), SignatureId::Anonymous { envelope_version: 4 });
    assert_eq!(tokenizer_for_kiro_model("claude-haiku-6"), Tokenizer::Current);
    assert_eq!(get_context_window_size("claude-haiku-6"), 1_000_000);
    // 默认只给付费档。
    assert!(denies_free("claude-haiku-6"));
    assert_eq!(active_extra_ids(), vec!["claude-haiku-6".to_string()]);
}

#[test]
fn optional_fields_inherit_from_like() {
    let _f = Fresh::new();
    // like haiku-4.5:无档位、窗口 200k、Unsigned、旧分词器。
    let mut s = spec("claude-mini-1", "claude-mini.1", "claude-haiku-4-5");
    s.pro_only = false;
    assert_eq!(set_extra_models(&[s]), ApplyOutcome::Applied);
    let m = resolve_base("claude-mini-1").unwrap();
    assert!(m.effort_levels.is_empty());
    assert_eq!(m.default_effort, None);
    assert_eq!(m.context_window, 200_000);
    assert_eq!(m.display_name, "claude-mini-1", "缺省展示名 = id");
    assert_eq!(signature_id_for("claude-mini.1"), SignatureId::Unsigned);
    assert_eq!(tokenizer_for_kiro_model("claude-mini.1"), Tokenizer::Legacy);
    assert!(!denies_free("claude-mini-1"));
    assert_eq!(clamp_effort_for_model("claude-mini-1", Some("high")), None, "无档位表一个字段都不发");
}

#[test]
fn default_effort_falls_back_to_like_then_lowest() {
    // like sonnet-5.5 的 default 是 high:在新档位里就用它,不在就取最低档。
    let like = like_row("claude-sonnet-5-5").unwrap();
    let mut s = spec("a-1", "a.1", "claude-sonnet-5-5");
    s.effort_levels = Some(vec!["medium".into(), "high".into()]);
    assert_eq!(resolve_efforts(&s, like).unwrap().1, Some("high"));
    s.effort_levels = Some(vec!["low".into(), "medium".into()]);
    assert_eq!(resolve_efforts(&s, like).unwrap().1, Some("low"));
}

#[test]
fn identical_settings_do_not_rebuild_or_leak() {
    let _f = Fresh::new();
    let specs = vec![spec("claude-haiku-6", "claude-haiku-6", "claude-sonnet-5-5")];
    assert_eq!(set_extra_models(&specs), ApplyOutcome::Applied);
    let before = extra_rows().as_ptr();
    for _ in 0..3 {
        assert_eq!(set_extra_models(&specs), ApplyOutcome::Unchanged, "30s 轮询同一份设置不该重建");
    }
    assert_eq!(extra_rows().as_ptr(), before, "没换表 = 没泄漏新的一份");
}

#[test]
fn invalid_set_is_rejected_whole_and_old_table_kept() {
    let _f = Fresh::new();
    assert_eq!(set_extra_models(&[spec("claude-haiku-6", "claude-haiku-6", "claude-sonnet-5-5")]), ApplyOutcome::Applied);
    // 第二条撞内置名 → 整组拒收(第一条合法也不生效),旧表原样保留。
    let bad = vec![
        spec("claude-new-1", "claude-new.1", "claude-sonnet-5-5"),
        spec("claude-opus-4-8", "claude-new.2", "claude-sonnet-5-5"),
    ];
    let ApplyOutcome::Rejected { reason, fresh: true } = set_extra_models(&bad) else {
        panic!("应整组拒收");
    };
    assert!(reason.contains("第 2 条"), "{reason}");
    assert_eq!(active_extra_ids(), vec!["claude-haiku-6".to_string()]);
    assert!(map_model("claude-new-1").is_none(), "被拒组里的合法条目也不生效");
    // 同一份坏设置再来一次:仍报拒收(面板错误不消失),但 fresh=false(不重复打日志),旧表仍在。
    assert!(matches!(set_extra_models(&bad), ApplyOutcome::Rejected { fresh: false, .. }));
    assert_eq!(active_extra_ids(), vec!["claude-haiku-6".to_string()]);
}

#[test]
fn empty_settings_clear_the_extra_rows() {
    let _f = Fresh::new();
    assert_eq!(set_extra_models(&[spec("claude-haiku-6", "claude-haiku-6", "claude-sonnet-5-5")]), ApplyOutcome::Applied);
    assert_eq!(set_extra_models(&[]), ApplyOutcome::Applied);
    assert!(active_extra_ids().is_empty());
    assert!(resolve_base("claude-haiku-6").is_none());
    // 内置行不受影响。
    assert_eq!(map_model("claude-haiku-5-5").as_deref(), Some("claude-haiku-5.5"));
}

#[test]
fn validation_rejects_each_bad_shape() {
    let ok = || spec("claude-x-9", "claude-x.9", "claude-sonnet-5-5");
    let cases: Vec<(&str, KiroExtraModelSpec)> = vec![
        ("撞内置对外名", KiroExtraModelSpec { id: "claude-sonnet-5-5".into(), ..ok() }),
        ("撞内置日期别名", KiroExtraModelSpec { id: "claude-haiku-4-5-20251001".into(), ..ok() }),
        ("撞内置 kiro_model", KiroExtraModelSpec { kiro_model: "claude-haiku-5.5".into(), ..ok() }),
        ("like 不存在", KiroExtraModelSpec { like: "claude-nope".into(), ..ok() }),
        ("大写", KiroExtraModelSpec { id: "Claude-X-9".into(), ..ok() }),
        ("-thinking 后缀", KiroExtraModelSpec { id: "claude-x-9-thinking".into(), ..ok() }),
        ("日期后缀", KiroExtraModelSpec { id: "claude-x-20261103".into(), ..ok() }),
        ("空 kiro_model", KiroExtraModelSpec { kiro_model: String::new(), ..ok() }),
        ("窗口为 0", KiroExtraModelSpec { context_window: Some(0), ..ok() }),
        ("档位乱序", KiroExtraModelSpec { effort_levels: Some(vec!["high".into(), "low".into()]), ..ok() }),
        ("档位不认识", KiroExtraModelSpec { effort_levels: Some(vec!["turbo".into()]), ..ok() }),
        ("default 不在档位里", KiroExtraModelSpec {
            effort_levels: Some(vec!["low".into()]),
            default_effort: Some("max".into()),
            ..ok()
        }),
        ("身份短名带换行", KiroExtraModelSpec { identity_short: Some("X\nY".into()), ..ok() }),
    ];
    for (what, s) in cases {
        assert!(validate_extra_models(&[s]).is_err(), "{what} 应被拒");
    }
    // 组内重复。
    assert!(validate_extra_models(&[ok(), ok()]).is_err(), "id 重复应被拒");
    let dup_kiro = KiroExtraModelSpec { id: "claude-x-10".into(), ..ok() };
    assert!(validate_extra_models(&[ok(), dup_kiro]).is_err(), "kiro_model 重复应被拒");
    // 条数上限。
    let many: Vec<KiroExtraModelSpec> = (0..=MAX_EXTRA_MODELS)
        .map(|i| spec(&format!("m-{i}"), &format!("m.{i}"), "claude-sonnet-5-5"))
        .collect();
    assert!(validate_extra_models(&many).is_err());
    // 合法的基准本身能过。
    assert!(validate_extra_models(&[ok()]).is_ok());
}
