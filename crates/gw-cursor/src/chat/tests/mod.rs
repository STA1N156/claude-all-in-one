use super::*;

mod builtin;
mod entry;
mod errors;
mod guard;
mod history;
mod media;
mod session;
mod stream;
mod system;
mod usage;

fn ctx() -> RunCtx {
    RunCtx {
        host: "agentn.global.api5.cursor.sh".into(),
        token: "tok".into(),
        machine_id: "m".repeat(64),
        mac_machine_id: Some("a".repeat(64)),
        config_version: "cv".into(),
        timezone: "Asia/Shanghai".into(),
        conversation_id: "conv-1".into(),
        sim_conversation_id: "conv-1".into(),
        account_id: "acc-1".into(),
        shape: RunShape::default(),
        context_frames: true,
        phase: run::Phase::Opening,
        profile: crate::cli::Profile::Ide,
        keep_stream_open: true,
        assets: std::sync::Arc::new(crate::AssetStore::default()),
        notices: std::sync::Arc::new(crate::TruncationNotices::default()),
        shadow: std::sync::Arc::new(crate::DescriptorShadow::default()),
        shadow_fps: Vec::new(),
        shadow_fps_full: Vec::new(),
        wire_descriptor_replay: true,
        sections: std::sync::Arc::new(crate::ContentSections::default()),
    }
}

/// 造一帧思考增量 `1.4.1`(与 run 单测同形,避免依赖私有 helper)。
fn thinking_wire(text: &str) -> Vec<u8> {
    use crate::protobuf::Writer;
    let mut delta = Writer::new();
    delta.string(1, text);
    delta.uint(2, 1);
    let mut msg = Writer::new();
    msg.message(4, &delta);
    let mut outer = Writer::new();
    outer.message(1, &msg);
    outer.into_bytes()
}

fn tool_with_schema(name: &str, schema: &str) -> run::ToolDef {
    run::ToolDef {
        name: name.into(),
        description: String::new(),
        schema: schema.into(),
    }
}

/// 造一帧内建终端调用(`1.2.2.1.1.1` = 命令串,§13.2 抓包实证形状)。
fn builtin_terminal_wire(command: &str) -> Vec<u8> {
    use crate::protobuf::Writer;
    let mut term = Writer::new();
    term.string(1, command);
    let mut one = Writer::new();
    one.message(1, &term);
    let mut detail = Writer::new();
    detail.message(1, &one); // 1.2.2.1 = 内建终端
    let mut ch = Writer::new();
    ch.string(1, "call-b-0");
    ch.message(2, &detail);
    let mut msg = Writer::new();
    msg.message(2, &ch);
    let mut outer = Writer::new();
    outer.message(1, &msg);
    outer.into_bytes()
}

/// 服务端 exec 写盘帧 `{2: {1: 7, 15: "exec-w-7", 3: {1: path, 2: file_text}}}`
/// (`write_args`,`file_text` 是服务端算好的完整内容)。
fn exec_write_wire(path: &str, content: &str) -> Vec<u8> {
    use crate::protobuf::Writer;
    let mut wr = Writer::new();
    wr.string(1, path);
    wr.string(2, content);
    let mut em = Writer::new();
    em.uint(1, 7);
    em.string(15, "exec-w-7");
    em.message(3, &wr);
    let mut top = Writer::new();
    top.message(2, &em);
    top.into_bytes()
}

/// 服务端 exec 读文件帧 `{2: {1: 8, 15: "exec-r-8", 7: {1: path}}}`(`read_args`)。
fn exec_read_wire(path: &str) -> Vec<u8> {
    use crate::protobuf::Writer;
    let mut rd = Writer::new();
    rd.string(1, path);
    let mut em = Writer::new();
    em.uint(1, 8);
    em.string(15, "exec-r-8");
    em.message(7, &rd);
    let mut top = Writer::new();
    top.message(2, &em);
    top.into_bytes()
}

