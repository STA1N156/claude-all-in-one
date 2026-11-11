//! 响应帧测试。

use super::*;

/// 会话登记通知(CLI 形态)必须被认出,且不能误吞真正的 exec 请求。
#[test]
fn session_notice_recognized_and_exec_untouched() {
    // 抓包实物同构:{2: {10: {2: conv}, 19: {1: trace, 2: span, 3: 0}, 55: 0}}
    let mut rc = Writer::new();
    rc.string(2, "9ea80b89-54e0-4273-a369-da13d190177c");
    let mut span = Writer::new();
    span.string(1, "3c0527122e953ea293870b02f1176698");
    span.string(2, "19ed9930a989daae");
    span.uint(3, 0);
    let mut ch = Writer::new();
    ch.message(10, &rc);
    ch.message(19, &span);
    ch.uint(55, 0);
    let mut notice = Writer::new();
    notice.message(2, &ch);
    assert!(is_session_notice(&notice.into_bytes()));

    // 带可执行负载(shell=field 2)的 exec 帧绝不能被判成通知。
    let mut shell = Writer::new();
    shell.string(1, "ls -la");
    let mut ch2 = Writer::new();
    ch2.message(2, &shell);
    ch2.uint(55, 0);
    let mut exec = Writer::new();
    exec.message(2, &ch2);
    assert!(!is_session_notice(&exec.into_bytes()));
}

#[test]
fn 文本增量是三层不是两层() {
    // 抓包实物:1 -> 1 -> {1: 文本, 2: 1}。少钻一层会把 message 的原始字节当文本。
    assert_eq!(parse_frame(&delta_frame(RESP_TEXT, "你好")).text, "你好");
}

/// **正文在 `1.1.1`,思考在 `1.4.1`,两者结构完全相同。**
///
/// 这条守的是一个不会报错的坏法:解错字段号时请求成功、也有字出来,
/// 但出来的是模型的推理过程,而它的真实回答一个字都收不到。
/// 实测「只回答 PINEAPPLE」那次:`1.4.1`='用户要求…',`1.1.1`='P'/'INE'/'APPLE'。
#[test]
fn 思考流绝不能混进正文() {
    let think = parse_frame(&delta_frame(RESP_THINKING, "用户要求我只回答一个词"));
    assert_eq!(think.text, "", "1.4 是思考,一个字都不许进正文");
    assert_eq!(think.thinking, "用户要求我只回答一个词");

    let body = parse_frame(&delta_frame(RESP_TEXT, "PINEAPPLE"));
    assert_eq!(body.text, "PINEAPPLE");
    assert_eq!(body.thinking, "");
}

/// 用量帧既是计费数据,也是**本轮唯一的收尾信号** —— BiDi 流不会自己关。
#[test]
fn 用量帧解出三个数且是收尾信号() {
    let mut u = Writer::new();
    u.uint(1, 12106);
    u.uint(2, 74);
    u.uint(3, 11904);
    let mut msg = Writer::new();
    msg.message(RESP_USAGE, &u);
    let mut outer = Writer::new();
    outer.message(RESP_MESSAGE, &msg);
    let fr = parse_frame(&outer.into_bytes());
    assert_eq!(
        fr.usage,
        Some(WireUsage {
            input: 12106,
            output: 74,
            cache_read: 11904,
            cache_write: 0
        })
    );
    // 普通文本帧不能被误判成收尾,否则第一个字出来就把流关了。
    assert_eq!(parse_frame(&delta_frame(RESP_TEXT, "x")).usage, None);
}

