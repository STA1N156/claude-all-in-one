//! 子机号的路由:交接与对话都经 hub 本地口 `/n/<节点>/...`;hub 的标记与子机 ensure 失败的映射;
//! 选号过滤跟着 hub `/status` 走。这里用一个假 hub 模拟子机一侧。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use futures::StreamExt;
use gw_core::account::Account;
use gw_core::error::UpstreamErrorKind;
use gw_core::provider::{CallCtx, ChatRequest, Provider};
use gw_dario::{DarioConfig, DarioProvider};

#[derive(Default)]
struct HubState {
    calls: Mutex<Vec<String>>,
    /// 对话路径的行为:"ok" / "offline" / "ensure409"。
    chat_mode: Mutex<String>,
    online: Mutex<bool>,
}
type S = Arc<HubState>;

async fn sup_get(State(s): State<S>, Path((node, id)): Path<(String, String)>) -> axum::response::Response {
    s.calls.lock().unwrap().push(format!("GET {node} {id}"));
    StatusCode::NOT_FOUND.into_response()
}

async fn sup_put(State(s): State<S>, h: HeaderMap, Path((node, id)): Path<(String, String)>, axum::Json(b): axum::Json<serde_json::Value>) -> impl IntoResponse {
    assert_eq!(h.get("x-api-key").unwrap(), "hub-key", "worker 用 dario.api_key 访问 hub 本地口");
    s.calls.lock().unwrap().push(format!("PUT {node} {id}"));
    axum::Json(serde_json::json!({"rev": b["rev"], "port": 18801, "proxy": null, "needsRelogin": false,
        "deviceId": b["deviceId"], "accountUuid": b["accountUuid"]}))
}

async fn chat(State(s): State<S>, Path((node, id)): Path<(String, String)>) -> axum::response::Response {
    s.calls.lock().unwrap().push(format!("CHAT {node} {id}"));
    let mode = s.chat_mode.lock().unwrap().clone();
    match mode.as_str() {
        "offline" => (StatusCode::SERVICE_UNAVAILABLE, [(gw_core::node::HDR_NODE, "offline")], "子机当前离线").into_response(),
        "lost" => (StatusCode::BAD_GATEWAY, [(gw_core::node::HDR_NODE, "lost")], "中断").into_response(),
        "ensure409" => (StatusCode::CONFLICT, [(gw_core::node::HDR_STAGE, "ensure")], "{\"error\":\"relogin\"}").into_response(),
        _ => {
            let sse = concat!(
                "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n\n",
                "event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":2}}\n\n",
                "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
            );
            ([("content-type", "text/event-stream")], sse).into_response()
        }
    }
}

async fn status(State(s): State<S>) -> impl IntoResponse {
    let online = *s.online.lock().unwrap();
    axum::Json(serde_json::json!({"nodes": [{"id": "n-1", "online": online, "disabled": false}]}))
}

async fn spawn_hub() -> (S, String) {
    let s: S = Arc::new(HubState::default());
    *s.chat_mode.lock().unwrap() = "ok".into();
    let app = axum::Router::new()
        .route("/status", get(status))
        .route("/n/{node}/sup/accounts/{id}", get(sup_get).put(sup_put))
        .route("/n/{node}/a/{id}/v1/messages", post(chat))
        .with_state(s.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (s, url)
}

fn node_account(kv: &[(&str, &str)]) -> Arc<Account> {
    let mut e = BTreeMap::new();
    e.insert("dario_node".to_string(), serde_json::json!("n-1"));
    for (k, v) in kv {
        e.insert(k.to_string(), serde_json::json!(v));
    }
    Arc::new(Account { account_id: "a@b.com".into(), provider: "claude-dario".into(), max_concurrency: 2, disabled: false, created_at: 0, extra: e })
}

fn provider(hub: &str) -> DarioProvider {
    DarioProvider::new(DarioConfig { sidecar_url: "http://127.0.0.1:9".into(), api_key: "hub-key".into(), hub_url: hub.into(), ..Default::default() })
}

async fn send(p: &DarioProvider, a: Arc<Account>) -> Result<Vec<gw_core::provider::StreamItem>, gw_core::error::UpstreamError> {
    let ctx = CallCtx { account: a, session_id: "s".into(), cache_key: String::new() };
    let req = ChatRequest::from_anthropic_body(serde_json::json!({"model": "claude-opus-4-8", "max_tokens": 8, "messages": [{"role": "user", "content": "hi"}]}));
    let stream = p.chat(req, &ctx).await?;
    Ok(stream.collect::<Vec<_>>().await.into_iter().map(|r| r.unwrap()).collect())
}

#[tokio::test]
async fn node_account_hands_over_and_chats_through_hub() {
    let (s, hub) = spawn_hub().await;
    let p = provider(&hub);
    let items = send(&p, node_account(&[("refresh_token", "rt-1"), ("device_id", "d"), ("account_uuid", "u")])).await.unwrap();
    assert!(!items.is_empty());
    let calls = s.calls.lock().unwrap().clone();
    assert_eq!(calls, vec!["GET n-1 a@b.com", "PUT n-1 a@b.com", "CHAT n-1 a@b.com"], "不走本机管理器、不单独 ensure");
    let ups = p.poll_token_updates();
    assert_eq!(ups.len(), 1, "子机交接的落库增量也要被 worker 取走");
    assert_eq!(ups[0].1["refresh_token"], "");
}

#[tokio::test]
async fn hub_marks_and_ensure_stage_are_mapped() {
    let (s, hub) = spawn_hub().await;
    let p = provider(&hub);
    let a = node_account(&[("refresh_token", "rt-1"), ("device_id", "d"), ("account_uuid", "u")]);
    send(&p, a.clone()).await.unwrap();

    *s.chat_mode.lock().unwrap() = "offline".into();
    let e = send(&p, a.clone()).await.unwrap_err();
    assert_eq!(e.kind, UpstreamErrorKind::RateLimited, "离线:冷却该号、换号");
    assert!(!e.delivery.may_have_executed());

    *s.chat_mode.lock().unwrap() = "lost".into();
    let e = send(&p, a.clone()).await.unwrap_err();
    assert!(e.delivery.may_have_executed(), "途中断开:可能已执行");

    *s.chat_mode.lock().unwrap() = "ensure409".into();
    let e = send(&p, a.clone()).await.unwrap_err();
    assert_eq!(e.kind, UpstreamErrorKind::TokenInvalid, "子机代为 ensure 的 409 = 需要重新登录");
}

#[tokio::test]
async fn node_account_with_proxy_is_rejected_before_any_call() {
    let (s, hub) = spawn_hub().await;
    let p = provider(&hub);
    let e = send(&p, node_account(&[("refresh_token", "rt"), ("proxy", "http://p:1")])).await.unwrap_err();
    assert_eq!(e.kind, UpstreamErrorKind::BadRequest);
    assert!(s.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn routable_follows_hub_status() {
    let (s, hub) = spawn_hub().await;
    *s.online.lock().unwrap() = true;
    let p = provider(&hub);
    let a = node_account(&[]);
    let local = Account { extra: BTreeMap::new(), ..(*a).clone() };
    assert!(p.account_routable(&local), "本机号恒可用");
    let mut ok = false;
    for _ in 0..40 {
        if p.account_routable(&a) {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(ok, "hub 报在线后子机号可用");
    *s.online.lock().unwrap() = false;
    let mut off = false;
    for _ in 0..40 {
        if !p.account_routable(&a) {
            off = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(off, "hub 报离线后几秒内被排除");
}
