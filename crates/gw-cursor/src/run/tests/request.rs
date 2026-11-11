//! 请求帧测试。

use super::*;

#[test]
fn body_lives_under_frame_field_1_not_at_top_level() {
    // 这条测试守的是模块头说的那跤:主体必须裹在帧 payload 的 field 1 里。
    // 若有人把 1.9 当顶层 field 9 写,这条会红。
    let bytes = f0(
        &one_user("hi"),
        &Model::new("grok-4.5"),
        "conv-1",
        "Asia/Shanghai",
        1_700_000_000_000,
        RunShape::default(),
        Phase::Opening,
    );
    let mut top = Reader::new(&bytes);
    let (f, _) = top.next().unwrap();
    assert_eq!(f, FRAME_BODY, "帧 payload 顶层只应有 field 1");
    assert!(top.next().is_none(), "顶层不应有第二个字段");

    // 模型在 body 的 field 9,不在顶层
    let body = dig(&bytes, &[FRAME_BODY]).unwrap();
    let model = dig(body, &[BODY_MODEL]).unwrap();
    assert_eq!(string_at(model, MODEL_NAME).as_deref(), Some("grok-4.5"));
    assert!(
        dig(&bytes, &[BODY_MODEL]).is_none() || dig(&bytes, &[BODY_MODEL]).unwrap() != model,
        "顶层 field 9 不应直接是模型块"
    );
}

#[test]
fn encodes_conversation_id_and_env() {
    let bytes = f0(
        &one_user("hi"),
        &Model::new("default"),
        "conv-xyz",
        "Asia/Shanghai",
        1_700_000_000_123,
        RunShape::default(),
        Phase::Continuation,
    );
    let body = dig(&bytes, &[FRAME_BODY]).unwrap();
    assert_eq!(
        string_at(body, BODY_CONVERSATION_ID).as_deref(),
        Some("conv-xyz")
    );
    assert_eq!(varint_at(body, BODY_FLAG10), Some(0));

    let env = dig(body, &[BODY_ENV]).unwrap();
    assert_eq!(varint_at(env, ENV_TIMESTAMP_MS), Some(1_700_000_000_123));
    assert_eq!(
        string_at(env, ENV_TIMEZONE).as_deref(),
        Some("Asia/Shanghai")
    );
    assert_eq!(varint_at(env, ENV_FLAG10), Some(1));
}

#[test]
fn env_client_kind_stays_ide_not_glass() {
    // 抓包实物:头是 glass,体内是 ide。防止有人「统一」掉。
    let bytes = f0(
        &one_user("hi"),
        &Model::new("default"),
        "c",
        "UTC",
        1,
        RunShape::default(),
        Phase::Continuation,
    );
    let env = dig(dig(&bytes, &[FRAME_BODY]).unwrap(), &[BODY_ENV]).unwrap();
    assert_eq!(string_at(env, ENV_CLIENT_KIND).as_deref(), Some("ide"));
    assert_eq!(crate::wire::CLIENT_TYPE, "glass", "头部才是 glass");
}

#[test]
fn message_carries_text_kind_and_rich_form() {
    let turns = vec![
        Turn {
            text: "问题".into(),
            is_user: true,
        },
        Turn {
            text: "回答".into(),
            is_user: false,
        },
    ];
    let bytes = f0(
        &turns,
        &Model::new("default"),
        "c",
        "UTC",
        1,
        RunShape::default(),
        Phase::Opening,
    );
    let body = dig(&bytes, &[FRAME_BODY]).unwrap();
    let conv = dig(body, &[BODY_CONVERSATION]).unwrap();

    // 两轮各一个 CONV_TURN
    let turns_seen: Vec<&[u8]> = Reader::new(conv)
        .filter_map(|(f, v)| match (f, v) {
            (CONV_TURN, PbValue::Len(s)) => Some(s),
            _ => None,
        })
        .collect();
    assert_eq!(turns_seen.len(), 2);

    let m0 = dig(turns_seen[0], &[TURN_MESSAGE]).unwrap();
    assert_eq!(string_at(m0, MSG_TEXT).as_deref(), Some("问题"));
    assert_eq!(varint_at(m0, MSG_KIND), Some(MSG_KIND_USER));
    let rich = string_at(m0, MSG_RICH).unwrap();
    let doc: Value = serde_json::from_str(&rich).unwrap();
    assert_eq!(doc["type"], "doc");
    assert_eq!(doc["content"][0]["content"][0]["text"], "问题");
    // uuid 每条独立
    assert_ne!(
        string_at(m0, MSG_UUID),
        string_at(dig(turns_seen[1], &[TURN_MESSAGE]).unwrap(), MSG_UUID)
    );

    let m1 = dig(turns_seen[1], &[TURN_MESSAGE]).unwrap();
    assert_eq!(varint_at(m1, MSG_KIND), Some(MSG_KIND_ASSISTANT));
}