/// 心跳帧与会话回显都不该被当成任何东西。
///
/// ⚠️ 真心跳是 **4 字节的 `field 1` 帧**,而 `field 1` 正是解析器唯一下钻的顶层字段 ——
/// 也就是说心跳和正文走**同一条**解析路径,这才是最该钉住的那条。
/// 只构造 `field 4` 回显是在考一条根本不会走错的路。
#[test]
fn 心跳与会话回显不产生文本也不收尾() {
    // ① 真心跳:field 1 里裹一个空串字段(§10.6 的 1.13)。
    let mut hb_inner = Writer::new();
    hb_inner.string(13, "");
    let mut hb = Writer::new();
    hb.message(RESP_MESSAGE, &hb_inner);
    assert_eq!(
        parse_frame(&hb.into_bytes()),
        RespFrame::default(),
        "心跳不许产出任何东西"
    );

    // ② 状态帧 1.8.1 也走 field 1,同样不该产出。
    let mut st_inner = Writer::new();
    st_inner.uint(1, 9);
    let mut st_msg = Writer::new();
    st_msg.message(8, &st_inner);
    let mut st = Writer::new();
    st.message(RESP_MESSAGE, &st_msg);
    assert_eq!(parse_frame(&st.into_bytes()), RespFrame::default());

    // ③ 会话回显走 field 4,连下钻都不该下。
    let mut echo = Writer::new();
    echo.string(1, "conversation-echo");
    let mut outer = Writer::new();
    outer.message(4, &echo);
    assert_eq!(parse_frame(&outer.into_bytes()), RespFrame::default());
}

/// 干活帧判读:手写字节,不复用实现里的常量。
#[test]
fn 工具参数增量与token计数算干活_心跳不算() {
    // {1:{7:{3:"abc"}}} = partial_tool_call.args_text_delta
    assert!(is_work_update(&[
        0x0a, 0x07, 0x3a, 0x05, 0x1a, 0x03, b'a', b'b', b'c'
    ]));
    // {1:{8:{1:3}}} = token_delta.tokens=3:刻意不算(卡住时也会来)
    assert!(!is_work_update(&[0x0a, 0x04, 0x42, 0x02, 0x08, 0x03]));
    // {1:{15:{}}} = tool_call_delta(字段 15 → tag 0x7a)
    assert!(is_work_update(&[0x0a, 0x02, 0x7a, 0x00]));
    // {1:{13:""}} = 心跳
    assert!(!is_work_update(&[0x0a, 0x02, 0x6a, 0x00]));
    // {1:{1:{1:"x"}}} = 正文(另有判据,不归这里)
    assert!(!is_work_update(&[0x0a, 0x05, 0x0a, 0x03, 0x0a, 0x01, b'x']));
    // 组装判定只认 1.7 / 1.15,token 计数和心跳都不算。
    assert!(is_tool_assembly(&[
        0x0a, 0x07, 0x3a, 0x05, 0x1a, 0x03, b'a', b'b', b'c'
    ]));
    assert!(is_tool_assembly(&[0x0a, 0x02, 0x7a, 0x00]));
    assert!(!is_tool_assembly(&[0x0a, 0x04, 0x42, 0x02, 0x08, 0x03]));
    assert!(!is_tool_assembly(&[0x0a, 0x02, 0x6a, 0x00]));
}

