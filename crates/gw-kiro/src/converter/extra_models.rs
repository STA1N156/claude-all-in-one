//! Kiro 模型**热追加**(`kiro_extra_models` 设置项)。
//!
//! 内置权威表 [`super::model_map::KIRO_MODELS`] 是编译期常量,上游每上一个新模型都得重新构建部署。
//! 本模块让运维在设置里追加模型行,worker 30s 热应用后:路由(`map_model`)、`/v1/models` 公告、
//! 身份行、窗口、effort 夹取、签名门控、分词器、FREE 过滤全部自动跟上 —— 因为这些派生点都经
//! [`super::model_map::all_models`] 查表,内置行在前、热追加行在后。
//!
//! **只能追加,不能改内置行**:id / kiro_model 与内置表撞名一律拒收。每条必须 `like` 一条内置行,
//! 没写的行为字段从它继承(签名方式、分词器、effort 路径、两个 5.5 系开关)—— 这些字段只能靠
//! 真号实测得出,配置里不开放,免得「填错值、编译照过、线上静默失效」。
//!
//! **存储**:行里的字符串字段是 `&'static str`(派生点到处借用),所以生效时把新行 `Box::leak`
//! 成 `'static`,再在 `RwLock` 里换掉切片引用。读侧拿到的是 `'static` 切片,锁只持有一次拷贝
//! 引用的瞬间,不分配。**只在内容真的变了时才泄漏一份**(与上次收到的设置逐字段比较),30s 轮询
//! 本身不泄漏;一份几百字节,运维改一次漏一次,可忽略。
//!
//! 校验 [`validate_extra_models`] 是写侧 admin 与读侧 worker 共用的同一套规则:写侧拦住,面板就
//! 不会显示一份 worker 实际拒收的表。

use std::sync::{Mutex, RwLock};

use gw_core::config::KiroExtraModelSpec;

use super::model_map::{KiroModel, KIRO_MODELS};
use crate::anthropic_types::VALID_EFFORTS;

/// 一次最多追加多少条(防手滑粘进一大坨)。
const MAX_EXTRA_MODELS: usize = 32;
/// id / kiro_model / 展示名的长度上限。
const MAX_NAME_LEN: usize = 64;
/// 上下文窗口上限(1000 万 token,远大于任何现存模型)。
const MAX_CONTEXT_WINDOW: i32 = 10_000_000;

/// 当前生效的热追加行。初值空切片。
static EXTRA: RwLock<&'static [KiroModel]> = RwLock::new(&[]);
/// 上次**收到**的设置(无论生效还是被拒)及其拒收原因。相同则直接跳过:不重复泄漏,也不每 30s
/// 刷一遍告警;但被拒的那份每轮仍报 [`ApplyOutcome::Rejected`],面板上的同步错误不会自己消失。
static LAST_OFFERED: Mutex<Option<(Vec<KiroExtraModelSpec>, Option<String>)>> = Mutex::new(None);

/// 一次热应用的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// 换上了新表。
    Applied,
    /// 与上次收到的相同且上次已生效,未动。
    Unchanged,
    /// 整组拒收、沿用旧表。`fresh` = 这份设置是第一次收到(调用方只在此时打日志)。
    Rejected { reason: String, fresh: bool },
}

/// 当前生效的热追加行(`'static`,可随意借用)。
pub(crate) fn extra_rows() -> &'static [KiroModel] {
    *EXTRA.read().unwrap_or_else(|e| e.into_inner())
}

/// 当前生效的热追加行的对外名(`/health` 回显用)。
pub fn active_extra_ids() -> Vec<String> {
    extra_rows().iter().map(|m| m.advertised_id.to_string()).collect()
}