/// 造一帧内建**编辑**界面帧(`1.2.2.12` edit_tool_call:`.1.1` = 路径、`.1.6` = 内容;
/// 模型调 Write 时内容是全文,调 StrReplace 时只是片段)。
fn builtin_write_wire(path: &str, content: &str) -> Vec<u8> {
    use crate::protobuf::Writer;
    let mut wr = Writer::new();
    wr.string(1, path);
    wr.string(6, content);
    let mut one = Writer::new();
    one.message(1, &wr);
    let mut detail = Writer::new();
    detail.message(12, &one); // 1.2.2.12 = 内建写文件
    let mut ch = Writer::new();
    ch.string(1, "call-w-7");
    ch.message(2, &detail);
    let mut msg = Writer::new();
    msg.message(2, &ch);
    let mut outer = Writer::new();
    outer.message(1, &msg);
    outer.into_bytes()
}

/// 文案断言用:带**内置默认**策略句的纯函数版,不碰进程全局。
fn guard(tools: &[run::ToolDef]) -> String {
    builtin_tool_guard_with(tools, crate::DEFAULT_TOOL_GUARD_POLICY)
}

fn tdefs(names: &[&str]) -> Vec<run::ToolDef> {
    names
        .iter()
        .map(|n| run::ToolDef {
            name: (*n).to_string(),
            description: String::new(),
            schema: "{}".to_string(),
        })
        .collect()
}

/// 服务端 exec 形态的 MCP 调用 `{2: {1: id, 15: exec_id, 11: {1: name, 2: {1: "filePath", 2: {3: path}}, 3: call_id, 5: name}}}`。
fn exec_mcp_wire(id: u64, exec_id: &str, call_id: &str, name: &str, path: &str) -> Vec<u8> {
    use crate::protobuf::Writer;
    let mut val = Writer::new();
    val.string(3, path);
    let mut kv = Writer::new();
    kv.string(1, "filePath");
    kv.message(2, &val);
    let mut args = Writer::new();
    args.string(1, name);
    args.message(2, &kv);
    args.string(3, call_id);
    args.string(5, name);
    let mut em = Writer::new();
    em.uint(1, id);
    em.string(15, exec_id);
    em.message(11, &args);
    let mut top = Writer::new();
    top.message(2, &em);
    top.into_bytes()
}

type ChanTx = tokio::sync::mpsc::UnboundedSender<Vec<u8>>;

/// 可随时追加帧的上游:发 payload(未分帧)即到。
fn chan_upstream() -> (
    ChanTx,
    impl futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Send + 'static,
) {
    use futures::StreamExt;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let s = tokio_stream::wrappers::UnboundedReceiverStream::new(rx)
        .map(|p| Ok(bytes::Bytes::from(wire::frame(&p))));
    (tx, s)
}

fn run_hold(
    up: impl futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Send + 'static,
    conv: &str,
    keepalive: BodyKeepalive,
    hold: HoldOpts,
) -> impl futures::Stream<Item = Result<StreamItem, UpstreamError>> + Send {
    stream_to_anthropic_with(
        up,
        "claude-opus-5-5".into(),
        false,
        keepalive,
        None,
        conv.into(),
        Default::default(),
        std::sync::Arc::new(crate::TruncationNotices::default()),
        BuiltinXlate::default(),
        ChatUsage::default(),
        0,
        true,
        None,
        None,
        hold,
    )
}

/// 同 [`run_hold`],但带调用方工具的翻译表(自带工具要翻成调用方工具才会交付)。
fn run_hold_x(
    up: impl futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Send + 'static,
    conv: &str,
    keepalive: BodyKeepalive,
    hold: HoldOpts,
    xlate: BuiltinXlate,
) -> impl futures::Stream<Item = Result<StreamItem, UpstreamError>> + Send {
    stream_to_anthropic_with(
        up,
        "claude-opus-5-5".into(),
        false,
        keepalive,
        None,
        conv.into(),
        Default::default(),
        std::sync::Arc::new(crate::TruncationNotices::default()),
        xlate,
        ChatUsage::default(),
        0,
        true,
        None,
        None,
        hold,
    )
}