#[test]
fn model_params_encode_as_key_value_pairs() {
    let m = Model::with_params("grok-4.5", &[("effort", "high"), ("fast", "false")]);
    let bytes = f0(
        &one_user("x"),
        &m,
        "c",
        "UTC",
        1,
        RunShape::default(),
        Phase::Opening,
    );
    let model = dig(dig(&bytes, &[FRAME_BODY]).unwrap(), &[BODY_MODEL]).unwrap();
    assert_eq!(string_at(model, MODEL_NAME).as_deref(), Some("grok-4.5"));

    let pairs: Vec<(String, String)> = Reader::new(model)
        .filter_map(|(f, v)| match (f, v) {
            (MODEL_PARAMS, PbValue::Len(s)) => Some((
                string_at(s, PARAM_KEY).unwrap_or_default(),
                string_at(s, PARAM_VAL).unwrap_or_default(),
            )),
            _ => None,
        })
        .collect();
    assert_eq!(
        pairs,
        vec![
            ("effort".to_string(), "high".to_string()),
            ("fast".to_string(), "false".to_string())
        ]
    );
}

#[test]
fn catalog_emits_one_repeated_entry_per_model() {
    // 真 IDE 每次 Run 都回传整份可用模型清单;为了不可区分,我们也发。
    let catalog = vec![
        Model::new("default"),
        Model::with_params("grok-4.5", &[("effort", "high")]),
    ];
    let bytes = build_frame0(
        &one_user("hi"),
        "",
        &[],
        Media::default(),
        &Model::new("default"),
        &catalog,
        "c",
        "UTC",
        1,
        RunShape::default(),
        Phase::Opening,
    );
    let body = dig(&bytes, &[FRAME_BODY]).unwrap();
    let listed: Vec<String> = Reader::new(body)
        .filter_map(|(f, v)| match (f, v) {
            (BODY_MODEL_CATALOG, PbValue::Len(s)) => string_at(s, MODEL_NAME),
            _ => None,
        })
        .collect();
    assert_eq!(listed, vec!["default", "grok-4.5"]);

    // 关掉开关就一条都不发
    let off = RunShape {
        model_catalog: false,
        ..RunShape::default()
    };
    let b2 = build_frame0(
        &one_user("hi"),
        "",
        &[],
        Media::default(),
        &Model::new("default"),
        &catalog,
        "c",
        "UTC",
        1,
        off,
        Phase::Opening,
    );
    assert!(dig(&b2, &[FRAME_BODY, BODY_MODEL_CATALOG]).is_none());
}

#[test]
fn probe_models_stay_out_of_the_menu() {
    // 探测项(menu_visible=false)可被选中为当前模型,但不进 1.14 清单 ——
    // 未证实的条目不污染所有模型的请求(审查 gpt-5.6-sol 高危)。
    let catalog = vec![
        Model::new("default"),
        Model::with_params("grok-4.6", &[("effort", "high")]).probe(),
    ];
    let bytes = build_frame0(
        &one_user("hi"),
        "",
        &[],
        Media::default(),
        &Model::with_params("grok-4.6", &[("effort", "high")]).probe(),
        &catalog,
        "c",
        "UTC",
        1,
        RunShape::default(),
        Phase::Opening,
    );
    let body = dig(&bytes, &[FRAME_BODY]).unwrap();
    let listed: Vec<String> = Reader::new(body)
        .filter_map(|(f, v)| match (f, v) {
            (BODY_MODEL_CATALOG, PbValue::Len(s)) => string_at(s, MODEL_NAME),
            _ => None,
        })
        .collect();
    assert_eq!(listed, vec!["default"], "探测项不该进 1.14 清单");
    // 但 1.9 当前模型确实是 grok-4.6 —— 探测流量真的打到它身上。
    let cur = dig(body, &[BODY_MODEL]).unwrap();
    assert_eq!(string_at(cur, MODEL_NAME).as_deref(), Some("grok-4.6"));
}

