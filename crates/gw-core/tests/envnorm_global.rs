//! envnorm 进程级开关:按线路生效、非法配置保留上一份。单独一个测试二进制,不与别的测试抢全局。
use gw_core::account::Account;
use gw_core::config::EnvNormalizeConfig;
use gw_core::envnorm;
use serde_json::json;
use std::collections::BTreeMap;

fn acct() -> Account {
    Account { account_id: "a".into(), provider: "claude-dario".into(), max_concurrency: 1, disabled: false, created_at: 0, extra: BTreeMap::new() }
}

#[test]
fn switch_is_per_family_and_bad_config_keeps_previous() {
    let src = json!({"system": "<omo-env>\n  Timezone: Asia/Shanghai\n  Locale: zh-CN\n</omo-env>"});
    // 默认全关:字节完全不变。
    let mut b = src.clone();
    assert_eq!(envnorm::apply_for("claude-dario", &mut b, &acct()).total(), 0);
    assert_eq!(b, src);

    let on = EnvNormalizeConfig { providers: vec!["claude-dario".into()], ..EnvNormalizeConfig::default() };
    envnorm::set_config(&on).unwrap();
    assert!(envnorm::enabled_for("claude-dario") && !envnorm::enabled_for("kiro"));
    let mut b = src.clone();
    assert_eq!(envnorm::apply_for("claude-dario", &mut b, &acct()).total(), 2);
    assert!(b["system"].as_str().unwrap().contains("Timezone: Asia/Singapore"));
    let mut k = src.clone();
    assert_eq!(envnorm::apply_for("kiro", &mut k, &acct()).total(), 0, "未开启的线路不处理");
    assert_eq!(k, src);

    // 非法配置:报错且沿用上一份。
    let bad = EnvNormalizeConfig { providers: vec!["openai".into()], ..EnvNormalizeConfig::default() };
    assert!(envnorm::set_config(&bad).is_err());
    assert!(envnorm::enabled_for("claude-dario"));

    envnorm::set_config(&EnvNormalizeConfig::default()).unwrap();
    assert!(!envnorm::enabled_for("claude-dario"));
}
