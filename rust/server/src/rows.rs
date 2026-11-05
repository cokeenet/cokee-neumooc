//! 把 `rusqlite::Row` 转成 JSON。
//!
//! Python 版这些接口大多直接 `[dict(r) for r in rows]` —— 返回的就是**列名到值**。
//! Rust 没有这个便利，但也不需要给每张表写一个结构体：前端本来就按列名取值。
//!
//! 所以这里做一件事：**按列名取值、按 SQLite 的实际类型转 JSON**。
//! `NULL` 转 `null`（不是空串）—— 前端靠它区分「没有」和「空」。

use rusqlite::types::ValueRef;
use rusqlite::{Connection, Result as SqlResult};
use serde_json::{Map, Value};

/// 执行查询并把每行转成 JSON 对象。
pub fn query_json(
    conn: &Connection,
    sql: &str,
    params: &[&dyn rusqlite::ToSql],
) -> SqlResult<Vec<Value>> {
    let mut stmt = conn.prepare(sql)?;
    let names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let mut rows = stmt.query(params)?;
    let mut out = Vec::new();
    while let Some(r) = rows.next()? {
        let mut m = Map::with_capacity(names.len());
        for (i, name) in names.iter().enumerate() {
            let v = match r.get_ref(i)? {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(n) => Value::from(n),
                ValueRef::Real(f) => Value::from(f),
                ValueRef::Text(t) => Value::from(String::from_utf8_lossy(t).into_owned()),
                // BLOB 不直接吐给前端（凭据密文就在这里），转成占位说明
                ValueRef::Blob(b) => Value::from(format!("<blob {} bytes>", b.len())),
            };
            m.insert(name.clone(), v);
        }
        out.push(Value::Object(m));
    }
    Ok(out)
}

/// 查一行，转成 JSON 对象；没有则 `None`。
pub fn query_one_json(
    conn: &Connection,
    sql: &str,
    params: &[&dyn rusqlite::ToSql],
) -> SqlResult<Option<Value>> {
    Ok(query_json(conn, sql, params)?.into_iter().next())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    #[test]
    fn maps_types_and_preserves_null() {
        let c = db::open_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE t (i INTEGER, r REAL, s TEXT, b BLOB, n TEXT);
             INSERT INTO t VALUES (42, 1.5, 'hi', x'0102', NULL);",
        )
        .unwrap();
        let rows = query_json(&c, "SELECT * FROM t", &[]).unwrap();
        assert_eq!(rows.len(), 1);
        let o = rows[0].as_object().unwrap();
        assert_eq!(o["i"], 42);
        assert_eq!(o["r"], 1.5);
        assert_eq!(o["s"], "hi");
        // **NULL 必须是 null，不是空串** —— 前端靠它区分「没有」和「空」
        assert!(o["n"].is_null(), "NULL 应转成 null");
        // BLOB 不外泄（凭据密文就是 BLOB）
        assert!(o["b"].as_str().unwrap().starts_with("<blob"));
    }

    #[test]
    fn column_names_are_used_as_keys() {
        let c = db::open_memory().unwrap();
        c.execute_batch("CREATE TABLE t (account_id INTEGER, res_name TEXT); INSERT INTO t VALUES (7,'x');")
            .unwrap();
        let rows = query_json(&c, "SELECT * FROM t", &[]).unwrap();
        let o = rows[0].as_object().unwrap();
        assert!(o.contains_key("account_id"));
        assert!(o.contains_key("res_name"));
    }

    #[test]
    fn query_one_returns_none_when_empty() {
        let c = db::open_memory().unwrap();
        c.execute_batch("CREATE TABLE t (a INTEGER);").unwrap();
        assert!(query_one_json(&c, "SELECT * FROM t", &[]).unwrap().is_none());
    }
}
