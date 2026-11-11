//! chat 测试:媒体收集。

use super::*;

/// 内嵌块与顶层块共用同一套校验/限额口径:url 源跳过(不代为下载)、
/// 超单个上限跳过;被跳过的块**不占编号**,占位与附件顺序无空洞。
#[test]
fn tool_result_nested_non_base64_or_oversized_is_skipped() {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(b"ok");
    // 超限走 base64 长度预检(len/4*3 > MAX_ONE_ATTACHMENT),不用真解码 12MB。
    let huge = "A".repeat(MAX_ONE_ATTACHMENT / 3 * 4 + 8);
    let body = json!({"messages":[
        {"role":"user","content":[
            {"type":"tool_result","tool_use_id":"c-1","content":[
                {"type":"image","source":{"type":"url","url":"https://x/img.png"}},
                {"type":"image","source":{"type":"base64","media_type":"image/png","data": huge}},
                {"type":"image","source":{"type":"base64","media_type":"image/png","data": b64}}
            ]}
        ]}
    ]});
    let (images, docs, ph) = to_media(&body);
    assert_eq!(images.len(), 1, "url 源与超限的都要跳过");
    assert!(docs.is_empty());
    assert_eq!(ph.len(), 1);
    // 唯一收下的是第 3 个内嵌块(ni=2),编号从 0 起、无空洞。
    assert_eq!(
        ph.get(&(0, 0, 2)).map(String::as_str),
        Some("[图片见附件 attach-0]")
    );
    // 被跳过的块在文本流里不留占位(占位表里没有它们)。
    let turns = to_turns_with_media(&body, Some(&ph));
    assert!(!turns[0].text.contains("attach-1"));
}
