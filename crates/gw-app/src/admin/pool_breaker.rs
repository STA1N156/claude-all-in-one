//! 号池熔断的管理接口:读状态、手动恢复。
//!
//! 熔断由 Kiro worker 判定,写进 `settings` 表的 `pool_breaker_state`;恢复由这里写
//! `pool_breaker_reset`(两个键分开,见 `worker::pool_breaker` 模块文档)。worker 下一轮同步
//! (≤30s)看到恢复时间后解除熔断并清空计数窗口。
//!
//! 状态同时看库和各 worker 的 `/health`:熔断先在 worker 内存生效、最多 30 秒后才落库,
//! 落库一直失败时库里也不会有 —— 只看库的话,后台既看不到横幅、也点不了恢复,而 worker
//! 正在挡掉所有请求(审查 11-09)。

use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};

use super::{internal_error, AdminState};
use crate::worker::pool_breaker::{load_stored, now_ms, write_reset, Event, StoredState};

pub fn router() -> Router<AdminState> {
    Router::new()
        .route("/pool-breaker", get(status))
        .route("/pool-breaker/reset", post(reset))
}

/// 一个 worker 内存里的熔断(`/health` 的 `pool_breaker.tripped`)。
#[derive(Debug, Clone, PartialEq)]
struct LiveTrip {
    at: i64,
    events: Vec<Event>,
}

fn live_trip_of(health: &serde_json::Value) -> Option<LiveTrip> {
    let tripped = health.get("pool_breaker")?.get("tripped")?;
    let at = tripped.get("at")?.as_i64()?;
    let events = serde_json::from_value(tripped.get("events")?.clone()).unwrap_or_default();
    Some(LiveTrip { at, events })
}

async fn live_trips(st: &AdminState) -> Vec<LiveTrip> {
    let mut out = Vec::new();
    for w in st.workers.iter() {
        let url = format!("http://{}/health", w.listen);
        let Ok(resp) = st.http.get(&url).send().await else { continue };
        let Ok(health) = resp.json::<serde_json::Value>().await else { continue };
        if let Some(trip) = live_trip_of(&health) {
            out.push(trip);
        }
    }
    out
}

/// 库里的视图 + worker 内存里的熔断合成一份。worker 内存里的熔断只有在晚于上次恢复时才算数
/// (恢复刚写进库、worker 还没同步到的那 30 秒里,不能把它又显示成熔断)。
fn view(doc: &StoredState, live: &[LiveTrip]) -> serde_json::Value {
    let reset_at = doc.reset_at.unwrap_or(i64::MIN);
    let live_active = live.iter().filter(|t| t.at > reset_at).min_by_key(|t| t.at);
    let (tripped, tripped_at, events) = if doc.is_tripped() {
        (true, doc.tripped_at, doc.events.clone())
    } else if let Some(t) = live_active {
        (true, Some(t.at), t.events.clone())
    } else {
        (false, doc.tripped_at, doc.events.clone())
    };
    serde_json::json!({
        "tripped": tripped,
        "tripped_at": tripped_at,
        "events": events,
        "reset_at": doc.reset_at,
        // 熔断还只在 worker 内存里、没进库(刚触发 ≤30 秒,或写库一直失败)。
        "persisted": !tripped || doc.is_tripped(),
    })
}

async fn status(State(st): State<AdminState>) -> axum::response::Response {
    let doc = match load_stored(&st.store) {
        Ok(doc) => doc,
        Err(e) => return internal_error(e),
    };
    let live = live_trips(&st).await;
    Json(view(&doc, &live)).into_response()
}

/// 恢复不要求库里先有熔断:worker 内存里的熔断同样靠这个时间解除。没熔断时点一下也无害
/// (只会清掉下一次熔断之前的旧记录的「熔断中」标记,而那本来就不是熔断中)。
async fn reset(State(st): State<AdminState>) -> axum::response::Response {
    if let Err(e) = write_reset(&st.store, now_ms()) {
        return internal_error(e);
    }
    let doc = match load_stored(&st.store) {
        Ok(doc) => doc,
        Err(e) => return internal_error(e),
    };
    tracing::warn!(tripped_at = ?doc.tripped_at, events = doc.events.len(), "号池熔断:管理员手动恢复");
    Json(view(&doc, &[])).into_response()
}

#[cfg(test)]
mod tests {
    use super::super::tests_support::{app, req};
    use super::*;
    use crate::worker::pool_breaker::{EventKind, STATE_KEY};
    use axum::body::to_bytes;
    use axum::http::StatusCode;
    use tower::ServiceExt;

    async fn body_json(resp: axum::response::Response) -> serde_json::Value {
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn status_and_reset() {
        let (app, store) = app();
        let resp = app.clone().oneshot(req("GET", "/pool-breaker", None)).await.unwrap();
        assert_eq!(body_json(resp).await["tripped"], false);

        let at = now_ms() - 1000;
        let tripped = StoredState {
            tripped_at: Some(at),
            events: vec![Event { account: "a".into(), kind: EventKind::Suspended, at }],
            reset_at: None,
        };
        store.upsert_kv(STATE_KEY, &serde_json::to_string(&tripped).unwrap()).unwrap();
        let v = body_json(app.clone().oneshot(req("GET", "/pool-breaker", None)).await.unwrap()).await;
        assert_eq!(v["tripped"], true);
        assert_eq!(v["events"][0]["kind"], "suspended");

        let resp = app.clone().oneshot(req("POST", "/pool-breaker/reset", None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let saved = load_stored(&store).unwrap();
        assert!(!saved.is_tripped());
        assert_eq!(saved.tripped_at, Some(at), "保留上次熔断信息供展示");
        let raw: StoredState = serde_json::from_str(&store.get_kv(STATE_KEY).unwrap().unwrap()).unwrap();
        assert!(raw.reset_at.is_none(), "恢复只写 RESET_KEY,不碰 worker 写的那个键");
    }

    #[test]
    fn live_trip_in_worker_memory_shows_until_reset() {
        let health = serde_json::json!({"pool_breaker": {"tripped": {"at": 5000, "events": [
            {"account": "a", "kind": "downgraded", "at": 4000}]}}});
        let live = vec![live_trip_of(&health).unwrap()];
        assert!(live_trip_of(&serde_json::json!({"pool_breaker": {"tripped": null}})).is_none());
        // 库里没有(还没落库 / 写库失败):照样显示熔断,并标出没落库。
        let v = view(&StoredState::default(), &live);
        assert_eq!(v["tripped"], true);
        assert_eq!(v["persisted"], false);
        assert_eq!(v["events"][0]["account"], "a");
        // 恢复时间晚于它:worker 还没同步到的那 30 秒里不再显示成熔断。
        let reset = StoredState { reset_at: Some(6000), ..StoredState::default() };
        assert_eq!(view(&reset, &live)["tripped"], false);
    }

    #[tokio::test]
    async fn requires_admin_token() {
        let (app, _store) = app();
        let unauth = axum::http::Request::builder()
            .method("POST")
            .uri("/pool-breaker/reset")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = app.oneshot(unauth).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}