/// 心跳的**判读**(排水段计时口径靠它排除零进展帧,2026-09-04 pi 卡死案)。
#[test]
fn 心跳帧的判读严格钉死形态() {
    // ① 真心跳 {1:{13:""}} → true。
    let mut hb_inner = Writer::new();
    hb_inner.string(13, "");
    let mut hb = Writer::new();
    hb.message(RESP_MESSAGE, &hb_inner);
    assert!(is_heartbeat(&hb.into_bytes()));

    // ② 思考帧(1.4)/正文帧同走顶层 field 1 → 必须 false,否则长思考
    //    会被当成零进展。
    let mut th_inner = Writer::new();
    th_inner.string(4, "在想");
    let mut th = Writer::new();
    th.message(RESP_MESSAGE, &th_inner);
    assert!(!is_heartbeat(&th.into_bytes()));

    // ③ 心跳与别的字段合帧(顶层多一个描述符 .3)→ false:那一帧有真信息。
    let mut hb_inner2 = Writer::new();
    hb_inner2.string(13, "");
    let mut mixed = Writer::new();
    mixed.message(RESP_MESSAGE, &hb_inner2);
    mixed.bytes(3, b"descriptor");
    assert!(!is_heartbeat(&mixed.into_bytes()));

    // ④ 反例(codex 评审):空子消息 / 非空值 / 重复字段 / 截断,一律 false。
    let mut empty_sub = Writer::new();
    empty_sub.message(RESP_MESSAGE, &Writer::new());
    assert!(
        !is_heartbeat(&empty_sub.into_bytes()),
        "{{1:{{}}}} 不是心跳"
    );

    let mut nonempty_inner = Writer::new();
    nonempty_inner.string(13, "x");
    let mut nonempty = Writer::new();
    nonempty.message(RESP_MESSAGE, &nonempty_inner);
    assert!(!is_heartbeat(&nonempty.into_bytes()), "13 非空串不是心跳");

    let mut dup_inner = Writer::new();
    dup_inner.string(13, "");
    dup_inner.string(13, "");
    let mut dup = Writer::new();
    dup.message(RESP_MESSAGE, &dup_inner);
    assert!(!is_heartbeat(&dup.into_bytes()), "重复 field 13 不是心跳");

    // 合法心跳掐掉最后一个字节(截断)→ false。
    let mut hb3_inner = Writer::new();
    hb3_inner.string(13, "");
    let mut hb3 = Writer::new();
    hb3.message(RESP_MESSAGE, &hb3_inner);
    let hb_bytes = hb3.into_bytes();
    assert!(
        !is_heartbeat(&hb_bytes[..hb_bytes.len() - 1]),
        "截断帧不是心跳"
    );
}

/// 同一帧里正文与用量并存时,正文**不能**被丢。
/// 「一帧只装一样」只是抓包观察,protobuf 层没有任何东西保证它。
#[test]
fn 正文与用量同帧时两者都要解出来() {
    let mut txt = Writer::new();
    txt.string(RESP_DELTA_TEXT, "APPLE");
    txt.uint(2, 1);
    let mut u = Writer::new();
    u.uint(1, 10);
    u.uint(2, 2);
    u.uint(3, 0);
    let mut msg = Writer::new();
    msg.message(RESP_TEXT, &txt);
    msg.message(RESP_USAGE, &u);
    let mut outer = Writer::new();
    outer.message(RESP_MESSAGE, &msg);
    let fr = parse_frame(&outer.into_bytes());
    assert_eq!(fr.text, "APPLE", "收口帧里的正文必须照样解出来");
    assert_eq!(
        fr.usage,
        Some(WireUsage {
            input: 10,
            output: 2,
            cache_read: 0,
            cache_write: 0
        })
    );
}

/// ⭐ 合帧:资产写调用(顶层 field 2)与用量(`1.14`)并进同一帧时,
/// 两边都得解得出来。chat.rs 的流程依赖这个 —— 先处理 exec、再判工具、
/// 最后判用量,谁也不许把谁吞掉(吞了用量 = 陪上游死等心跳到 watchdog)。
#[test]
fn 资产写调用与用量合帧两者都可解() {
    let mut args = Writer::new();
    args.string(1, "/assets/attach-0-x.png");
    args.bytes(5, b"png-bytes");
    let mut ch = Writer::new();
    ch.message(EXEC_WRITE_ARGS, &args);
    let mut u = Writer::new();
    u.uint(1, 100);
    u.uint(2, 5);
    u.uint(3, 90);
    let mut msg = Writer::new();
    msg.message(RESP_USAGE, &u);
    let mut top = Writer::new();
    top.message(RESP_MESSAGE, &msg);
    top.message(RESP_TOOL_CHANNEL, &ch);
    let frame = top.into_bytes();
    let w = parse_exec_write(&frame).expect("合帧里的写调用必须解得出");
    assert_eq!(w.path, "/assets/attach-0-x.png");
    assert_eq!(w.bytes, b"png-bytes");
    assert!(parse_tool_call(&frame).is_none(), "合帧里没有工具调用");
    assert_eq!(
        parse_frame(&frame).usage,
        Some(WireUsage {
            input: 100,
            output: 5,
            cache_read: 90,
            cache_write: 0
        }),
        "合帧里的用量绝不能被写调用吞掉"
    );
}