/// 热应用一份设置,见 [`ApplyOutcome`]。
pub fn set_extra_models(specs: &[KiroExtraModelSpec]) -> ApplyOutcome {
    let mut last = LAST_OFFERED.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((prev, prev_err)) = last.as_ref() {
        if prev.as_slice() == specs {
            return match prev_err {
                None => ApplyOutcome::Unchanged,
                Some(reason) => ApplyOutcome::Rejected { reason: reason.clone(), fresh: false },
            };
        }
    }
    if let Err(reason) = validate_extra_models(specs) {
        *last = Some((specs.to_vec(), Some(reason.clone())));
        return ApplyOutcome::Rejected { reason, fresh: true };
    }
    let rows: Vec<KiroModel> = specs.iter().map(materialize).collect();
    let leaked: &'static [KiroModel] = Box::leak(rows.into_boxed_slice());
    *EXTRA.write().unwrap_or_else(|e| e.into_inner()) = leaked;
    *last = Some((specs.to_vec(), None));
    ApplyOutcome::Applied
}

/// 整组校验(写侧 admin、读侧 worker 共用)。任何一条不合法即返回带条目序号的错误。
pub fn validate_extra_models(specs: &[KiroExtraModelSpec]) -> Result<(), String> {
    if specs.len() > MAX_EXTRA_MODELS {
        return Err(format!("最多 {MAX_EXTRA_MODELS} 条,收到 {} 条", specs.len()));
    }
    let mut seen_ids: Vec<&str> = Vec::with_capacity(specs.len());
    let mut seen_kiro: Vec<&str> = Vec::with_capacity(specs.len());
    for (i, s) in specs.iter().enumerate() {
        let at = |msg: String| format!("第 {} 条({}):{msg}", i + 1, s.id);
        check_id(&s.id).map_err(at)?;
        check_kiro_model(&s.kiro_model).map_err(at)?;
        if seen_ids.contains(&s.id.as_str()) {
            return Err(at("id 重复".into()));
        }
        if seen_kiro.contains(&s.kiro_model.as_str()) {
            return Err(at(format!("kiro_model {} 重复", s.kiro_model)));
        }
        seen_ids.push(&s.id);
        seen_kiro.push(&s.kiro_model);
        let like = like_row(&s.like).ok_or_else(|| at(format!("like {} 不是内置模型的对外名", s.like)))?;
        for (field, v) in [("display_name", &s.display_name), ("identity_short", &s.identity_short)] {
            if let Some(v) = v {
                check_label(v).map_err(|e| at(format!("{field} {e}")))?;
            }
        }
        if let Some(w) = s.context_window {
            if !(1..=MAX_CONTEXT_WINDOW).contains(&w) {
                return Err(at(format!("context_window 须在 1–{MAX_CONTEXT_WINDOW} 之间,收到 {w}")));
            }
        }
        resolve_efforts(s, like).map_err(at)?;
    }
    Ok(())
}

fn like_row(like: &str) -> Option<&'static KiroModel> {
    KIRO_MODELS.iter().find(|m| m.advertised_id == like)
}

fn is_name_char(b: u8) -> bool {
    b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-'
}

fn check_id(id: &str) -> Result<(), String> {
    if id.is_empty() || id.len() > MAX_NAME_LEN || !id.bytes().all(is_name_char) {
        return Err(format!("id 只能用小写字母、数字、`.`、`-`,长度 1–{MAX_NAME_LEN}"));
    }
    // 这两种后缀由 resolve_base 统一剥掉,带着它们的 id 永远匹配不上。
    if id.ends_with("-thinking") || id.ends_with("-reasoning") {
        return Err("id 不能以 -thinking 或 -reasoning 结尾(Thinking 变体自动派生)".into());
    }
    if id.rsplit('-').next().is_some_and(|t| t.len() == 8 && t.bytes().all(|b| b.is_ascii_digit())) {
        return Err("id 不能以 8 位日期结尾(日期段会被剥掉,永远匹配不上)".into());
    }
    if KIRO_MODELS.iter().any(|m| m.advertised_id == id || m.dated_alias == Some(id)) {
        return Err("与内置模型撞名(热追加只能新增,不能改内置行)".into());
    }
    Ok(())
}

fn check_kiro_model(kiro_model: &str) -> Result<(), String> {
    if kiro_model.is_empty() || kiro_model.len() > MAX_NAME_LEN || !kiro_model.bytes().all(is_name_char) {
        return Err(format!("kiro_model 只能用小写字母、数字、`.`、`-`,长度 1–{MAX_NAME_LEN}"));
    }
    // 签名门控、分词器按 kiro_model 反查表,两行同名会让后一行永远查不到。
    if KIRO_MODELS.iter().any(|m| m.kiro_model == kiro_model) {
        return Err(format!("kiro_model {kiro_model} 已在内置表里"));
    }
    Ok(())
}