/// Claude Code 形态的 Read / Bash 工具翻译表。
fn cc_read_bash_xlate() -> BuiltinXlate {
    BuiltinXlate::from_tools(&[
        run::ToolDef {
            name: "Read".into(),
            description: "Reads a file".into(),
            schema: json!({"type":"object","properties":{"file_path":{"type":"string"},"offset":{"type":"number"},"limit":{"type":"number"}}}).to_string(),
        },
        run::ToolDef {
            name: "Bash".into(),
            description: "Executes a bash command".into(),
            schema: json!({"type":"object","properties":{"command":{"type":"string"}}}).to_string(),
        },
    ])
}

/// 服务端 exec 帧:读文件 `{2:{1:id, 15:exec_id, 7:{1:path, 2:tool_call_id}}}`
/// (cursor-agent `agent.v1.ReadArgs`;2026-11-10 实验号实测界面帧之后同毫秒到达)。
fn exec_read_args_wire(id: u64, exec_id: &str, path: &str, call_id: &str) -> Vec<u8> {
    use crate::protobuf::Writer;
    let mut a = Writer::new();
    a.string(1, path);
    a.string(2, call_id);
    let mut em = Writer::new();
    em.uint(1, id);
    em.string(15, exec_id);
    em.message(7, &a);
    let mut top = Writer::new();
    top.message(2, &em);
    top.into_bytes()
}

/// 服务端 exec 帧:流式终端 `{2:{1:id, 15:exec_id, 14:{1:command, 4:tool_call_id}}}`
/// (`shell_stream_args`,与 `shell_args` 同为 `agent.v1.ShellArgs`;实测自带终端走的是它)。
fn exec_shell_stream_args_wire(id: u64, exec_id: &str, command: &str, call_id: &str) -> Vec<u8> {
    use crate::protobuf::Writer;
    let mut a = Writer::new();
    a.string(1, command);
    a.string(4, call_id);
    let mut em = Writer::new();
    em.uint(1, id);
    em.string(15, exec_id);
    em.message(14, &a);
    let mut top = Writer::new();
    top.message(2, &em);
    top.into_bytes()
}

fn tool_use_ids(items: &[Result<StreamItem, UpstreamError>]) -> Vec<String> {
    items
        .iter()
        .filter_map(|i| match i {
            Ok(StreamItem::Sse(ev))
                if ev.event == "content_block_start"
                    && ev.data["content_block"]["type"] == "tool_use" =>
            {
                ev.data["content_block"]["id"].as_str().map(str::to_string)
            }
            _ => None,
        })
        .collect()
}

fn stop_reason_of(items: &[Result<StreamItem, UpstreamError>]) -> Option<String> {
    items.iter().find_map(|i| match i {
        Ok(StreamItem::Sse(ev)) if ev.event == "message_delta" => {
            ev.data["delta"]["stop_reason"].as_str().map(str::to_string)
        }
        _ => None,
    })
}

/// 造一帧能通过 `run::is_turn_commit` 的「本轮已提交」回显:
/// 顶层 field 4 → .3 → .2 → .1 = {3: 36 字符 turn id, 4: 长签名}(见该函数注释)。
fn turn_commit_frame() -> Vec<u8> {
    let mut rec = crate::protobuf::Writer::new();
    rec.string(3, &"t".repeat(36));
    rec.string(4, &"s".repeat(172));
    let mut w1 = crate::protobuf::Writer::new();
    w1.message(1, &rec);
    let mut w2 = crate::protobuf::Writer::new();
    w2.message(2, &w1);
    let mut w3 = crate::protobuf::Writer::new();
    w3.message(3, &w2);
    let mut top = crate::protobuf::Writer::new();
    top.message(4, &w3);
    top.into_bytes()
}

/// 按「(延迟秒, 帧)」序列吐帧,吐完挂住(上游 BiDi 永不自关)。时钟须暂停。
fn timed_frames(
    seq: Vec<(u64, Vec<u8>)>,
) -> impl futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Send + 'static {
    use futures::StreamExt;
    futures::stream::iter(seq)
        .then(|(secs, f)| async move {
            tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
            Ok(bytes::Bytes::from(wire::frame(&f)))
        })
        .chain(futures::stream::pending())
}

fn run_timed(
    seq: Vec<(u64, Vec<u8>)>,
    conv: &str,
) -> impl futures::Stream<Item = Result<StreamItem, UpstreamError>> + Send {
    run_timed_with(seq, conv, None, true)
}