/// 总结开始/完成帧:字节照 2026-11-09 生产诊断日志(`[1]/[10] 4B`)手写,不用常量造。
#[test]
fn 总结开始与完成帧按实物识别() {
    let started = [0x0A, 0x02, 0x52, 0x00]; // {1:{10:""}}
    let completed = [0x0A, 0x02, 0x5A, 0x00]; // {1:{11:""}}
    let heartbeat = [0x0A, 0x02, 0x6A, 0x00]; // {1:{13:""}}
    assert!(is_summary_started(&started));
    assert!(!is_summary_completed(&started));
    assert!(is_summary_completed(&completed));
    assert!(!is_summary_started(&completed));
    assert!(!is_summary_started(&heartbeat) && !is_summary_completed(&heartbeat));
    assert!(is_heartbeat(&heartbeat));
}

#[test]
fn parse_text_delta_ignores_session_echo_and_unknown_fields() {
    // field 4 是会话回显,不是文本;还塞一个未知的 fixed64 确认不会错位。
    let mut echo = Writer::new();
    echo.string(1, "conversation-echo");
    let mut inner = Writer::new();
    inner.string(2, "not text");
    let mut outer = Writer::new();
    outer.message(RESP_MESSAGE, &inner);
    outer.message(4, &echo);
    assert_eq!(parse_frame(&outer.into_bytes()), RespFrame::default());
}

/// `4.2` 内容分节哈希需求检测。
///
/// ⚠️ 帧字节**手写十六进制**,不用 `Writer`/常量造 —— lessons §7:逆向出来的
/// 协议若拿与解析器同一套常量造帧再解,字段号整套写错也照样绿。
/// 结构 `{4: {2: {1: bin[32]}}}`:
///   `22 24`(field4,len36)`12 22`(field2,len34)`0a 20`(field1,len32)+32B
#[test]
fn 内容分节哈希需求_按实物结构识别() {
    let h32: Vec<u8> = (0u8..32).collect();
    let mut frame = vec![0x22, 0x24, 0x12, 0x22, 0x0a, 0x20];
    frame.extend_from_slice(&h32);
    assert_eq!(content_hash_echo(&frame), 1);

    // 同一个 field-4 里两条需求。
    let mut two = vec![0x22, 0x48, 0x12, 0x22, 0x0a, 0x20];
    two.extend_from_slice(&h32);
    two.extend_from_slice(&[0x12, 0x22, 0x0a, 0x20]);
    two.extend_from_slice(&h32);
    assert_eq!(content_hash_echo(&two), 2);

    // 负面①:`.1` 不是 32 字节 → 不是内容分节哈希。
    let short = vec![0x22, 0x06, 0x12, 0x04, 0x0a, 0x02, 0xaa, 0xbb];
    assert_eq!(content_hash_echo(&short), 0);
    // 负面②:空帧/非 field-4。
    assert_eq!(content_hash_echo(&[]), 0);
    assert_eq!(content_hash_echo(&[0x0a, 0x02, 0x01, 0x02]), 0);
}

/// 负面③(**用真实抓包字节**):带顶层 `.3` 描述符的真响应帧里没有内容分节需求。
/// 这条是防误报的关键 —— 若 `.3` 被误算成需求,每条正常续轮都会被判成要 CAS 上传。
#[test]
fn 内容分节哈希需求_真描述符帧不误报() {
    assert_eq!(
        content_hash_echo(&crate::run::descriptor_samples::turn1()),
        0
    );
    assert_eq!(
        content_hash_echo(&crate::run::descriptor_samples::turn2()),
        0
    );
}