#[test]
fn shape_toggles_actually_drop_sections() {
    let off = RunShape {
        env_block: false,
        budget_table: false,
        prosemirror: false,
        model_catalog: false,
        context_block: false,
    };
    let bytes = f0(
        &one_user("hi"),
        &Model::new("default"),
        "c",
        "UTC",
        1,
        off,
        Phase::Continuation,
    );
    let body = dig(&bytes, &[FRAME_BODY]).unwrap();
    assert!(
        dig(body, &[BODY_ENV]).is_none(),
        "env_block=false 应整块不发"
    );
    let m = dig(
        dig(body, &[BODY_CONVERSATION]).unwrap(),
        &[CONV_TURN, TURN_MESSAGE],
    )
    .unwrap();
    assert!(
        string_at(m, MSG_RICH).is_none(),
        "prosemirror=false 应不发富文本"
    );
    // 会话与模型是骨架,任何 shape 下都要在
    assert_eq!(string_at(m, MSG_TEXT).as_deref(), Some("hi"));
    assert!(dig(body, &[BODY_MODEL]).is_some());

    // 预算表单独关
    let no_budget = RunShape {
        budget_table: false,
        ..RunShape::default()
    };
    let b2 = f0(
        &one_user("hi"),
        &Model::new("default"),
        "c",
        "UTC",
        1,
        no_budget,
        Phase::Continuation,
    );
    let env2 = dig(dig(&b2, &[FRAME_BODY]).unwrap(), &[BODY_ENV]).unwrap();
    assert!(dig(env2, &[ENV_BUDGET]).is_none());
    // 但环境块其余字段还在
    assert_eq!(string_at(env2, ENV_TIMEZONE).as_deref(), Some("UTC"));
}

#[test]
fn budget_table_reports_real_char_count() {
    let turns = vec![Turn {
        text: "12345678".into(),
        is_user: true,
    }];
    let bytes = f0(
        &turns,
        &Model::new("default"),
        "c",
        "UTC",
        1,
        RunShape::default(),
        Phase::Continuation,
    );
    let env = dig(dig(&bytes, &[FRAME_BODY]).unwrap(), &[BODY_ENV]).unwrap();
    // 三层:1.1.5{1:合计,2:上限,3:{1:合计,2:上限,3:[分节…]}}
    let outer = dig(env, &[ENV_BUDGET]).unwrap();
    assert_eq!(
        varint_at(outer, 2),
        Some(BUDGET_MAX_TOKENS),
        "上限恒为 256000"
    );
    let inner = dig(outer, &[3]).unwrap();
    assert_eq!(varint_at(inner, 2), Some(BUDGET_MAX_TOKENS));
    let secs: Vec<&[u8]> = Reader::new(inner)
        .filter_map(|(f, v)| match (f, v) {
            (3, PbValue::Len(s)) => Some(s),
            _ => None,
        })
        .collect();
    assert_eq!(secs.len(), 8, "真客户端报 8 节");
    let conv = secs
        .iter()
        .find(|s| string_at(s, 1).as_deref() == Some("conversation"))
        .unwrap();
    assert_eq!(varint_at(conv, 4), Some(8), "chars 必须是真实字符数");
    assert_eq!(varint_at(conv, 3), Some(2), "tokens = ceil(chars/4)");
    // 合计 = 各节之和(抓包实物这条恒成立)
    let sum: u64 = secs.iter().map(|s| varint_at(s, 3).unwrap_or(0)).sum();
    assert_eq!(varint_at(outer, 1), Some(sum));
    assert_eq!(varint_at(inner, 1), Some(sum));
    // 工具/mcp/subagents 必须报 0 —— 声明了就会收到无法应答的工具调用
    for k in ["tools", "mcp", "subagents", "rules", "skills"] {
        let sec = secs
            .iter()
            .find(|s| string_at(s, 1).as_deref() == Some(k))
            .unwrap();
        assert_eq!(varint_at(sec, 3).unwrap_or(0), 0, "{k} 必须报 0");
    }
}

#[test]
fn prosemirror_splits_paragraphs_and_escapes() {
    let doc: Value = serde_json::from_str(&prosemirror_doc("a\n\nb\"q\"", &[])).unwrap();
    let c = doc["content"].as_array().unwrap();
    assert_eq!(c.len(), 3);
    assert_eq!(c[0]["content"][0]["text"], "a");
    assert!(c[1].get("content").is_none(), "空行是无内容的段落");
    assert_eq!(c[2]["content"][0]["text"], "b\"q\"");
}

