//! 请求里的图片/文档收集与占位。
//!
//! 2026-11-10 由 chat.rs 拆出(只搬不改行为)。

use super::*;

/// 单个附件的原始字节上限。
///
/// 无上限时的失效链(两位审查员各自独立指出):50MB 的 PDF → base64 解码 50MB →
/// `from_utf8_lossy` 最坏膨胀到 ~150MB 的 `String` → 进 protobuf frame0 → gzip 缓冲,
/// 原始字节 / lossy 串 / protobuf 副本 / gzip 输出**同时存活**,峰值是附件的数倍。
/// 而这些工作全在 `chat_stream` 首个 await 之前同步做完,会占住 tokio 线程,
/// 让同 worker 上**别的账号**的请求一起饥饿。gw-kiro 早有出站体积硬顶,这里补齐。
pub(super) const MAX_ONE_ATTACHMENT: usize = 12 * 1024 * 1024;

/// 一次请求内所有附件的原始字节合计上限。
///
/// 单个上限挡不住"20 张 10MB 图片"。而多轮会话更狠:`to_media` 收的是**有史以来
/// 全部**消息里的附件,每轮全量重新内联,请求体随会话长度线性膨胀。
pub(super) const MAX_ALL_ATTACHMENTS: usize = 24 * 1024 * 1024;

/// [`to_media`] 的占位表:`(消息下标, 顶层块下标, tool_result 内嵌块下标)` → 占位文本。
///
/// 只登记 tool_result **内嵌**且真的被收进附件的媒体块。文本渲染
/// ([`extract_text_in_msg`])按同一坐标在原位补占位 —— 占位表与附件出自
/// [`to_media`] 的同一次扫描,编号不可能与附件顺序漂移。
pub(super) type MediaPlaceholders = std::collections::HashMap<(usize, usize, usize), String>;

/// 校验并收下**一个** image / document 块 —— 顶层块与 tool_result 内嵌块共用的
/// 单块逻辑。限额 / base64 校验 / 跳过口径必须两处一致,拆开写迟早不同步。
/// 收下返回 true;非 image/document、非 base64 源、超限、解码失败一律跳过返回 false。
pub(super) fn collect_media_block(
    b: &Value,
    images: &mut Vec<run::ImageAttachment>,
    docs: &mut Vec<run::DocAttachment>,
    budget: &mut usize,
) -> bool {
    use base64::Engine as _;
    let kind = b.get("type").and_then(|t| t.as_str()).unwrap_or("");
    if kind != "image" && kind != "document" {
        return false;
    }
    let src = b.get("source");
    let stype = src.and_then(|s| s.get("type")).and_then(|t| t.as_str());
    if stype != Some("base64") {
        tracing::warn!(kind, ?stype, "cursor: 非 base64 的媒体源,跳过(不代为下载)");
        return false;
    }
    let mime = src
        .and_then(|s| s.get("media_type"))
        .and_then(|t| t.as_str())
        .unwrap_or(if kind == "image" {
            "image/png"
        } else {
            "application/pdf"
        })
        .to_string();
    let Some(data) = src.and_then(|s| s.get("data")).and_then(|d| d.as_str()) else {
        return false;
    };
    // 解码**之前**先按 base64 长度估原始大小,别先解出 200MB 再判超限。
    // base64 是 4:3,估值只用于挡明显超标的,精确判断在解码后。
    if data.len() / 4 * 3 > MAX_ONE_ATTACHMENT {
        tracing::warn!(kind, b64_len = data.len(), "cursor: 附件超单个上限,跳过");
        return false;
    }
    let Ok(raw) = base64::engine::general_purpose::STANDARD.decode(data) else {
        tracing::warn!(kind, "cursor: base64 解码失败,跳过该附件");
        return false;
    };
    if raw.len() > MAX_ONE_ATTACHMENT || *budget + raw.len() > MAX_ALL_ATTACHMENTS {
        tracing::warn!(
            kind,
            bytes = raw.len(),
            budget = *budget,
            "cursor: 附件超上限(单个或累计),跳过 —— 无上限时一个大附件能打爆整个 worker"
        );
        return false;
    }
    *budget += raw.len();
    if kind == "image" {
        let (width, height) = run::image_dims(&raw);
        images.push(run::ImageAttachment {
            mime,
            bytes: raw,
            width,
            height,
        });
    } else {
        // 文档字段在上游是 proto3 `string`,真客户端把 PDF 当 UTF-8 读、
        // 二进制部分有损替换成 U+FFFD。我方逐字节同构地照做。
        let n = docs.len();
        // 我方**自己抽文本层**。上游协议不原生支持 PDF:真客户端是让模型
        // 调终端工具跑 `pdftotext`,由客户端在真实磁盘上执行 —— 反代不能
        // 答应内建终端工具调用(那等于执行模型选定的任意 shell 命令)。
        let extracted = crate::pdf::extract_text(&raw);
        docs.push(run::DocAttachment {
            path: format!("/tmp/gw-cursor/doc-{n}.pdf"),
            // `.20` 登记表仍照发原文,与真客户端同形。
            text: String::from_utf8_lossy(&raw).into_owned(),
            extracted,
        });
    }
    true
}

