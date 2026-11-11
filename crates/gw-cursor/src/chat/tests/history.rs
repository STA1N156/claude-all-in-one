//! chat 测试:消息转换、续轮判定与折叠。

use super::*;

/// **2026-08-17 生产事故的回归锁**:真实报文形态 —— 中转在每条用户消息之后
/// 追一条 `role:"system"` 的预算计数器。分流前末轮是那条计数器,
/// CLI 驱动就把它当 prompt 发出去了(用户看到「你这条消息是空的」)。
#[test]
fn 分流器_预算计数器不再顶掉用户末轮() {
    let mut body = json!({"messages": [
        {"role": "user", "content": [{"type": "text", "text": "# 项目 X 交接:第四阶段启动"}]},
        {"role": "system", "content": [{"type": "text", "text": "<total_tokens>15000000 tokens left</total_tokens>"}]},
    ]});
    // 分流前 + 旧的取法(`turns.last()`):末轮是注入的计数器,发出去的就是它。
    assert_eq!(
        to_turns(&body).last().unwrap().text,
        "<total_tokens>15000000 tokens left</total_tokens>",
        "这就是事故现场:用户打的那段被计数器整条顶掉"
    );
    route_system_role_messages(&mut body);
    // 分流后:计数器被丢弃,本轮(末轮)是用户真正打的那段。
    assert_eq!(body["messages"].as_array().unwrap().len(), 1);
    assert_eq!(to_turns(&body).last().unwrap().text, "# 项目 X 交接:第四阶段启动");
}

/// 同一条尾巴还会掐死工具回路:末条是 system 时认不出「工具结果轮」,
/// 挂流续跑与续轮都接不上(当年 CLI 驱动的表现是模型报「MCP 读取被中断了」)。
#[test]
fn 分流器_工具回路的接续不再被尾巴掐死() {
    let mut body = json!({"messages": [
        {"role": "user", "content": "读一下 STATUS.md"},
        {"role": "assistant", "content": [{"type": "tool_use", "id": "toolu_a", "name": "Read", "input": {}}]},
        {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_a", "content": "文件内容"}]},
        {"role": "system", "content": "<total_tokens>14990000 tokens left</total_tokens>"},
    ]});
    assert!(!last_user_is_tool_result_only(&body), "分流前:末条是计数器,认不出工具结果轮");
    route_system_role_messages(&mut body);
    assert!(last_user_is_tool_result_only(&body), "分流后:末条是工具结果,能接续");
}

/// 增量模式:只发本轮,历史交给服务端 —— 折叠形态那段假对话记录必须消失。
#[test]
fn 增量历史只发本轮_不重传历史() {
    let turns = vec![
        Turn {
            text: "第一个问题".into(),
            is_user: true,
        },
        Turn {
            text: "第一个回答".into(),
            is_user: false,
        },
        Turn {
            text: "第二个问题".into(),
            is_user: true,
        },
    ];
    let out = delta_history(&turns, None);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].text, "第二个问题");
    assert!(out[0].is_user);
    // 关键:历史与那对包装标签一个字都不能出现(它们正是「同一段对话喂两遍」的来源)。
    for leak in ["第一个问题", "第一个回答", "<conversation_history>"] {
        assert!(!out[0].text.contains(leak), "增量模式不该重传 {leak}");
    }
    // 对照:折叠模式仍然把历史整段带上(两条路都要保持各自的形状)。
    let folded = fold_history(&turns, None);
    assert!(folded[0].text.contains("<conversation_history>"));
    assert!(folded[0].text.contains("第一个回答"));
}

/// 尾随 assistant 是 prefill(让模型续写),丢掉会改变语义 —— 增量模式也要带上。
#[test]
fn 增量历史保留尾随的_prefill() {
    let turns = vec![
        Turn {
            text: "历史问题".into(),
            is_user: true,
        },
        Turn {
            text: "历史回答".into(),
            is_user: false,
        },
        Turn {
            text: "本轮问题".into(),
            is_user: true,
        },
        Turn {
            text: "答案是".into(),
            is_user: false,
        },
    ];
    let out = delta_history(&turns, None);
    assert_eq!(out[0].text, "本轮问题\n\n答案是");
    assert!(!out[0].text.contains("历史问题"));
}