/// 文档必须在 ProseMirror 里有 `mentionNode`,否则模型不知道附件存在。
#[test]
fn 文档在富文本里带提及节点() {
    let s = prosemirror_doc("讲什么", &["/tmp/gw-cursor/doc-0.pdf".to_string()]);
    let doc: Value = serde_json::from_str(&s).unwrap();
    let node = &doc["content"][0]["content"][0];
    assert_eq!(node["type"], "mentionNode");
    assert_eq!(node["attrs"]["mentionType"], "file");
    assert_eq!(node["attrs"]["label"], "doc-0.pdf");
    assert_eq!(node["attrs"]["rawText"], "/tmp/gw-cursor/doc-0.pdf");
    assert_eq!(
        node["attrs"]["payload"]["uri"]["path"],
        "/tmp/gw-cursor/doc-0.pdf"
    );
    // id/uuid 是 `file:file://<百分号编码路径>`,与真客户端同形。
    assert_eq!(node["attrs"]["id"], "file:file:///tmp/gw-cursor/doc-0.pdf");
    // 文本仍在后面的段落里。
    assert_eq!(doc["content"][1]["content"][0]["text"], "讲什么");
}

#[test]
fn 路径百分号编码只转非ascii() {
    assert_eq!(percent_encode_path("/tmp/a-b_c.pdf"), "/tmp/a-b_c.pdf");
    // 中文按 UTF-8 逐字节转,与真客户端的 %E4%B8%8B%E8%BD%BD 同形。
    assert_eq!(
        percent_encode_path("/下载/x.pdf"),
        "/%E4%B8%8B%E8%BD%BD/x.pdf"
    );
    assert_eq!(percent_encode_path("/a b"), "/a%20b");
}

#[test]
fn prelude_frames_match_captured_client_exactly() {
    // 逐字对齐真客户端。⚠️ 这四帧**不影响生成**(只发帧0 也出字),
    // 守的是「与真 IDE 不可区分」,不是「能不能出字」——别再把因果记反。
    let f = build_prelude_frames();
    assert_eq!(f.len(), 4);

    // 帧1 {3:{3:""}}
    let i0 = dig(&f[0], &[FRAME_CONTEXT]).unwrap();
    assert_eq!(string_at(i0, 3).as_deref(), Some(""));
    assert!(varint_at(i0, 1).is_none(), "帧1 没有 3.1");

    // 帧2 {3:{1:1,3:""}}
    let i1 = dig(&f[1], &[FRAME_CONTEXT]).unwrap();
    assert_eq!(varint_at(i1, 1), Some(1));
    assert_eq!(string_at(i1, 3).as_deref(), Some(""));

    // 帧3/帧4 {7:""}
    for k in [2usize, 3] {
        assert_eq!(string_at(&f[k], FRAME_READY).as_deref(), Some(""));
    }
    // 与抓包实物的帧 payload 字节数逐一对齐(解压后 4 / 6 / 2 / 2)。
    // 这条是「逐字照抄」的硬证据 —— 差一个字节就说明结构又不一样了。
    assert_eq!(
        f.iter().map(|x| x.len()).collect::<Vec<_>>(),
        vec![4, 6, 2, 2]
    );
}

/// **prefill**:Anthropic 允许以 assistant 消息结尾让模型续写。
/// 那时最后一轮是助手轮,而上下文块在抓包实物里永远长在**用户轮**上。
#[test]
fn prefill时上下文块挂在最后一条用户轮上() {
    let turns = vec![
        Turn {
            text: "问题".into(),
            is_user: true,
        },
        Turn {
            text: "答案是".into(),
            is_user: false,
        },
    ];
    let bytes = build_frame0(
        &turns,
        "sys",
        &[],
        Media::default(),
        &Model::new("default"),
        &[],
        "c",
        "UTC",
        1,
        RunShape::default(),
        Phase::Opening,
    );
    let conv = dig(&bytes, &[FRAME_BODY, BODY_CONVERSATION]).unwrap();
    let seen: Vec<&[u8]> = Reader::new(conv)
        .filter_map(|(f, v)| match (f, v) {
            (CONV_TURN, PbValue::Len(s)) => Some(s),
            _ => None,
        })
        .collect();
    assert_eq!(seen.len(), 2);
    assert!(dig(seen[0], &[TURN_CONTEXT]).is_some(), "必须挂在用户轮");
    assert!(
        dig(seen[1], &[TURN_CONTEXT]).is_none(),
        "助手轮(prefill)不该带上下文块"
    );
}

