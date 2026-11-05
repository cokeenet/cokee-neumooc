//! 反序列化容错 —— **真实接口比文档脏**。
//!
//! 这三条都是被真实响应打出来的，Python 版靠弱类型天然容忍，Rust 必须在类型上写明：
//!
//! | 现象 | 实例 | 用哪个 |
//! | --- | --- | --- |
//! | 字符串字段返回 `null` | `title` / `courseName` | [`de_str`] |
//! | 数字字段返回 `null` | `type` / `studyStatus` | [`de_i64`] |
//! | 坐标有时数字有时字符串 | `latitude=38.8977`（浮点） | [`de_num_or_str`] |
//!
//! ⚠️ 还有一条**不在本模块**但同样致命：字段名是驼峰（`attendanceId`），
//! 漏了 `rename` **不报错，只静默变空串**。那条只能靠回归测试钉住。

use serde_json::Value;

/// 字符串：`null` → `""`，数字/布尔也收成字符串。
pub fn de_str<'de, D>(d: D) -> std::result::Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    Ok(match Option::<Value>::deserialize(d)? {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s,
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(other) => other.to_string(),
    })
}

/// 整数：`null` → `0`，字符串数字也收。
pub fn de_i64<'de, D>(d: D) -> std::result::Result<i64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    Ok(match Option::<Value>::deserialize(d)? {
        None | Some(Value::Null) => 0,
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(0),
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        Some(Value::Bool(b)) => i64::from(b),
        _ => 0,
    })
}

/// 可选整数：`null` → `None`。
pub fn de_opt_i64<'de, D>(d: D) -> std::result::Result<Option<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    Ok(match Option::<Value>::deserialize(d)? {
        None | Some(Value::Null) => None,
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Some(Value::String(s)) => s.parse().ok(),
        _ => None,
    })
}

/// 坐标：数字或字符串都收成 `Option<String>`，`null` → `None`。
pub fn de_num_or_str<'de, D>(d: D) -> std::result::Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    Ok(match Option::<Value>::deserialize(d)? {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s),
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(other) => Some(other.to_string()),
    })
}

/// 数字或字符串 → f64。
///
/// 平台的分数/得分字段**混用**：`finalScore: 91.0`（数字）但
/// `paperScore: "100"`（字符串），同一个响应里都有。按 f64 硬解会直接失败。
pub fn de_f64<'de, D>(d: D) -> std::result::Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    let v = Option::<serde_json::Value>::deserialize(d)?;
    Ok(match v {
        Some(serde_json::Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(serde_json::Value::String(s)) => s.trim().parse().unwrap_or(0.0),
        _ => 0.0,
    })
}