/// 判决语义:**只有「拿不出来」才是失败**,「被点名」本身不是。
///
/// 这条是 2026-08-23 时序实证之后的更正。早先把「出现 `4.2`」当 NO-GO,
/// 会把一次健康的 resume(服务端缓存过期→索取→客户端交付)判成方案不可行。
#[test]
fn wire判决_点名不是失败_拿不出来才是() {
    // 点名了、也全都交付了(上传腿接上后的正常 resume)→ Ok。
    let served = WireTurnOutcome {
        saw_descriptor: true,
        saw_finish: true,
        content_chars: 42,
        demanded: 7,
        unavailable: 0,
    };
    assert_eq!(served.verdict(), WireVerdict::Ok);
    assert!(!served.should_fallback());
    assert!(!served.verdict().needs_reflow());

    // 点名 7 个、拿不出 7 个(当前无上传腿的形态)→ 本轮失败 + 作废描述符。
    let missing = WireTurnOutcome {
        demanded: 7,
        unavailable: 7,
        ..Default::default()
    };
    assert_eq!(
        missing.verdict(),
        WireVerdict::ContentUnavailable {
            missing: 7,
            demanded: 7
        }
    );
    assert!(missing.should_fallback());
    assert!(missing.verdict().needs_reflow());

    // 部分拿不出来也算失败:少一节就是少一段历史。
    let partial = WireTurnOutcome {
        saw_finish: true,
        content_chars: 10,
        demanded: 7,
        unavailable: 1,
        ..Default::default()
    };
    assert!(partial.should_fallback(), "缺一节也不能记成功");
}

/// 判决优先级与 NoFinish 的不对称处理。
#[test]
fn wire判决_优先级与NoFinish不回退() {
    // 裸态 + 拿不出内容:必须报 ContentUnavailable(它解释裸态成因)。
    let both = WireTurnOutcome {
        demanded: 3,
        unavailable: 3,
        ..Default::default()
    };
    assert!(matches!(
        both.verdict(),
        WireVerdict::ContentUnavailable { .. }
    ));

    // 纯裸态。
    let barren = WireTurnOutcome::default();
    assert_eq!(barren.verdict(), WireVerdict::Barren);
    assert!(barren.should_fallback());

    // 出了字但没收尾:**不**回退本轮(答案有价值),但**要**作废描述符。
    let nf = WireTurnOutcome {
        content_chars: 42,
        ..Default::default()
    };
    assert_eq!(nf.verdict(), WireVerdict::NoFinish);
    assert!(!nf.should_fallback(), "出了字的轮次不该扔掉");
    assert!(nf.verdict().needs_reflow(), "但它的描述符不可信,必须作废");

    // 干净收尾但没有新 `.3` → NoDescriptor(codex 二轮 #2)。
    //
    // 官方每轮尾部都给新描述符(实测 turn1/turn2/turn4 各 3 份),不给就是异常。
    // 处理不对称:**本轮不回退**(回答是好的,扔掉等于白烧一次额度),
    // 但**旧描述符必须作废** —— 它已经缺了本轮,留着下轮回放就是静默失忆。
    let no_desc = WireTurnOutcome {
        saw_descriptor: false,
        saw_finish: true,
        content_chars: 42,
        ..Default::default()
    };
    assert_eq!(no_desc.verdict(), WireVerdict::NoDescriptor);
    assert!(!no_desc.should_fallback(), "出了字的轮次不该扔掉");
    assert!(no_desc.verdict().needs_reflow(), "但旧描述符必须作废");

    // 有新描述符 + 干净收尾 + 出字 = 唯一的 Ok。
    let ok = WireTurnOutcome {
        saw_descriptor: true,
        saw_finish: true,
        content_chars: 42,
        ..Default::default()
    };
    assert_eq!(ok.verdict(), WireVerdict::Ok);
    assert!(!ok.verdict().needs_reflow());
}