#[should_panic(expected = "至少一轮消息")]
#[test]
fn 空消息列表当场panic而不是造出不生成的请求() {
    build_frame0(
        &[],
        "",
        &[],
        Media::default(),
        &Model::new("default"),
        &[],
        "c",
        "UTC",
        1,
        RunShape::default(),
        Phase::Opening,
    );
}

#[test]
fn system_prompt_goes_to_context_block_not_user_turn() {
    let bytes = build_frame0(
        &one_user("问题"),
        "你是助手",
        &[],
        Media::default(),
        &Model::new("default"),
        &[],
        "c",
        "UTC",
        1,
        RunShape::default(),
        Phase::Opening,
    );
    let conv = dig(&bytes, &[FRAME_BODY, BODY_CONVERSATION]).unwrap();
    let det = dig(conv, &[CONV_TURN, TURN_CONTEXT]).unwrap();
    assert_eq!(
        string_at(det, DET_SYSTEM_PROMPT).as_deref(),
        Some("你是助手")
    );
    // 用户消息里**不能**混进系统提示
    let m = dig(conv, &[CONV_TURN, TURN_MESSAGE]).unwrap();
    assert_eq!(string_at(m, MSG_TEXT).as_deref(), Some("问题"));
    // 环境详情带 os/shell/时区
    let envd = dig(det, &[DET_ENV]).unwrap();
    assert!(string_at(envd, ENV_OS).unwrap().starts_with("linux "));
    assert_eq!(string_at(envd, ENV_TZ).as_deref(), Some("UTC"));
    // 那串开关要逐一在场
    for (f, v) in DET_FLAGS {
        assert_eq!(varint_at(det, *f), Some(*v), "9.{f} 应为 {v}");
    }
    assert_eq!(string_at(det, DET_HOOKS_A).as_deref(), Some("enabled"));
    // 空 system 时不发 .25
    let b2 = build_frame0(
        &one_user("q"),
        "",
        &[],
        Media::default(),
        &Model::new("default"),
        &[],
        "c",
        "UTC",
        1,
        RunShape::default(),
        Phase::Opening,
    );
    let det2 = dig(
        &b2,
        &[FRAME_BODY, BODY_CONVERSATION, CONV_TURN, TURN_CONTEXT],
    )
    .unwrap();
    assert!(string_at(det2, DET_SYSTEM_PROMPT).is_none());
}

/// **上下文块必须在场,哪怕是空的** —— 这条守的是本 crate 卡了整轮的那个 bug。
///
/// 2026-08-07 从真包削出来的对照:两个请求只差这一个长度为 0 的字段
/// (445B vs 446B),没有它上游回 200、回一帧会话回显,然后每 10 秒一个
/// 4 字节心跳,**永不出字**;有它立刻正常生成。既没有错误码也没有超时,
/// 单看返回值永远查不出来。
///
/// 所以它不能挂在 `RunShape.context_block` 下面 —— 那个开关只管里面装什么。
#[test]
fn 上下文块即使为空也必须发出否则上游静默挂起() {
    let shape = RunShape {
        context_block: false,
        ..RunShape::default()
    };
    let bytes = build_frame0(
        &one_user("q"),
        "系统提示",
        &[],
        Media::default(),
        &Model::new("default"),
        &[],
        "c",
        "UTC",
        1,
        shape,
        Phase::Opening,
    );
    let det = dig(
        &bytes,
        &[FRAME_BODY, BODY_CONVERSATION, CONV_TURN, TURN_CONTEXT],
    )
    .expect("1.2.1.2 必须在场,否则服务端接受请求但永远不生成");
    assert!(
        det.is_empty(),
        "关掉 context_block 时里面应为空,但字段本身要在"
    );
}

