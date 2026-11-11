//! exec 读写帧测试。

use super::*;

/// 反向钉住:真工具帧(外部/内建)**不是**写/读调用 —— 否则工具回路直接哑掉。
#[test]
fn 工具帧不被当成_exec_调用() {
    let ext = include_bytes!("../../../tests/fixtures/tool_call_external_real.bin");
    assert!(parse_exec_write(ext).is_none(), "外部工具帧不是写调用");
    assert!(parse_exec_read(ext).is_none(), "外部工具帧不是读调用");
    let builtin = include_bytes!("../../../tests/fixtures/tool_call_builtin_real.bin");
    assert!(parse_exec_write(builtin).is_none(), "内建工具帧不是写调用");
    assert!(parse_exec_read(builtin).is_none(), "内建工具帧不是读调用");
}

/// 回执编码:字段号必须与 `agent.v1` schema 一致 —— 自己写自己解是白测,
/// 所以这里手解字节核对关键字段的位置。
#[test]
fn 写盘回执的字段号对齐_schema() {
    let ack = encode_write_success(42, "exec-1", "/assets/a.png", 73);
    // AgentClientMessage.2 = exec_client_message
    let ecm = Reader::new(&ack)
        .find_map(|(f, v)| match (f, v) {
            (2, PbValue::Len(s)) => Some(s),
            _ => None,
        })
        .expect("顶层必须有 field 2");
    let mut seen_id = None;
    let mut seen_exec_id = None;
    let mut wr = None;
    for (f, v) in Reader::new(ecm) {
        match (f, v) {
            (1, PbValue::Varint(n)) => seen_id = Some(n),
            (15, PbValue::Len(s)) => seen_exec_id = Some(String::from_utf8_lossy(s).to_string()),
            (3, PbValue::Len(s)) => wr = Some(s),
            _ => {}
        }
    }
    assert_eq!(seen_id, Some(42), "id 必须原样带回(field 1)");
    assert_eq!(
        seen_exec_id.as_deref(),
        Some("exec-1"),
        "exec_id 在 field 15"
    );
    // write_result.1 = write_success{1:path, 3:file_size}
    let ws = Reader::new(wr.expect("write_result 在 field 3"))
        .find_map(|(f, v)| match (f, v) {
            (1, PbValue::Len(s)) => Some(s),
            _ => None,
        })
        .expect("success 在 field 1");
    let mut path = None;
    let mut size = None;
    for (f, v) in Reader::new(ws) {
        match (f, v) {
            (1, PbValue::Len(s)) => path = Some(String::from_utf8_lossy(s).to_string()),
            (3, PbValue::Varint(n)) => size = Some(n),
            _ => {}
        }
    }
    assert_eq!(path.as_deref(), Some("/assets/a.png"));
    assert_eq!(size, Some(73));

    // 缺省 id(0 / "")不占字段 —— 实测的资产写调用就是这个形态。
    let ack0 = encode_write_success(0, "", "/assets/a.png", 1);
    let ecm0 = Reader::new(&ack0)
        .find_map(|(f, v)| match (f, v) {
            (2, PbValue::Len(s)) => Some(s),
            _ => None,
        })
        .unwrap();
    assert!(
        !Reader::new(ecm0).any(|(f, _)| f == 1 || f == 15),
        "缺省 id 不该出现在线上"
    );
}

/// 读盘回执:图片走 `data`(field 5,bytes),file_size 在 field 4。
#[test]
fn 读盘回执的字段号对齐_schema() {
    let reply = encode_read_success_data(7, "e2", "/assets/a.png", b"\x89PNG");
    let ecm = Reader::new(&reply)
        .find_map(|(f, v)| match (f, v) {
            (2, PbValue::Len(s)) => Some(s),
            _ => None,
        })
        .expect("顶层必须有 field 2");
    let mut seen_id = None;
    let mut rr = None;
    for (f, v) in Reader::new(ecm) {
        match (f, v) {
            (1, PbValue::Varint(n)) => seen_id = Some(n),
            (7, PbValue::Len(s)) => rr = Some(s),
            _ => {}
        }
    }
    assert_eq!(seen_id, Some(7));
    let rs = Reader::new(rr.expect("read_result 在 field 7"))
        .find_map(|(f, v)| match (f, v) {
            (1, PbValue::Len(s)) => Some(s),
            _ => None,
        })
        .expect("success 在 field 1");
    let mut path = None;
    let mut size = None;
    let mut data = None;
    for (f, v) in Reader::new(rs) {
        match (f, v) {
            (1, PbValue::Len(s)) => path = Some(String::from_utf8_lossy(s).to_string()),
            (4, PbValue::Varint(n)) => size = Some(n),
            (5, PbValue::Len(s)) => data = Some(s.to_vec()),
            _ => {}
        }
    }
    assert_eq!(path.as_deref(), Some("/assets/a.png"));
    assert_eq!(size, Some(4));
    assert_eq!(
        data.as_deref(),
        Some(&b"\x89PNG"[..]),
        "图片字节走 field 5(data)"
    );
}