/// 空顶层 `.3` 不算捕获(codex 审查 #4):否则上层判成「有料」→ 静默失忆。
#[test]
fn 空描述符不算捕获() {
    // {3: ""} —— 结构合法但零长度。
    let payload = vec![0x1a, 0x00];
    assert!(descriptor_field3(&payload).is_none(), "空 .3 必须当没有");
    // 一帧里先空后非空:取非空那份。
    let mut mixed = vec![0x1a, 0x00];
    mixed.extend_from_slice(&[0x1a, 0x03, b'a', b'b', b'c']);
    assert_eq!(descriptor_field3(&mixed), Some(&b"abc"[..]));
    // 先非空后空:空的**不得**把非空那份挤掉。
    let mut mixed2 = vec![0x1a, 0x03, b'a', b'b', b'c'];
    mixed2.extend_from_slice(&[0x1a, 0x00]);
    assert_eq!(descriptor_field3(&mixed2), Some(&b"abc"[..]));
}

/// 影子模式①:真实抓包帧(resp-011/013 形态)里解出顶层 `.3` 并数对 ref 个数。
/// turn1 = 4 个 blob 引用,turn2 = 6 个(turn1 产生的新 blob 追加进来)。
#[test]
fn 描述符_真实帧解出field3并数对ref() {
    let t1 = crate::run::descriptor_samples::turn1();
    let d1 = descriptor_field3(&t1).expect("turn1 帧必须带顶层 .3");
    assert_eq!(descriptor_ref_count(d1), 4, "turn1 描述符 = 4 个 blob 引用");
    // 整帧就是一份 `.3`(抓包实物:顶层只有 field 3)。
    assert_eq!(d1.len() + (t1.len() - d1.len()), t1.len());

    let t2 = crate::run::descriptor_samples::turn2();
    let d2 = descriptor_field3(&t2).expect("turn2 帧必须带顶层 .3");
    assert_eq!(descriptor_ref_count(d2), 6, "turn2 描述符 = 6 个 blob 引用");
    // turn1 的 4 个 ref 原样排在 turn2 前头(纯追加,顺序不动)——
    // 这是「描述符 = 累计态」的直接证据,也是将来回放能跨轮的依据。
    let refs_of = |d: &[u8]| -> Vec<Vec<u8>> {
        Reader::new(d)
            .filter(|(f, v)| *f == 1 && matches!(v, PbValue::Len(s) if s.len() == 32))
            .map(|(_, v)| match v {
                PbValue::Len(s) => s.to_vec(),
                _ => unreachable!(),
            })
            .collect()
    };
    let (r1, r2) = (refs_of(d1), refs_of(d2));
    assert_eq!(
        &r2[..4],
        &r1[..],
        "turn2 的前 4 个 ref 必须就是 turn1 的全部"
    );
}

/// 影子模式②:顶层不带 field 3 的帧不产生捕获。
#[test]
fn 描述符_无field3不捕获() {
    // 真实 turn1 的 resp-012(顶层只有 field 1 的状态帧)。
    let payload = [0x0a, 0x08, 0x8a, 0x01, 0x05, 0x08, 0x02, 0x10, 0xda, 0x17];
    assert!(descriptor_field3(&payload).is_none());
    assert!(descriptor_field3(&[]).is_none());
}

/// 一帧内 `.3` 多次出现时取**最后一份**(尾部状态最新,见函数注释)。
#[test]
fn 描述符_一帧多份取尾部() {
    let mut w = Writer::new();
    w.bytes(3, b"first");
    w.bytes(1, b"text");
    w.bytes(3, b"second");
    let p = w.into_bytes();
    assert_eq!(descriptor_field3(&p), Some(&b"second"[..]));
}
