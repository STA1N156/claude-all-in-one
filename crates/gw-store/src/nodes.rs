//! 子机(dario 节点)表:令牌摘要、机器绑定、禁用、最近一次见到;以及子机离线时的待删号。
//!
//! 只新增表,不改已有表:还没升级的老镜像不读它们,完全不受影响。

use rusqlite::OptionalExtension;

use crate::SqliteStore;

pub(crate) const NODES_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS nodes (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL DEFAULT '',
    token_hash  TEXT NOT NULL,
    install_id  TEXT,
    disabled    INTEGER NOT NULL DEFAULT 0,
    created_at  INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    last_seen   INTEGER,
    last_ip     TEXT,
    version     TEXT,
    info_json   TEXT
);
CREATE TABLE IF NOT EXISTS node_tombstones (
    node_id     TEXT NOT NULL,
    account_id  TEXT NOT NULL,
    created_at  INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    PRIMARY KEY (node_id, account_id)
);
"#;

/// `settings` 表里 hub 身份(证书指纹 + 对外地址)的键。
pub const KEY_NODE_HUB_IDENTITY: &str = "node_hub_identity";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRow {
    pub id: String,
    pub name: String,
    pub token_hash: String,
    pub install_id: Option<String>,
    pub disabled: bool,
    pub created_at: i64,
    pub last_seen: Option<i64>,
    pub last_ip: Option<String>,
    pub version: Option<String>,
    pub info_json: Option<String>,
}

/// 首次连接时绑定机器的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindOutcome {
    /// 刚绑定,或本来就是这台机器。
    Ok,
    /// 节点不存在(已吊销)。
    NotFound,
    /// 已绑定另一台机器。
    Mismatch,
}

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<NodeRow> {
    Ok(NodeRow {
        id: r.get(0)?,
        name: r.get(1)?,
        token_hash: r.get(2)?,
        install_id: r.get(3)?,
        disabled: r.get::<_, i64>(4)? != 0,
        created_at: r.get(5)?,
        last_seen: r.get(6)?,
        last_ip: r.get(7)?,
        version: r.get(8)?,
        info_json: r.get(9)?,
    })
}

const COLS: &str = "id, name, token_hash, install_id, disabled, created_at, last_seen, last_ip, version, info_json";

impl SqliteStore {
    pub fn create_node(&self, id: &str, name: &str, token_hash: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock();
        let n = conn.execute(
            "INSERT OR IGNORE INTO nodes (id, name, token_hash) VALUES (?1, ?2, ?3)",
            (id, name, token_hash),
        )?;
        Ok(n == 1)
    }

    pub fn list_nodes(&self) -> anyhow::Result<Vec<NodeRow>> {
        let conn = self.conn.lock();
        let mut st = conn.prepare(&format!("SELECT {COLS} FROM nodes ORDER BY created_at, id"))?;
        let rows = st.query_map([], row)?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn get_node(&self, id: &str) -> anyhow::Result<Option<NodeRow>> {
        let conn = self.conn.lock();
        Ok(conn
            .query_row(&format!("SELECT {COLS} FROM nodes WHERE id = ?1"), [id], row)
            .optional()?)
    }

    /// 改名 / 禁用。`false` = 不存在。
    pub fn update_node(&self, id: &str, name: Option<&str>, disabled: Option<bool>) -> anyhow::Result<bool> {
        let conn = self.conn.lock();
        let mut changed = conn
            .prepare_cached("SELECT 1 FROM nodes WHERE id = ?1")?
            .exists([id])?;
        if let Some(n) = name {
            changed = conn.execute("UPDATE nodes SET name = ?1 WHERE id = ?2", (n, id))? == 1;
        }
        if let Some(d) = disabled {
            changed = conn.execute("UPDATE nodes SET disabled = ?1 WHERE id = ?2", (d as i64, id))? == 1;
        }
        Ok(changed)
    }

    /// 吊销:删掉节点与它的待删记录。hub 5 秒内断开它。
    pub fn delete_node(&self, id: &str) -> anyhow::Result<bool> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let n = tx.execute("DELETE FROM nodes WHERE id = ?1", [id])?;
        tx.execute("DELETE FROM node_tombstones WHERE node_id = ?1", [id])?;
        tx.commit()?;
        Ok(n == 1)
    }