/// 从 Anthropic 请求里收集图片与文档附件。
///
/// Anthropic 侧:
/// - 图片 `{type:"image", source:{type:"base64", media_type, data}}`
/// - 文档 `{type:"document", source:{type:"base64", media_type:"application/pdf", data}}`
/// - **工具返回的图/文档**:同样的块,嵌在 `tool_result.content` 数组里。
///
/// 两者去处不同(见 [`run::Media`])。`source.type != "base64"`(如 `url`)一律跳过 ——
/// 替调用方去下载 URL 是我方主动出网,不该悄悄做。
///
/// 第三个返回值是 tool_result 内嵌媒体的占位表(见 [`MediaPlaceholders`])。
/// 曾经这里只扫顶层块,tool_result 内嵌的 image 直接 continue 掉;而
/// extract_text 的注释声称「媒体走 to_media 内联」—— 两边互相推诿,agent 用
/// Read 工具读的图片就静默消失、日志一条不留(2026-08-13 生产事故)。
pub(crate) fn to_media(
    body: &Value,
) -> (
    Vec<run::ImageAttachment>,
    Vec<run::DocAttachment>,
    MediaPlaceholders,
) {
    let mut images = Vec::new();
    let mut docs = Vec::new();
    let mut placeholders = MediaPlaceholders::new();
    // 已收下的附件原始字节总量,用来卡总闸(见 MAX_ONE_ATTACHMENT / MAX_ALL_ATTACHMENTS)。
    // 顶层块与 tool_result 内嵌块**共用同一个预算**,内嵌不是绕开限额的后门。
    let mut budget: usize = 0;
    let Some(msgs) = body.get("messages").and_then(|m| m.as_array()) else {
        return (images, docs, placeholders);
    };
    for (mi, m) in msgs.iter().enumerate() {
        let Some(blocks) = m.get("content").and_then(|c| c.as_array()) else {
            continue;
        };
        for (bi, b) in blocks.iter().enumerate() {
            let kind = b.get("type").and_then(|t| t.as_str()).unwrap_or("");
            if kind == "tool_result" {
                // content 为字符串时没有媒体;为块数组时逐块按顶层同款口径收。
                let Some(nested) = b.get("content").and_then(|c| c.as_array()) else {
                    continue;
                };
                for (ni, n) in nested.iter().enumerate() {
                    let is_image = n.get("type").and_then(|t| t.as_str()) == Some("image");
                    if collect_media_block(n, &mut images, &mut docs, &mut budget) {
                        // 占位文本里的编号与 run.rs `ImageAttachment::encode(seq)`
                        // 合成的路径 `attach-{seq}-…` 同源(都是 images 向量下标)。
                        let ph = if is_image {
                            format!("[图片见附件 attach-{}]", images.len() - 1)
                        } else {
                            format!(
                                "[文档见 {}]",
                                docs.last().map(|d| d.path.as_str()).unwrap_or("")
                            )
                        };
                        placeholders.insert((mi, bi, ni), ph);
                    }
                }
            } else {
                collect_media_block(b, &mut images, &mut docs, &mut budget);
            }
        }
    }
    if !images.is_empty() || !docs.is_empty() {
        // info 级:生产排查「图有没有收进来」曾经只能靠猜(收不到时零日志)。
        tracing::info!(
            images = images.len(),
            docs = docs.len(),
            from_tool_result = placeholders.len(),
            "cursor: 已收集请求附件"
        );
    }
    (images, docs, placeholders)
}