/// 展示名 / 身份短名:非空、不过长、不含控制字符(身份短名会进系统提示的身份行)。
fn check_label(v: &str) -> Result<(), String> {
    let t = v.trim();
    if t.is_empty() || t.len() > MAX_NAME_LEN || t.chars().any(char::is_control) {
        return Err(format!("须非空、不超过 {MAX_NAME_LEN} 字节、不含换行等控制字符"));
    }
    Ok(())
}

/// 按规则定出档位表与默认档(校验与生成共用,保证两边口径一致)。
fn resolve_efforts(
    s: &KiroExtraModelSpec,
    like: &'static KiroModel,
) -> Result<(Vec<&'static str>, Option<&'static str>), String> {
    let levels: Vec<&'static str> = match &s.effort_levels {
        None => like.effort_levels.to_vec(),
        Some(raw) => {
            let mut out: Vec<&'static str> = Vec::with_capacity(raw.len());
            for lv in raw {
                let canon = VALID_EFFORTS.iter().copied().find(|v| *v == lv.as_str()).ok_or_else(|| {
                    format!("effort_levels 里的 {lv:?} 不认识(只能是 {})", VALID_EFFORTS.join(" / "))
                })?;
                if out.last().is_some_and(|prev| rank(prev) >= rank(canon)) {
                    return Err("effort_levels 须按 low→max 升序且不重复".into());
                }
                out.push(canon);
            }
            out
        }
    };
    let default = match &s.default_effort {
        Some(d) => {
            let hit = levels.iter().copied().find(|v| *v == d.as_str());
            if hit.is_none() {
                return Err(format!("default_effort {d:?} 不在档位表 {levels:?} 里"));
            }
            hit
        }
        None => like
            .default_effort
            .filter(|d| levels.contains(d))
            .or_else(|| levels.first().copied()),
    };
    Ok((levels, default))
}

fn rank(level: &str) -> usize {
    VALID_EFFORTS.iter().position(|v| *v == level).unwrap_or(usize::MAX)
}

fn leak_str(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

/// 已校验过的 spec → 一条 `'static` 行。只在 [`set_extra_models`] 换表时调用。
fn materialize(s: &KiroExtraModelSpec) -> KiroModel {
    let like = like_row(&s.like).expect("validate_extra_models 已确认 like 存在");
    let (levels, default) = resolve_efforts(s, like).expect("validate_extra_models 已确认档位合法");
    let display = s.display_name.as_deref().map(str::trim).unwrap_or(&s.id);
    let short = match s.identity_short.as_deref() {
        Some(v) => v.trim().to_string(),
        None => display.strip_prefix("Claude ").unwrap_or(display).to_string(),
    };
    KiroModel {
        advertised_id: leak_str(&s.id),
        display_name: leak_str(display),
        kiro_model: leak_str(&s.kiro_model),
        identity_short: leak_str(&short),
        context_window: s.context_window.unwrap_or(like.context_window),
        supports_thinking: like.supports_thinking,
        dated_alias: None,
        effort_levels: Box::leak(levels.into_boxed_slice()),
        default_effort: default,
        signature_id: like.signature_id,
        structured_effort_only: like.structured_effort_only,
        rewrite_visible_thinking_directive: like.rewrite_visible_thinking_directive,
        tokenizer: like.tokenizer,
        effort_schema: like.effort_schema,
        pro_only: s.pro_only,
    }
}

/// 测试专用:热追加是进程全局状态,改它的测试与「数公告条数」的测试要串行。
#[cfg(test)]
pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());

/// 测试专用:清空热追加(同时清掉「上次收到」,下一次 set 一定会生效)。
#[cfg(test)]
pub(crate) fn reset_for_test() {
    *LAST_OFFERED.lock().unwrap_or_else(|e| e.into_inner()) = None;
    *EXTRA.write().unwrap_or_else(|e| e.into_inner()) = &[];
}

#[cfg(test)]
#[path = "extra_models_tests.rs"]
mod tests;
