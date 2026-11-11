//! 附件库(服务端推来的 /assets/ 图片等,按会话缓存)。
//!
//! 2026-11-10 由 lib.rs 拆出(只搬不改行为)。

use super::*;

/// 服务端「写盘」调用(`write_args`)的内存兜底,**只认 `/assets/` 资产路径**。
///
/// 带图请求的服务端流程(2026-08-10 实物帧 + opencodex 的 `agent.v1` schema 核对):
/// 服务端用内建 write 把附件字节推给客户端「落盘」(`/assets/attach-N-<uuid>.png`),
/// 模型随后用内建 read 读同一路径把图看进去。真 IDE 有真磁盘所以无感;
/// 我们没有 —— write 存这里,read 从这里回。不回执的代价是服务端 90s 心跳死等
/// (带图请求全模型 502 的第二阶段病因)。
///
/// 按 conversation_id 分组、与 [`CONV_TTL`] 同寿命:服务端会话忘了,
/// 资产副本也没用了。存的是**我们自己刚发上去的附件字节**,不是用户新数据。
#[derive(Default, Debug)]
pub(crate) struct AssetStore {
    pub(super) inner: Mutex<HashMap<String, ConvAssets>>,
}

#[derive(Debug)]
pub(super) struct ConvAssets {
    /// 路径 → (字节, 最近读写序号)。会话装满时淘汰序号最小(最久没碰)的文件。
    pub(super) files: HashMap<String, (Vec<u8>, u64)>,
    pub(super) total: usize,
    pub(super) at: Instant,
    /// 下一个写入序号(会话内单调递增)。
    pub(super) next_seq: u64,
}

/// 单文件/单会话上限:与 chat.rs 的附件闸同值(服务端推来的本就是我们发上去的附件,
/// 只会更小不会更大;真超了说明上游行为变了,宁可丢资产也不放内存炸弹)。
pub(super) const ASSET_FILE_CAP: usize = 12 * 1024 * 1024;
pub(super) const ASSET_CONV_CAP: usize = 24 * 1024 * 1024;
/// provider 级总闸:会话闸 × 会话数在两小时内可以无界增长(每个新 conversation_id
/// 都是新桶),必须有全局上限。超了按最久未触的会话逐桶驱逐。
pub(super) const ASSET_GLOBAL_CAP: usize = 256 * 1024 * 1024;

impl AssetStore {
    /// 锁:毒化即恢复(同 [`ConvRegistry`] 的理由:纯缓存,不能让它变成全 provider 的 panic 点)。
    pub(super) fn map(&self) -> std::sync::MutexGuard<'_, HashMap<String, ConvAssets>> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// 存一份资产。超上限(单文件/单会话/全局驱逐后仍放不下)返回 `false` ——
    /// 调用方仍回执成功(别因我方内存策略把整轮搞死),但后续 read 会落空,日志里要留痕。
    pub(super) fn put(&self, conv: &str, path: &str, bytes: &[u8]) -> bool {
        if bytes.len() > ASSET_FILE_CAP {
            return false;
        }
        let mut map = self.map();
        // 顺手清过期:这张表按会话增长,没人清就是内存泄漏。
        map.retain(|_, e| e.at.elapsed() < CONV_TTL);
        // **先过单会话闸**:装不下就淘汰本会话最早写入的文件。
        //
        // 2026-11-07 生产实证:客户端每轮重传全部附件,服务端每次给一个新 uuid 路径
        // (`/assets/attach-N-<uuid>.png`),旧轮次的副本永远不会再被读,却一直占着会话额度。
        // 旧写法装满就拒新图 —— 70 分钟里 37 张 380~500KB 的图因此没存,模型读图落空。
        // 新图一定比旧副本更可能被读,所以淘汰旧的。
        if let Some(e) = map.get_mut(conv) {
            let old = e.files.get(path).map_or(0, |(v, _)| v.len());
            while e.total - old + bytes.len() > ASSET_CONV_CAP {
                let Some(victim) = e
                    .files
                    .iter()
                    .filter(|(k, _)| k.as_str() != path)
                    .min_by_key(|(_, (_, seq))| *seq)
                    .map(|(k, _)| k.clone())
                else {
                    break;
                };
                if let Some((v, _)) = e.files.remove(&victim) {
                    e.total -= v.len();
                }
            }
        }
        let (conv_total, old) = map
            .get(conv)
            .map(|e| (e.total, e.files.get(path).map_or(0, |(v, _)| v.len())))
            .unwrap_or((0, 0));
        if conv_total - old + bytes.len() > ASSET_CONV_CAP {
            return false;
        }
        // 再过全局闸:按最久未触逐桶驱逐。当前会话**显式排除** —— 把自己的桶
        // 逐掉再新建一个写入,同会话原有资产全灭,后续 read 全落空。
        let mut global: usize = map.values().map(|e| e.total).sum();
        // 投影要扣掉同路径旧值:覆盖写不是净增。
        while global - old + bytes.len() > ASSET_GLOBAL_CAP {
            let Some(oldest) = map
                .iter()
                .filter(|(k, _)| k.as_str() != conv)
                .min_by_key(|(_, e)| e.at)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some(e) = map.remove(&oldest) {
                global -= e.total;
            }
        }
        if global - old + bytes.len() > ASSET_GLOBAL_CAP {
            return false;
        }
        let e = map.entry(conv.to_string()).or_insert_with(|| ConvAssets {
            files: HashMap::new(),
            total: 0,
            at: Instant::now(),
            next_seq: 0,
        });
        e.at = Instant::now();
        e.total = e.total - old + bytes.len();
        let seq = e.next_seq;
        e.next_seq += 1;
        e.files.insert(path.to_string(), (bytes.to_vec(), seq));
        true
    }

    /// 取一份资产(模型 read 时回图)。不存在 → `None`(调用方走原来的收口);
    /// 过期桶**顺手删掉** —— 只读路径不能把过期数据留到地老天荒。
    pub(super) fn get(&self, conv: &str, path: &str) -> Option<Vec<u8>> {
        let mut map = self.map();
        let e = map.get_mut(conv)?;
        if e.at.elapsed() >= CONV_TTL {
            map.remove(conv);
            return None;
        }
        e.at = Instant::now();
        // 读一次就刷新序号:淘汰按「最近最少读/写」,正在用的图不会因为写得早被挤掉。
        let seq = e.next_seq;
        let (v, s) = e.files.get_mut(path)?;
        *s = seq;
        e.next_seq += 1;
        Some(v.clone())
    }
}