/// 工具回路那句提醒两条路共用同一份措辞(治的是「本轮只有工具返回、没有问题」,
/// 与历史在谁手里无关)。
#[test]
fn 增量历史照样补工具回路提醒() {
    let _g = NUDGE_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let turns = vec![Turn {
        text: "[工具返回]42".into(),
        is_user: true,
    }];
    let d = delta_history(&turns, Some("帮我算一下"));
    let f = fold_history(&turns, Some("帮我算一下"));
    assert!(d[0].text.contains("重复调用已经返回过结果的工具"));
    assert!(d[0].text.ends_with("用户的请求是:帮我算一下"));
    // 单轮时两条路应当给出完全一样的东西(没有历史可省)。
    assert_eq!(d[0].text, f[0].text);
}

/// 畸形输入(没有任何 user 轮)不得产出空 turns:`build_frame0` 对空是 assert,
/// 而空请求上游只回心跳。
#[test]
fn 没有_user_轮时退回折叠而不是返回空() {
    let turns = vec![Turn {
        text: "只有助手".into(),
        is_user: false,
    }];
    let out = delta_history(&turns, None);
    assert!(!out.is_empty());
    assert!(out[0].text.contains("只有助手"));
}

/// **开关默认关**:正确性押在「服务端真存住了历史」这个未验证假设上,
/// 所以代码先上、保持关闭,等真号实验验过再热开。
#[test]
fn 增量模式默认关闭() {
    assert!(
        !delta_history_enabled(),
        "默认必须是关的 —— 押错的代价是模型静默丢上下文"
    );
}

/// 工具回路第 2 轮:折叠后上游看到的「当前用户消息」。
///
/// 这个用例存在的理由是一次真实事故:grok 在 opencode 里对同一个文件连调
/// 9 次 `read`。原因看这里打印出来的东西 —— 折叠把 tool_result 变成了
/// **本轮用户消息的全部内容**,模型收到的是一段没有问题的工具返回。
#[test]
fn tool_loop_second_round_leaves_a_question_in_the_current_message() {
    let _g = NUDGE_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let body = json!({
      "tools": [{"name":"read","input_schema":{"type":"object"}}],
      "messages": [
        {"role":"user","content":"读一下 data.txt 然后告诉我里面有什么"},
        {"role":"assistant","content":[
            {"type":"tool_use","id":"call-abc-0\nfc_abc_0","name":"read",
             "input":{"filePath":"/tmp/data.txt"}}]},
        {"role":"user","content":[
            {"type":"tool_result","tool_use_id":"call-abc-0\nfc_abc_0",
             "content":[{"type":"text","text":"gamma\ndelta\n"}]}]}
      ]
    });
    assert!(
        last_user_is_tool_result_only(&body),
        "这一轮应被认成工具回路中间轮"
    );
    let task = latest_real_user_request(&body);
    assert_eq!(
        task.as_deref(),
        Some("读一下 data.txt 然后告诉我里面有什么")
    );

    let turns = fold_history(&to_turns(&body), task.as_deref());
    assert_eq!(turns.len(), 1);
    // 原始问题必须还在**当前**消息里,不能只留在 <conversation_history> 里 ——
    // 否则本轮的用户消息只是一段裸的工具返回,模型会重新调一遍工具。
    let current = turns[0]
        .text
        .rsplit("</conversation_history>")
        .next()
        .unwrap_or("")
        .to_string();
    assert!(
        current.contains("告诉我里面有什么"),
        "本轮用户消息里没有任何问题,只有工具返回:\n{current}"
    );
    // 工具结果本身当然也要还在。
    assert!(current.contains("gamma"));
    // 而且要明确禁止重复调用 —— 这就是那 9 次 read 的直接病因。
    assert!(current.contains("重复调用已经返回过结果的工具"));
}