    /// 首次连接绑定机器;已绑定则必须是同一台。
    pub fn bind_node_install(&self, id: &str, install_id: &str) -> anyhow::Result<BindOutcome> {
        let conn = self.conn.lock();
        let cur: Option<Option<String>> = conn
            .query_row("SELECT install_id FROM nodes WHERE id = ?1", [id], |r| r.get(0))
            .optional()?;
        match cur {
            None => Ok(BindOutcome::NotFound),
            Some(Some(b)) if !b.is_empty() => Ok(if b == install_id { BindOutcome::Ok } else { BindOutcome::Mismatch }),
            Some(_) => {
                // 条件更新:并发的两台机器只有一台能绑上。
                let n = conn.execute(
                    "UPDATE nodes SET install_id = ?1 WHERE id = ?2 AND (install_id IS NULL OR install_id = '')",
                    (install_id, id),
                )?;
                if n == 1 {
                    return Ok(BindOutcome::Ok);
                }
                let now: Option<String> =
                    conn.query_row("SELECT install_id FROM nodes WHERE id = ?1", [id], |r| r.get(0))?;
                Ok(if now.as_deref() == Some(install_id) { BindOutcome::Ok } else { BindOutcome::Mismatch })
            }
        }
    }

    /// 解除机器绑定(子机重装系统后,下次连接重新绑定)。
    pub fn reset_node_install(&self, id: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock();
        Ok(conn.execute("UPDATE nodes SET install_id = NULL WHERE id = ?1", [id])? == 1)
    }