/// **首轮 `1.1` 必须是空的**(字段在、长度 0),后续轮才装预算表。
///
/// 抓包实测:首轮 98858B / 后续轮 2121B。首轮那 98KB 上下文是内联发的、
/// 服务端自己看得见;后续轮上下文在服务端手里,客户端只能报账告诉它各节多大。
#[test]
fn 首轮环境块为空后续轮才报账() {
    let open = f0(
        &one_user("q"),
        &Model::new("default"),
        "c",
        "UTC",
        1,
        RunShape::default(),
        Phase::Opening,
    );
    let env = dig(&open, &[FRAME_BODY, BODY_ENV]).expect("字段本身要在");
    assert!(env.is_empty(), "首轮 1.1 必须是空的,实际 {}B", env.len());

    let cont = f0(
        &one_user("q"),
        &Model::new("default"),
        "c",
        "UTC",
        1,
        RunShape::default(),
        Phase::Continuation,
    );
    let env = dig(&cont, &[FRAME_BODY, BODY_ENV]).unwrap();
    assert!(!env.is_empty(), "后续轮 1.1 要带预算表");
    assert_eq!(string_at(env, ENV_TIMEZONE).as_deref(), Some("UTC"));
}

/// 后续轮**只发最后那条用户消息**,上下文声明从 `1.2.1.2` 挪到 `1.2.17`。
///
/// 历史在服务端手里(按 `1.5` 持有)。重发历史既浪费也可能被当成新内容 ——
/// 抓包实物 turn2 的 `1.2.1` 就只有一条,且里面**没有** `.2`。
#[test]
fn 后续轮只发新消息但上下文声明仍挂轮内() {
    let turns = vec![
        Turn {
            text: "第一问".into(),
            is_user: true,
        },
        Turn {
            text: "第一答".into(),
            is_user: false,
        },
        Turn {
            text: "第二问".into(),
            is_user: true,
        },
    ];
    let bytes = build_frame0(
        &turns,
        "sys",
        &[],
        Media::default(),
        &Model::new("default"),
        &[],
        "c",
        "UTC",
        1,
        RunShape::default(),
        Phase::Continuation,
    );
    let conv = dig(&bytes, &[FRAME_BODY, BODY_CONVERSATION]).unwrap();
    let seen: Vec<&[u8]> = Reader::new(conv)
        .filter_map(|(f, v)| match (f, v) {
            (CONV_TURN, PbValue::Len(s)) => Some(s),
            _ => None,
        })
        .collect();
    assert_eq!(seen.len(), 1, "后续轮只该发一条消息,实际 {}", seen.len());
    let m = dig(seen[0], &[TURN_MESSAGE]).unwrap();
    assert_eq!(
        string_at(m, MSG_TEXT).as_deref(),
        Some("第二问"),
        "发的必须是最新那条"
    );
    // ⭐ 上下文声明**仍在轮内** 1.2.1.2 —— 这是 2026-08-08 实测出来的唯一可用形态
    // (挪到会话级 1.2.17 会让上游静默挂起,见 PROTOCOL §17)。
    let det = dig(seen[0], &[TURN_CONTEXT]).expect("后续轮轮内必须有 1.2.1.2");
    assert_eq!(string_at(det, DET_SYSTEM_PROMPT).as_deref(), Some("sys"));

    // 且**绝不发** 1.2.17:那个块要求 FileSync 上传过的 blob 哈希。
    assert!(
        dig(conv, &[CONV_CONTEXT]).is_none(),
        "1.2.17 一旦出现,服务端会等一个我们永远不会上传的 blob"
    );
}

/// 多轮时上下文块只挂**最后一轮**:与真客户端一致,也避免几十 KB 重复。
#[test]
fn 多轮对话只有最后一轮带上下文块() {
    let turns = vec![
        Turn {
            text: "第一问".into(),
            is_user: true,
        },
        Turn {
            text: "第一答".into(),
            is_user: false,
        },
        Turn {
            text: "第二问".into(),
            is_user: true,
        },
    ];
    let bytes = build_frame0(
        &turns,
        "sys",
        &[],
        Media::default(),
        &Model::new("default"),
        &[],
        "c",
        "UTC",
        1,
        RunShape::default(),
        Phase::Opening,
    );
    let conv = dig(&bytes, &[FRAME_BODY, BODY_CONVERSATION]).unwrap();
    let seen: Vec<&[u8]> = Reader::new(conv)
        .filter_map(|(f, v)| match (f, v) {
            (CONV_TURN, PbValue::Len(s)) => Some(s),
            _ => None,
        })
        .collect();
    assert_eq!(seen.len(), 3);
    assert!(
        dig(seen[0], &[TURN_CONTEXT]).is_none(),
        "前面几轮不该重复上下文块"
    );
    assert!(dig(seen[1], &[TURN_CONTEXT]).is_none());
    let det = dig(seen[2], &[TURN_CONTEXT]).expect("最后一轮必须带");
    assert_eq!(string_at(det, DET_SYSTEM_PROMPT).as_deref(), Some("sys"));
}