/// 2026-11-09「说完要做什么就停」:提醒不许再把工具说成只用来查信息、
/// 不许鼓励「信息够了就给最终回答」;要明确「下一步要动手就直接调工具」。
#[test]
fn tool_loop_nudge_pushes_action_not_final_answer() {
    let d = DEFAULT_TOOL_LOOP_NUDGE;
    assert!(!d.contains("最终回答"), "旧措辞会诱导模型写完计划就收尾");
    assert!(d.contains("直接调用对应的工具"));
    assert!(d.contains("不要只把打算做的事写成文字就结束这一轮"));
    assert!(
        d.contains("重复调用已经返回过结果的工具"),
        "治 grok 连读的半句要保留"
    );
}

/// 热配置:覆盖生效、空串回默认、过长报错且保留上一份有效值;格式框架(前置空行与
/// 「用户的请求是:」)由代码补,配置只管正文。
#[test]
fn tool_loop_nudge_hot_override() {
    let _g = NUDGE_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let turns = vec![Turn {
        text: "[工具返回]42".into(),
        is_user: true,
    }];
    let default_rev = tool_loop_nudge_rev();
    set_tool_loop_nudge("自定义提醒").unwrap();
    let f = fold_history(&turns, Some("算一下"));
    assert!(
        f[0].text.ends_with("\n\n自定义提醒\n用户的请求是:算一下"),
        "{}",
        f[0].text
    );
    assert_ne!(tool_loop_nudge_rev(), default_rev);
    // 过长:报错且沿用「自定义提醒」。
    assert!(set_tool_loop_nudge(&"长".repeat(TOOL_LOOP_NUDGE_MAX_CHARS + 1)).is_err());
    assert_eq!(&*tool_loop_nudge(), "自定义提醒");
    // 空串回内置默认。
    set_tool_loop_nudge("  ").unwrap();
    assert_eq!(&*tool_loop_nudge(), DEFAULT_TOOL_LOOP_NUDGE);
    assert_eq!(tool_loop_nudge_rev(), default_rev);
}

#[test]
fn prev_assistant_has_tool_use_结构校验() {
    // 标准工具结果轮:assistant tool_use → user tool_result ✔
    let ok = json!({"messages":[
        {"role":"user","content":"读 data.txt"},
        {"role":"assistant","content":[
            {"type":"tool_use","id":"t1","name":"read","input":{"filePath":"/tmp/x"}}]},
        {"role":"user","content":[
            {"type":"tool_result","tool_use_id":"t1","content":"内容"}]}
    ]});
    assert!(prev_assistant_has_tool_use(&ok));
    assert!(last_user_is_tool_result_only(&ok));
    // 末条是普通提问(不是结果轮)→ false
    let ask = json!({"messages":[{"role":"user","content":"你好"}]});
    assert!(!prev_assistant_has_tool_use(&ask));
    // 上一条 assistant 没有 tool_use(被编辑过)→ false,不许进 CLI 续轮
    let edited = json!({"messages":[
        {"role":"user","content":"读 data.txt"},
        {"role":"assistant","content":"我看看"},
        {"role":"user","content":[
            {"type":"tool_result","tool_use_id":"t1","content":"内容"}]}
    ]});
    assert!(!prev_assistant_has_tool_use(&edited));
    // 上一条是 user 而不是 assistant → false
    let doubled = json!({"messages":[
        {"role":"user","content":"读 data.txt"},
        {"role":"user","content":[
            {"type":"tool_result","tool_use_id":"t1","content":"内容"}]}
    ]});
    assert!(!prev_assistant_has_tool_use(&doubled));
}

#[test]
fn a_real_question_turn_is_not_treated_as_a_tool_loop() {
    // 普通提问轮不能被复述指令污染(否则每一轮都多一段废话)。
    let body = json!({"messages":[{"role":"user","content":"你好"}]});
    assert!(!last_user_is_tool_result_only(&body));
    let turns = fold_history(&to_turns(&body), None);
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].text, "你好", "单轮请求不该被加工");
    assert!(!turns[0].text.contains("<conversation_history>"));
}