    pub fn touch_node(&self, id: &str, ip: &str, version: &str, info_json: Option<&str>) -> anyhow::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE nodes SET last_seen = strftime('%s','now'), last_ip = ?1, version = ?2, \
             info_json = COALESCE(?3, info_json) WHERE id = ?4",
            (ip, version, info_json, id),
        )?;
        Ok(())
    }

    /// 绑在某个子机上的号的数量(`extra.dario_node`)。
    pub fn count_node_accounts(&self) -> anyhow::Result<std::collections::HashMap<String, i64>> {
        let conn = self.conn.lock();
        let mut st = conn.prepare(
            "SELECT json_extract(extra, '$.dario_node') AS n, COUNT(*) FROM accounts \
             WHERE json_valid(extra) AND n IS NOT NULL AND n <> '' GROUP BY n",
        )?;
        let rows = st
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    pub fn add_node_tombstone(&self, node_id: &str, account_id: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT OR IGNORE INTO node_tombstones (node_id, account_id) VALUES (?1, ?2)",
            (node_id, account_id),
        )?;
        Ok(())
    }

    pub fn list_node_tombstones(&self, node_id: &str) -> anyhow::Result<Vec<String>> {
        let conn = self.conn.lock();
        let mut st = conn.prepare("SELECT account_id FROM node_tombstones WHERE node_id = ?1 ORDER BY created_at")?;
        let rows = st.query_map([node_id], |r| r.get(0))?.collect::<Result<_, _>>()?;
        Ok(rows)
    }

    pub fn remove_node_tombstone(&self, node_id: &str, account_id: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "DELETE FROM node_tombstones WHERE node_id = ?1 AND account_id = ?2",
            (node_id, account_id),
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_lifecycle_and_binding() {
        let s = SqliteStore::open_in_memory().unwrap();
        assert!(s.create_node("n-1", "东京", "h1").unwrap());
        assert!(!s.create_node("n-1", "dup", "h2").unwrap(), "同 ID 不覆盖");
        assert_eq!(s.bind_node_install("n-1", "A").unwrap(), BindOutcome::Ok);
        assert_eq!(s.bind_node_install("n-1", "A").unwrap(), BindOutcome::Ok);
        assert_eq!(s.bind_node_install("n-1", "B").unwrap(), BindOutcome::Mismatch);
        assert_eq!(s.bind_node_install("n-x", "A").unwrap(), BindOutcome::NotFound);
        assert!(s.reset_node_install("n-1").unwrap());
        assert_eq!(s.bind_node_install("n-1", "B").unwrap(), BindOutcome::Ok);

        assert!(s.update_node("n-1", Some("大阪"), Some(true)).unwrap());
        assert!(!s.update_node("n-x", None, Some(true)).unwrap());
        s.touch_node("n-1", "198.51.100.2", "v1", Some("{\"a\":1}")).unwrap();
        s.touch_node("n-1", "198.51.100.2", "v1", None).unwrap();
        let n = s.get_node("n-1").unwrap().unwrap();
        assert_eq!(n.name, "大阪");
        assert!(n.disabled);
        assert_eq!(n.last_ip.as_deref(), Some("198.51.100.2"));
        assert_eq!(n.info_json.as_deref(), Some("{\"a\":1}"), "没带新状态时保留旧的");

        s.add_node_tombstone("n-1", "a@x").unwrap();
        s.add_node_tombstone("n-1", "a@x").unwrap();
        assert_eq!(s.list_node_tombstones("n-1").unwrap(), vec!["a@x"]);
        s.remove_node_tombstone("n-1", "a@x").unwrap();
        assert!(s.list_node_tombstones("n-1").unwrap().is_empty());

        s.add_node_tombstone("n-1", "b@x").unwrap();
        assert!(s.delete_node("n-1").unwrap());
        assert!(s.get_node("n-1").unwrap().is_none());
        assert!(s.list_node_tombstones("n-1").unwrap().is_empty());
    }

    #[test]
    fn counts_accounts_per_node() {
        let s = SqliteStore::open_in_memory().unwrap();
        s.create_account("a", "", "claude-dario", 1, r#"{"dario_node":"n-1"}"#).unwrap();
        s.create_account("b", "", "claude-dario", 1, r#"{"dario_node":"n-1"}"#).unwrap();
        s.create_account("c", "", "claude-dario", 1, r#"{"dario_node":""}"#).unwrap();
        s.create_account("d", "", "kiro", 1, r#"{}"#).unwrap();
        let m = s.count_node_accounts().unwrap();
        assert_eq!(m.get("n-1"), Some(&2));
        assert_eq!(m.len(), 1);
    }
}

#[cfg(test)]
mod node_lock_tests {
    use crate::{AccountExtraEdit, SqliteStore, UpdateAccountOutcome};
    use gw_core::store::AccountPatch;

    fn edit(s: &SqliteStore, extra: &str, mode: AccountExtraEdit) -> UpdateAccountOutcome {
        let patch = AccountPatch { extra: Some(extra.to_string()), ..Default::default() };
        s.update_account_from_admin("a", &patch, mode).unwrap()
    }

    #[test]
    fn dario_node_is_write_once() {
        let s = SqliteStore::open_in_memory().unwrap();
        s.create_account("a", "", "claude-dario", 1, r#"{"dario_node":"n-1","dario_rev":"rt-1","note":"x"}"#).unwrap();
        let replace = AccountExtraEdit::Replace { allow_credential_drop: false };
        // Replace 没带这个键:保留原值,不报错。
        assert_eq!(edit(&s, r#"{"dario_rev":"rt-1","note":"y"}"#, replace), UpdateAccountOutcome::Ok);
        let row = s.get_account("a").unwrap().unwrap();
        assert!(row.extra.contains("\"dario_node\":\"n-1\""), "{}", row.extra);
        // 改成别的子机 / 清空:拒绝。
        assert_eq!(edit(&s, r#"{"dario_node":"n-2"}"#, AccountExtraEdit::Merge), UpdateAccountOutcome::NodeLocked);
        assert_eq!(edit(&s, r#"{"dario_node":""}"#, AccountExtraEdit::Merge), UpdateAccountOutcome::NodeLocked);
        // 同值:放行。
        assert_eq!(edit(&s, r#"{"dario_node":"n-1"}"#, AccountExtraEdit::Merge), UpdateAccountOutcome::Ok);

        // 本机号不能事后挂到子机上。
        s.create_account("b", "", "claude-dario", 1, r#"{"dario_rev":"rt-2"}"#).unwrap();
        let patch = AccountPatch { extra: Some(r#"{"dario_node":"n-1"}"#.into()), ..Default::default() };
        assert_eq!(
            s.update_account_from_admin("b", &patch, AccountExtraEdit::Merge).unwrap(),
            UpdateAccountOutcome::NodeLocked
        );
    }

    #[test]
    fn merge_cannot_move_node_or_add_proxy() {
        let s = SqliteStore::open_in_memory().unwrap();
        s.create_account("a", "", "claude-dario", 1, r#"{"dario_node":"n-1"}"#).unwrap();
        assert!(s.merge_account_extra("a", r#"{"proxy":"http://p:1"}"#).is_err(), "子机号不能配代理");
        assert!(s.merge_account_extra("a", r#"{"proxy":null}"#).is_ok(), "清代理可以");
        assert!(s.merge_account_extra("a", r#"{"dario_node":"n-2"}"#).is_err());
        assert!(s.merge_account_extra("a", r#"{"priority":5}"#).is_ok());
        s.create_account("b", "", "kiro", 1, r#"{}"#).unwrap();
        assert!(s.merge_account_extra("b", r#"{"proxy":"http://p:1"}"#).is_ok(), "本机号照旧");
        assert!(s.merge_account_extra("b", r#"{"dario_node":"n-1"}"#).is_err(), "不能事后挂子机");
    }
}