fn run_timed_with(
    seq: Vec<(u64, Vec<u8>)>,
    conv: &str,
    keepalive: BodyKeepalive,
    cli_mode: bool,
) -> impl futures::Stream<Item = Result<StreamItem, UpstreamError>> + Send {
    run_timed_full(seq, conv, keepalive, cli_mode, BuiltinXlate::default())
}

fn run_timed_full(
    seq: Vec<(u64, Vec<u8>)>,
    conv: &str,
    keepalive: BodyKeepalive,
    cli_mode: bool,
    xlate: BuiltinXlate,
) -> impl futures::Stream<Item = Result<StreamItem, UpstreamError>> + Send {
    run_timed_assets(seq, conv, keepalive, cli_mode, xlate, Default::default())
}

fn run_timed_assets(
    seq: Vec<(u64, Vec<u8>)>,
    conv: &str,
    keepalive: BodyKeepalive,
    cli_mode: bool,
    xlate: BuiltinXlate,
    assets: std::sync::Arc<crate::AssetStore>,
) -> impl futures::Stream<Item = Result<StreamItem, UpstreamError>> + Send {
    stream_to_anthropic(
        timed_frames(seq),
        "claude-fable-5-1".into(),
        false,
        keepalive,
        None,
        conv.into(),
        assets,
        std::sync::Arc::new(crate::TruncationNotices::default()),
        xlate,
        ChatUsage::default(),
        0,
        cli_mode,
        None,
        None,
    )
}

// 手写字节:{1:{1:{1:"红"}}} 正文(E7 BA A2);{1:{7:{3:"ab"}}} 工具参数增量。
const TEXT_HAO: [u8; 9] = [0x0a, 0x07, 0x0a, 0x05, 0x0a, 0x03, 0xe7, 0xba, 0xa2];
const PARTIAL_TOOL: [u8; 8] = [0x0a, 0x06, 0x3a, 0x04, 0x1a, 0x02, b'a', b'b'];

/// 读文件界面帧 `{1:{2:{1:id, 2:{8:{1:{1:path}}}}}}`。
fn builtin_read_wire(path: &str) -> Vec<u8> {
    use crate::protobuf::Writer;
    let mut args = Writer::new();
    args.string(1, path);
    let mut one = Writer::new();
    one.message(1, &args);
    let mut detail = Writer::new();
    detail.message(8, &one);
    let mut ch = Writer::new();
    ch.string(1, "call-r-1");
    ch.message(2, &detail);
    let mut msg = Writer::new();
    msg.message(2, &ch);
    let mut outer = Writer::new();
    outer.message(1, &msg);
    outer.into_bytes()
}

fn tool(name: &str, desc: &str, props: Value) -> run::ToolDef {
    run::ToolDef {
        name: name.into(),
        description: desc.into(),
        schema: json!({"type":"object","properties":props,"required":["pattern"]}).to_string(),
    }
}
fn arg_map(tc: run::ToolCall) -> std::collections::HashMap<String, Value> {
    tc.args.into_iter().collect()
}
fn grep_call(path: Option<&str>, glob: Option<&str>) -> run::BuiltinCall {
    run::BuiltinCall::Grep {
        id: "g".into(),
        pattern: "it's".into(),
        path: path.map(Into::into),
        glob: glob.map(Into::into),
    }
}

/// 帧字节照 2026-11-09 生产诊断:`{1:{10:""}}` 总结开始、`{1:{11:""}}` 总结完成、`{1:{13:""}}` 心跳。
const SUMMARY_STARTED: [u8; 4] = [0x0A, 0x02, 0x52, 0x00];
const SUMMARY_COMPLETED: [u8; 4] = [0x0A, 0x02, 0x5A, 0x00];

fn count_red(items: &[Result<StreamItem, UpstreamError>]) -> usize {
    items
        .iter()
        .filter(|i| {
            matches!(i, Ok(StreamItem::Sse(ev))
                    if ev.event == "content_block_delta" && ev.data.to_string().contains("红"))
        })
        .count()
}