#[test]
fn tool_loop_uses_the_latest_question_not_the_first() {
    // 用户改了主题:复述必须取**最近**那个问题,否则把模型带回上个话题。
    let body = json!({"messages":[
        {"role":"user","content":"先看看 a.txt"},
        {"role":"assistant","content":"好"},
        {"role":"user","content":"算了,改成统计 b.txt 的行数"},
        {"role":"assistant","content":[{"type":"tool_use","id":"c-1","name":"bash","input":{}}]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"c-1","content":"42"}]}
    ]});
    assert_eq!(
        latest_real_user_request(&body).as_deref(),
        Some("算了,改成统计 b.txt 的行数")
    );
}

/// 2026-08-13 生产事故的回归锁:tool_result **内嵌**的 image 块必须被
/// to_media 收进附件,且文本流原位出现编号一致的占位 —— 曾经 to_media 只扫
/// 顶层块、extract_text 把媒体丢给 `_ => None`,两边互相推诿,工具读的图
/// 静默消失且零日志。
#[test]
fn tool_result_nested_image_is_collected_with_placeholder() {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(b"fake-png-bytes");
    let body = json!({"messages":[
        {"role":"user","content":[
            {"type":"image","source":{"type":"base64","media_type":"image/png","data": b64}},
            {"type":"text","text":"看这张图"}
        ]},
        {"role":"assistant","content":[{"type":"tool_use","id":"c-1","name":"read","input":{}}]},
        {"role":"user","content":[
            {"type":"tool_result","tool_use_id":"c-1","content":[
                {"type":"text","text":"Image read successfully"},
                {"type":"image","source":{"type":"base64","media_type":"image/png","data": b64}}
            ]}
        ]}
    ]});
    let (images, docs, ph) = to_media(&body);
    assert_eq!(images.len(), 2, "顶层 1 张 + tool_result 内嵌 1 张");
    assert!(docs.is_empty());
    // 占位表只登记内嵌那张。编号 = images 向量下标(顶层那张是 0,内嵌是 1),
    // 与 run.rs `ImageAttachment::encode(seq)` 合成的 attach-{seq} 路径同源。
    assert_eq!(ph.len(), 1);
    assert_eq!(
        ph.get(&(2, 0, 1)).map(String::as_str),
        Some("[图片见附件 attach-1]"),
        "占位坐标 = (消息2, 顶层块0, 内嵌块1)"
    );
    // 文本流:工具返回的文字不丢,占位在原位出现。
    let turns = to_turns_with_media(&body, Some(&ph));
    assert!(turns[2].text.contains("Image read successfully"));
    assert!(turns[2].text.contains("[图片见附件 attach-1]"));
    // 不带表 = 旧行为(cache_sim 指纹等经 to_turns 的调用点):无占位,
    // 指纹口径不因本修复漂移。
    let old = to_turns(&body);
    assert!(!old[2].text.contains("附件"));
    assert!(old[2].text.contains("Image read successfully"));
    // 折叠后收进当前(最后一条)用户消息 —— run.rs 把图挂最后一条用户轮,
    // 历史折成一条,嵌套图天然随之挂上。
    let folded = fold_history(&turns, None);
    assert_eq!(folded.len(), 1);
    assert!(folded[0].text.contains("[图片见附件 attach-1]"));
}

/// tool_result 内嵌 document:收进 docs 并留 `[文档见 <path>]` 占位;
/// content 为纯字符串的 tool_result 没有媒体,一个附件都不收。
#[test]
fn tool_result_nested_document_and_string_content() {
    use base64::Engine as _;
    let pdf = base64::engine::general_purpose::STANDARD.encode(b"%PDF-1.4 fake");
    let body = json!({"messages":[
        {"role":"user","content":[
            {"type":"tool_result","tool_use_id":"c-1","content":"plain string result"},
            {"type":"tool_result","tool_use_id":"c-2","content":[
                {"type":"document","source":{"type":"base64","media_type":"application/pdf","data": pdf}}
            ]}
        ]}
    ]});
    let (images, docs, ph) = to_media(&body);
    assert!(images.is_empty());
    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0].path, "/tmp/gw-cursor/doc-0.pdf");
    assert_eq!(
        ph.get(&(0, 1, 0)).map(String::as_str),
        Some("[文档见 /tmp/gw-cursor/doc-0.pdf]")
    );
    let turns = to_turns_with_media(&body, Some(&ph));
    assert!(turns[0].text.contains("plain string result"));
    assert!(turns[0].text.contains("[文档见 /tmp/gw-cursor/doc-0.pdf]"));
}

