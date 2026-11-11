//! dario 线的报文环境标准化端到端:模拟管理器 + 模拟该号 dario 进程,看到达进程的报文。
//! 单独一个测试二进制(envnorm 开关是进程级全局)。
use axum::routing::{get, post};
use futures::StreamExt;
use gw_core::account::Account;
use gw_core::config::EnvNormalizeConfig;
use gw_core::provider::{CallCtx, ChatRequest, Provider};
use gw_dario::{DarioConfig, DarioProvider};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

async fn spawn_stack() -> (String, Arc<Mutex<Vec<serde_json::Value>>>) {
    let seen: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let seen2 = seen.clone();
    let dario = axum::Router::new().route("/v1/messages", post(move |b: axum::Json<serde_json::Value>| {
        let seen = seen2.clone();
        async move {
            seen.lock().unwrap().push(b.0);
            ([("content-type", "text/event-stream")], "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n")
        }
    }));
    let dl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dport = dl.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(dl, dario).await.unwrap() });
    let sup = axum::Router::new()
        .route("/accounts/{id}", get(|| async { axum::Json(serde_json::json!({"rev": "rt-x", "port": 1, "proxy": null, "needsRelogin": false})) }))
        .route("/accounts/{id}/ensure", post(move || async move { axum::Json(serde_json::json!({"port": dport})) }));
    let sl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let saddr = sl.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(sl, sup).await.unwrap() });
    (format!("http://{saddr}"), seen)
}

fn account(id: &str, persona: &str) -> Arc<Account> {
    let mut e = BTreeMap::new();
    e.insert("dario_rev".into(), serde_json::json!("rt-x"));
    e.insert("env_persona".into(), serde_json::json!(persona));
    Arc::new(Account { account_id: id.into(), provider: "claude-dario".into(), max_concurrency: 1, disabled: false, created_at: 0, extra: e })
}

fn request() -> ChatRequest {
    ChatRequest::from_anthropic_body(serde_json::json!({
        "model": "claude-opus-4-8", "max_tokens": 8,
        "system": [{"type": "text", "text": "# Environment\n - Platform: win32\n - OS Version: Windows 11 Home China 10.0.22631\n\ngitStatus: x\nGit user: 张三"}],
        "messages": [{"role": "user", "content": "<omo-env>\n  Timezone: Asia/Shanghai\n  Locale: zh-CN\n</omo-env>\nhi"}],
    }))
}

async fn send(p: &DarioProvider, acct: Arc<Account>) {
    let ctx = CallCtx { account: acct, session_id: "s".into(), cache_key: String::new() };
    let items: Vec<_> = p.chat(request(), &ctx).await.unwrap().collect().await;
    assert!(items.iter().all(|i| i.is_ok()));
}

#[tokio::test]
async fn dario_line_sends_persona_normalized_body_only_when_enabled() {
    let (sup_url, seen) = spawn_stack().await;
    let p = DarioProvider::new(DarioConfig { sidecar_url: sup_url, api_key: "k".into(), ..Default::default() });

    // 关:原样。
    send(&p, account("a1", "p1")).await;
    let off = seen.lock().unwrap().pop().unwrap();
    assert!(off.to_string().contains("Windows 11 Home China") && off.to_string().contains("Asia/Shanghai"));

    gw_core::envnorm::set_config(&EnvNormalizeConfig { providers: vec!["claude-dario".into()], ..EnvNormalizeConfig::default() }).unwrap();
    send(&p, account("a1", "p1")).await;
    send(&p, account("a2", "p2")).await;
    let bodies = seen.lock().unwrap().clone();
    let (b1, b2) = (bodies[0].to_string(), bodies[1].to_string());
    for b in [&b1, &b2] {
        for gone in ["Home China", "Asia/Shanghai", "zh-CN", "张三"] {
            assert!(!b.contains(gone), "{gone} 应已替换: {b}");
        }
        assert!(b.contains("Platform: win32"), "平台保留");
        assert!(b.contains("Timezone: Asia/Singapore"));
    }
    assert!(b1.contains("Git user: Alex Morgan") && b1.contains("Windows 11 Pro 10.0.26100"), "{b1}");
    assert!(b2.contains("Git user: Jordan Lee") && b2.contains("Windows 11 Home 10.0.26100"), "换号即换人设: {b2}");
    gw_core::envnorm::set_config(&EnvNormalizeConfig::default()).unwrap();
}
