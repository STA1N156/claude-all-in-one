//! 内容分节库(应答服务端 4.2 点名)。
//!
//! 2026-11-10 由 lib.rs 拆出(只搬不改行为)。

use super::*;

/// 内容分节库:`sha256(bytes) -> bytes`,用来应答服务端的 `4.2` 内容点名。
///
/// ## 为什么需要它(2026-08-23 抓包 + store.db 复核)
///
/// 描述符不是不透明状态,是一张**内容寻址清单**(`.1` 装消息节哈希、`.8` 装轮次提交
/// 记录哈希)。服务端的内容缓存过期后会逐条点名索取(field-4 `4.2`,一帧一个哈希),
/// 客户端必须按 hash 交出原字节。官方 CLI 就是靠
/// `~/.cursor/chats/<project>/<session>/store.db` 的 `blobs(id TEXT PRIMARY KEY, data BLOB)`
/// 应答的 —— 实测 **70/70 条 `id == sha256(data)`**,四个会话无一例外。
///
/// 我方四个供给来源(都已验证可得):
/// 1. **系统提示节**:每模型一份、跨会话稳定(Composer 2025B `83ccb7a5…`、
///    Grok 4.6 4343B `83ec1496…`)。它**不在 ENV 帧里**,是客户端自持的,
///    所以要从官方 store.db 收割成节库文件,见 [`ContentSections::load_model_library`]。
/// 2. **工具/rules 等节**:我方自己构造,构造时算 hash 存进来即可。
/// 3. **消息节**:我方持有的历史。
/// 4. **轮次提交记录节**:从响应的 field-4 收到(带 182B 服务端签名,只能原样留存)。
///
/// 内容寻址天然全局去重(同一份系统提示节被所有会话引用),所以这里**不按会话分片**,
/// 一张全局表 + 总量上限就够。
#[derive(Default, Debug)]
pub struct ContentSections {
    pub(super) inner: Mutex<HashMap<[u8; 32], SectionEntry>>,
}

#[derive(Debug)]
pub(super) struct SectionEntry {
    pub(super) data: Vec<u8>,
    /// 常驻项(节库里的系统提示节)不参与淘汰 —— 它被每份描述符引用,
    /// 淘汰掉就等于把续轮能力丢了,而它总量很小(每模型几 KB)。
    pub(super) pinned: bool,
    pub(super) at: Instant,
}

/// 非常驻节的总量上限。消息节随会话增长,要有闸。
pub(super) const SECTIONS_CAP: usize = 8192;

impl ContentSections {
    pub(super) fn map(&self) -> std::sync::MutexGuard<'_, HashMap<[u8; 32], SectionEntry>> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// 算 hash 并存入,返回该节的 hash。已存在则只刷新时间戳(内容寻址,同 hash 同内容)。
    pub fn insert(&self, data: &[u8]) -> [u8; 32] {
        self.insert_inner(data, false)
    }

    /// 存入常驻节(节库项,不参与淘汰)。
    pub fn insert_pinned(&self, data: &[u8]) -> [u8; 32] {
        self.insert_inner(data, true)
    }

    pub(super) fn insert_inner(&self, data: &[u8], pinned: bool) -> [u8; 32] {
        let h = Self::hash(data);
        let mut map = self.map();
        match map.get_mut(&h) {
            Some(e) => {
                e.at = Instant::now();
                e.pinned |= pinned;
            }
            None => {
                map.insert(
                    h,
                    SectionEntry {
                        data: data.to_vec(),
                        pinned,
                        at: Instant::now(),
                    },
                );
            }
        }
        Self::enforce_cap(&mut map);
        h
    }

    /// 按 hash 取内容。取不到 = 我方交不出这一节 = 本轮必须重铺。
    pub fn get(&self, h: &[u8; 32]) -> Option<Vec<u8>> {
        let mut map = self.map();
        let e = map.get_mut(h)?;
        e.at = Instant::now();
        Some(e.data.clone())
    }

    pub fn hash(data: &[u8]) -> [u8; 32] {
        use sha2::{Digest, Sha256};
        let d = Sha256::digest(data);
        let mut h = [0u8; 32];
        h.copy_from_slice(&d);
        h
    }