#[test]
fn system_stays_out_of_the_conversation_turns() {
    // system 的家是 1.2.17.9.25;混进用户消息会让模型把系统指令当用户说的话。
    let body = json!({
        "system": "be brief",
        "messages": [
            {"role":"user","content":"hi"},
            {"role":"assistant","content":"yo"},
            {"role":"user","content":"more"}
        ]
    });
    let turns = to_turns(&body);
    assert_eq!(turns.len(), 3);
    assert_eq!(turns[0].text, "hi", "system 不该出现在用户轮里");
    assert!(turns.iter().all(|t| !t.text.contains("be brief")));
    assert_eq!(turns[2].text, "more");
    assert!(!turns[1].is_user);
    // 单独取得出来
    assert_eq!(extract_system(&body), "be brief");
}

#[test]
fn unsupported_content_keeps_turn_alignment() {
    // 只含 image 的一轮不能被整条丢掉,否则 user/assistant 交替会错位。
    let body = json!({"messages":[
        {"role":"user","content":[{"type":"image"}]},
        {"role":"assistant","content":"ok"}
    ]});
    let turns = to_turns(&body);
    assert_eq!(turns.len(), 2);
    assert!(turns[0].is_user);
    assert!(!turns[1].is_user);
}

/// 真实转换链路回归(审查 4.6):从 Anthropic body 走 to_turns → 指纹 →
/// peek/commit,而不是手工构造 Turn。专门防「单测手造 3 个 Turn,生产
/// fold_history 早已折成 1 个」的脱节:折叠形态里闭合标签横在中间,
/// 按折叠后内容取指纹会让第二轮整段 miss —— 所以指纹必须取折叠前的
/// 逐条消息,第二轮才能命中第一轮的逻辑前缀。
#[test]
fn 模拟缓存走真实转换链路第二轮命中() {
    let body1 = json!({
        "system": "You are a coding agent.",
        "messages": [{"role":"user","content":"帮我看看这个项目"}]
    });
    let body2 = json!({
        "system": "You are a coding agent.",
        "messages": [
            {"role":"user","content":"帮我看看这个项目"},
            {"role":"assistant","content":"好的,这是一个 Rust 工作区…"},
            {"role":"user","content":"先跑一下测试"}
        ]
    });
    // 与 chat_stream 同源的指纹构造(逐条消息 + tools + 带守卫的 system)。
    let fps_of = |body: &Value| {
        let mut sys = extract_system(body);
        let tools = to_tools(body);
        sys.push_str(&builtin_tool_guard(&tools));
        crate::cache_sim::fingerprints_from_context(&sys, &tools, &to_turns(body), est_text_tokens)
    };
    let store = crate::cache_sim::CacheSimStore::new();
    let t0 = std::time::Instant::now();
    let key = "acc-1\x1fconv-1";

    let fps1 = fps_of(&body1);
    let (r1, gen1) = store.peek_at(key, "claude-fable-5", &fps1, t0);
    assert_eq!(r1.cache_read_tokens, 0, "首轮冷启动必须 0 命中");
    assert!(store.commit_at(key, "claude-fable-5", fps1, t0, gen1));

    let fps2 = fps_of(&body2);
    let (r2, _) = store.peek_at(
        key,
        "claude-fable-5",
        &fps2,
        t0 + std::time::Duration::from_secs(30),
    );
    // 第二轮应命中:system + 第一轮 user 消息(逻辑前缀)。
    let expect_min = r1.total_tokens; // 第一轮全部被第二轮包含为前缀
    assert!(
        r2.cache_read_tokens >= expect_min,
        "第二轮应命中第一轮全部内容: got {}, expect >= {}",
        r2.cache_read_tokens,
        expect_min
    );
    assert!(
        r2.cache_read_tokens < r2.total_tokens,
        "本轮新增(assistant 回复 + 新问题)不能算命中"
    );
}
