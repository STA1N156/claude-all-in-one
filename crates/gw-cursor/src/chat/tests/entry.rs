//! chat 测试:请求入口。

use super::*;

#[test]
fn 策略句变化会改指纹() {
    let _g = crate::GUARD_TEST_LOCK.lock().unwrap();
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = crate::set_tool_guard_policy("");
        }
    }
    let _r = Restore;

    crate::set_tool_guard_policy("").unwrap();
    let a = crate::tool_guard_rev();
    crate::set_tool_guard_policy("换一版文案").unwrap();
    let b = crate::tool_guard_rev();
    assert_ne!(a, b, "分桶字段必须随文案变");
    assert_eq!(a.len(), 8, "短指纹 4 字节 = 8 hex");
}

#[test]
fn 空会话id不记纠偏标记() {
    // conversation_id 兜底失败时是空串;拿它当 key 会让所有无 id 请求串成一个。
    let n = crate::TruncationNotices::default();
    n.record("", Some("读文件"));
    assert!(n.take("").is_none());
}

/// IDE 回退路径要挂 KV 应答(否则服务端 set_blob 等不到回执就挂死);
/// 不带推迟帧、不报 MCP 工具;请求流不保持打开时回不了帧,不挂。
#[test]
fn ide路径挂kv应答且不带推迟帧() {
    let sections = std::sync::Arc::new(crate::ContentSections::default());
    let up = ide_kv_upload(true, &sections, &[]).expect("保持流打开时必须挂");
    assert!(up.deferred.is_empty());
    assert!(up.mcp_tools.is_empty());
    assert!(
        std::sync::Arc::ptr_eq(&up.sections, &sections),
        "节库要与会话共用"
    );
    assert!(ide_kv_upload(false, &sections, &[]).is_none());
}