    pub(super) fn enforce_cap(map: &mut HashMap<[u8; 32], SectionEntry>) {
        let n = map.values().filter(|e| !e.pinned).count();
        if n <= SECTIONS_CAP {
            return;
        }
        let mut drop_n = n - SECTIONS_CAP;
        while drop_n > 0 {
            let Some(oldest) = map
                .iter()
                .filter(|(_, e)| !e.pinned)
                .min_by_key(|(_, e)| e.at)
                .map(|(k, _)| *k)
            else {
                break;
            };
            map.remove(&oldest);
            drop_n -= 1;
        }
    }

    /// 表内节数(常驻, 非常驻)。
    pub fn len(&self) -> (usize, usize) {
        let map = self.map();
        let p = map.values().filter(|e| e.pinned).count();
        (p, map.len() - p)
    }

    pub fn is_empty(&self) -> bool {
        self.map().is_empty()
    }
}

/// 内置系统提示节库(逐字节从官方 CLI 的 `store.db` 提取)。
///
/// 形状 `{ key: { sha256, json } }`。`json` 是**字符串**,用的时候必须按 UTF-8 字节
/// 处理 —— grok 那节是 4339 字符 / **4343 字节**(含多字节字符),按字符数截取就会
/// 算出错的 hash,那节从此永远交不出去。
///
/// 刷新走独立工具(harvest),不在运行期读 `~/.cursor`:生产机上没有那个目录,
/// 而节库必须随二进制一起可用。
pub(super) const SECTION_LIBRARY: &str = include_str!("../sections/system-prompt-sections.json");

/// 模型名 → 节库 key。
///
/// **只映射有证据的**,认不出的模型返回 `None`(宁可这一节交不出去→重铺,
/// 也不拿另一个模型的系统提示节冒充 —— 那会让 hash 对不上,等于白存)。
///
/// - `default` = Auto 的线上名,路由到 Composer。证据:08-23 抓包会话 `3d9e9788`
///   的帧0 `.9` = `{1:'default'}`,其系统节正是 `83ccb7a5…`(Composer 2025B)。
/// - `grok-4.6`:节正文自称 "powered by Cursor Grok 4.6",会话 `bf51854b`/`f39ae02f`。
pub(super) fn section_key_for_model(model: &str) -> Option<&'static str> {
    match model {
        "default" => Some("_composer_claude"),
        "grok-4.6" => Some("grok-4.6"),
        _ => None,
    }
}

impl ContentSections {
    /// 把该模型的系统提示节装进表(常驻)。返回是否装上。
    ///
    /// 装入前**校验 sha256 与节库声明一致**:节库是人工导出的文件,一旦被改动
    /// (哪怕只是编辑器改了行尾)hash 就变,而错的字节比没有更糟 —— 它会让我方
    /// 以为能应答,实际交出去的内容服务端不认。
    pub fn load_model_library(&self, model: &str) -> bool {
        let Some(key) = section_key_for_model(model) else {
            tracing::debug!(
                model,
                "cursor 节库:该模型没有已知的系统提示节,续轮被点名时只能重铺"
            );
            return false;
        };
        let lib: serde_json::Value = match serde_json::from_str(SECTION_LIBRARY) {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(error = %e, "cursor 节库 JSON 解析失败");
                return false;
            }
        };
        let Some(entry) = lib.get(key) else {
            tracing::error!(key, "cursor 节库缺该 key");
            return false;
        };
        let Some(text) = entry.get("json").and_then(|v| v.as_str()) else {
            tracing::error!(key, "cursor 节库该 key 缺 json 字段");
            return false;
        };
        // ⚠️ UTF-8 字节,不是字符。
        let bytes = text.as_bytes();
        let want = entry
            .get("sha256")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let got = Self::hash(bytes);
        let got_hex: String = got.iter().map(|b| format!("{b:02x}")).collect();
        if got_hex != want {
            tracing::error!(
                key, want, got = %got_hex, bytes = bytes.len(),
                "cursor 节库 sha256 不符 —— 拒绝装入(错字节比没有更糟)"
            );
            return false;
        }
        self.insert_pinned(bytes);
        tracing::info!(model, key, bytes = bytes.len(), hash = %&got_hex[..12], "cursor 节库:系统提示节已装入");
        true
    }
}