/// 工具声明落在 `1.2.1.2.7`,**5 个字段一个不能少**(真包 16/16 全都有)。
#[test]
fn 工具声明逐字对齐真包的五个字段() {
    let tools = vec![ToolDef {
        name: "get_weather".into(),
        description: "查天气".into(),
        schema: r#"{"type":"object"}"#.into(),
    }];
    let bytes = build_frame0(
        &one_user("北京天气"),
        "sys",
        &tools,
        Media::default(),
        &Model::new("default"),
        &[],
        "c",
        "UTC",
        1,
        RunShape::default(),
        Phase::Opening,
    );
    let det = dig(
        &bytes,
        &[FRAME_BODY, BODY_CONVERSATION, CONV_TURN, TURN_CONTEXT],
    )
    .unwrap();
    let t = dig(det, &[DET_TOOL]).expect("工具必须落在 1.2.1.2.7");
    // 全名 = <命名空间>-<裸名>,回调时靠这个前缀认领「这是调用方的工具」。
    assert_eq!(
        string_at(t, TOOL_FULL_NAME).as_deref(),
        Some("gwtools-get_weather")
    );
    assert_eq!(string_at(t, TOOL_DESC).as_deref(), Some("查天气"));
    assert_eq!(string_at(t, TOOL_NAMESPACE).as_deref(), Some(TOOL_NS));
    assert_eq!(string_at(t, TOOL_BARE_NAME).as_deref(), Some("get_weather"));
    assert_eq!(
        string_at(t, TOOL_SCHEMA).as_deref(),
        Some(r#"{"type":"object"}"#)
    );
}

/// 没声明工具时**不能**凭空多出一个空的 `1.2.1.2.7`。
#[test]
fn 没有工具时不发空的工具声明() {
    let bytes = build_frame0(
        &one_user("hi"),
        "",
        &[],
        Media::default(),
        &Model::new("default"),
        &[],
        "c",
        "UTC",
        1,
        RunShape::default(),
        Phase::Opening,
    );
    let det = dig(
        &bytes,
        &[FRAME_BODY, BODY_CONVERSATION, CONV_TURN, TURN_CONTEXT],
    )
    .unwrap();
    assert!(dig(det, &[DET_TOOL]).is_none());
}

/// **绝不发 `1.2.17`**:那里只放 blob 哈希,而我们从没走 FileSync 上传过东西。
/// 实测发了(带真哈希)会被 `invalid_argument: Failed to resolve request context
/// blobs` 直接拒掉,而这是唯一一条会让整轮请求作废的踩法。
#[test]
fn 绝不发会话级的1_2_17blob哈希块() {
    let bytes = build_frame0(
        &one_user("q"),
        "sys",
        &[],
        Media::default(),
        &Model::new("default"),
        &[],
        "c",
        "UTC",
        1,
        RunShape::default(),
        Phase::Opening,
    );
    let conv = dig(&bytes, &[FRAME_BODY, BODY_CONVERSATION]).unwrap();
    assert!(
        dig(conv, &[17]).is_none(),
        "1.2.17 一旦出现,服务端会去解析我们根本没上传的 blob"
    );
}

/// 无图时 `1.2.1.1.3` 发**空**(与真客户端一致);它不是"恒为空",而是附件容器。
#[test]
fn 无图时附件容器为空() {
    let bytes = f0(
        &one_user("x"),
        &Model::new("default"),
        "c",
        "UTC",
        1,
        RunShape::default(),
        Phase::Opening,
    );
    let m = dig(
        &bytes,
        &[FRAME_BODY, BODY_CONVERSATION, CONV_TURN, TURN_MESSAGE],
    )
    .unwrap();
    assert_eq!(string_at(m, MSG_ATTACH).as_deref(), Some(""));
}

/// 图片内联进 `1.2.1.1.3`,逐字对齐带图抓包:uuid / 路径 / 宽高 / mime / {哈希, 原始字节}。
///
/// ⚠️ **不走 FileSync**。文档 §5 猜的「图片走 blob 上传」是错的 ——
/// 带图那次真包里没有任何 FSSyncFile 调用,字节就在请求里。
#[test]
fn 图片内联进消息附件容器() {
    // 2×1 的最小合法 PNG(只为让 image_dims 解出尺寸)。
    let png = {
        let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
        v.extend_from_slice(&[0, 0, 0, 13]);
        v.extend_from_slice(b"IHDR");
        v.extend_from_slice(&2u32.to_be_bytes());
        v.extend_from_slice(&1u32.to_be_bytes());
        v.extend_from_slice(&[8, 2, 0, 0, 0]);
        v
    };
    let imgs = vec![ImageAttachment {
        mime: "image/png".into(),
        bytes: png.clone(),
        width: 2,
        height: 1,
    }];
    let bytes = build_frame0(
        &one_user("这是什么"),
        "",
        &[],
        Media {
            images: &imgs,
            docs: &[],
        },
        &Model::new("default"),
        &[],
        "c",
        "UTC",
        1,
        RunShape::default(),
        Phase::Opening,
    );
    let m = dig(
        &bytes,
        &[FRAME_BODY, BODY_CONVERSATION, CONV_TURN, TURN_MESSAGE],
    )
    .unwrap();
    // 注意 `.1`:真包是 1.2.1.1.3 → .1 → 各字段,少这层服务端回 internal。
    let att = dig(m, &[MSG_ATTACH, 1]).expect("图片必须落在 1.2.1.1.3.1");
    assert_eq!(string_at(att, ATT_MIME).as_deref(), Some("image/png"));
    let dims = dig(att, &[ATT_DIMS]).expect("要带宽高");
    assert_eq!(varint_at(dims, ATT_DIM_W), Some(2));
    assert_eq!(varint_at(dims, ATT_DIM_H), Some(1));
    let content = dig(att, &[ATT_CONTENT]).expect("要有 .9 内容块");
    // 原始字节内联,不是哈希引用。
    let inlined = dig(content, &[ATT_BYTES]).expect("原始字节要在 .9.2");
    assert_eq!(inlined, &png[..], "必须是原始字节,不能是 base64 或哈希");
    let hash = dig(content, &[ATT_HASH]).expect("要有 32 字节哈希");
    assert_eq!(hash.len(), 32);
}

/// 文档挂**上下文块**的 `1.2.1.2.20`,不是消息附件容器 —— 两者去处不同。
#[test]
fn 文档挂上下文块而不是消息() {
    let docs = vec![DocAttachment {
        path: "/tmp/x.pdf".into(),
        text: "%PDF-1.4 内容".into(),
        extracted: None,
    }];
    let bytes = build_frame0(
        &one_user("这个 PDF 讲什么"),
        "",
        &[],
        Media {
            images: &[],
            docs: &docs,
        },
        &Model::new("default"),
        &[],
        "c",
        "UTC",
        1,
        RunShape::default(),
        Phase::Opening,
    );
    let det = dig(
        &bytes,
        &[FRAME_BODY, BODY_CONVERSATION, CONV_TURN, TURN_CONTEXT],
    )
    .unwrap();
    let d = dig(det, &[DET_DOC]).expect("文档必须落在 1.2.1.2.20");
    assert_eq!(string_at(d, DOC_PATH).as_deref(), Some("/tmp/x.pdf"));
    assert_eq!(string_at(d, DOC_CONTENT).as_deref(), Some("%PDF-1.4 内容"));
    // 而消息附件容器必须还是空的。
    let m = dig(
        &bytes,
        &[FRAME_BODY, BODY_CONVERSATION, CONV_TURN, TURN_MESSAGE],
    )
    .unwrap();
    assert_eq!(string_at(m, MSG_ATTACH).as_deref(), Some(""));
}

#[test]
fn 图片尺寸只读头不解像素() {
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.extend_from_slice(&[0, 0, 0, 13]);
    png.extend_from_slice(b"IHDR");
    png.extend_from_slice(&614u32.to_be_bytes());
    png.extend_from_slice(&694u32.to_be_bytes());
    assert_eq!(image_dims(&png), (614, 694));
    // JPEG:SOF0 段里 高在前、宽在后。
    let jpeg = vec![
        0xFF, 0xD8, // SOI
        0xFF, 0xC0, 0x00, 0x11, 0x08, 0x01, 0x2C, 0x02, 0x58, // SOF0: h=300 w=600
    ];
    assert_eq!(image_dims(&jpeg), (600, 300));
    // 认不出来的格式给 (0,0),**不猜** —— 猜错的宽高会让上游的图像分块算错。
    assert_eq!(image_dims(b"GIF89a...."), (0, 0));
    assert_eq!(image_dims(b""), (0, 0));
}
