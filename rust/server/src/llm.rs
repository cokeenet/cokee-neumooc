//! LLM 客户端：思考（reasoning）、思考强度、流式、图片。
//!
//! 配置思路对齐 DSH 的 provider/model 两层结构：
//!
//! ```text
//! provider: { base_url, api_key, api, compat }
//! model:    { id, context_window, max_tokens, input: [text, image], reasoning_efforts }
//! ```
//!
//! 但我们只做一道题/一张卷子，不需要 DSH 那么全 —— 只保留**影响答题结果**的几项：
//!
//! * `reasoning_effort` —— 思考强度（off/low/medium/high/max）
//! * `stream`           —— 是否流式（长答案时能更早看到进度）
//! * `input`            —— 是否支持图片（题干/选项里带图时用）
//!
//! ## 关于 reasoning 的兼容性
//!
//! 不同厂商落点不一样，这里按 Qwen/GLM/DeepSeek 常见的三种都发：
//!
//! ```json
//! { "reasoning_effort": "high",              // OpenAI 风格
//!   "thinking": { "type": "enabled" },       // 部分国内网关
//!   "enable_thinking": true }                // 阿里/百炼风格
//! ```
//!
//! 多发的字段会被忽略，**不发才真的不生效**。

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// 思考强度。对齐 DSH 的 `reasoningEfforts` 取值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Max,
}

impl Default for Effort {
    fn default() -> Self {
        Effort::Medium
    }
}

impl Effort {
    pub fn as_str(self) -> &'static str {
        match self {
            Effort::Off => "off",
            Effort::Minimal => "minimal",
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::Max => "max",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "disabled" => Effort::Off,
            "minimal" | "min" => Effort::Minimal,
            "low" => Effort::Low,
            "medium" | "mid" | "default" => Effort::Medium,
            "high" => Effort::High,
            "max" => Effort::Max,
            _ => return None,
        })
    }
}

/// 一条消息。`content` 可以是纯文本，也可以是 OpenAI 多模态数组。
#[derive(Debug, Clone)]
pub struct Message {
    pub role: &'static str,
    pub content: Value,
}

impl Message {
    pub fn user_text(text: impl Into<String>) -> Self {
        Self { role: "user", content: json!(text.into()) }
    }

    /// 带图片的 user 消息（OpenAI 多模态格式）。
    ///
    /// `images` 是 data URL（`data:image/png;base64,...`）或公网 URL。
    pub fn user_text_with_images(text: &str, images: &[String]) -> Self {
        if images.is_empty() {
            return Self::user_text(text);
        }
        let mut parts = vec![json!({ "type": "text", "text": text })];
        for img in images {
            parts.push(json!({
                "type": "image_url",
                "image_url": { "url": img }
            }));
        }
        Self { role: "user", content: json!(parts) }
    }
}

/// 一次调用的结果。
#[derive(Debug, Default)]
pub struct Completion {
    /// 最终答案
    pub content: String,
    /// 思考过程（有的厂商单独放在 `reasoning_content`）
    pub reasoning: String,
    /// 实际用的模型（网关可能改写）
    pub model: String,
    /// token 用量
    pub usage: Value,
}

/// 调用参数。
#[derive(Debug, Clone)]
pub struct Options {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub effort: Effort,
    /// 是否请求流式（内部会聚合成完整结果返回）
    pub stream: bool,
    pub max_tokens: Option<u32>,
    pub timeout_secs: u64,
}

impl Options {
    /// 请求体。**兼容性优先**：多发的字段被忽略，不发才不生效。
    fn body(&self, messages: &[Message]) -> Value {
        let mut b = json!({
            "model": self.model,
            "messages": messages.iter().map(|m| json!({
                "role": m.role,
                "content": m.content,
            })).collect::<Vec<_>>(),
            // 答题要稳定结果，不要创意
            "temperature": 0,
            "stream": self.stream,
        });
        if let Some(mt) = self.max_tokens {
            b["max_tokens"] = json!(mt);
        }
        match self.effort {
            // off：三种关法都发，总有一个厂商认
            Effort::Off => {
                b["reasoning_effort"] = json!("none");
                b["thinking"] = json!({ "type": "disabled" });
                b["enable_thinking"] = json!(false);
            }
            e => {
                b["reasoning_effort"] = json!(e.as_str());
                b["thinking"] = json!({ "type": "enabled" });
                b["enable_thinking"] = json!(true);
            }
        }
        b
    }
}

/// 流式回调：拿到一段增量就调一次。
///
/// * `reasoning` = true 表示这段是**思考过程**（`reasoning_content`），
///   界面上应该用不同的样式展示（灰字/折叠），别和答案混在一起
/// * `text` 是本次增量（不是累积）
/// 流式回调（`reasoning=true` 表示这是思考过程）。
pub trait DeltaSink {
    fn delta(&mut self, reasoning: bool, text: &str);
}

impl<F: FnMut(bool, &str)> DeltaSink for F {
    fn delta(&mut self, reasoning: bool, text: &str) {
        self(reasoning, text)
    }
}

/// 发一次请求。`stream=true` 时内部读 SSE 并聚合成完整结果。
pub fn complete(opts: &Options, messages: &[Message]) -> Result<Completion, String> {
    complete_streaming(opts, messages, None)
}

/// 同上，但每收到一段增量就回调一次 —— 用于把思考/答案**实时**推给界面。
///
/// 注意：即使 `opts.stream=false`，这个函数也会在最后回调一次完整内容，
/// 这样调用方不用写两条路径。
pub fn complete_streaming(
    opts: &Options,
    messages: &[Message],
    mut on_delta: Option<&mut dyn DeltaSink>,
) -> Result<Completion, String> {
    let url = format!("{}/chat/completions", opts.base_url.trim_end_matches('/'));
    let http = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(opts.timeout_secs))
        .build()
        .map_err(|e| format!("HTTP 客户端构造失败：{e}"))?;

    let resp = http
        .post(&url)
        .header("Authorization", format!("Bearer {}", opts.api_key))
        .header("Content-Type", "application/json")
        .json(&opts.body(messages))
        .send()
        .map_err(|e| format!("LLM 请求失败：{e}"))?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().unwrap_or_default();
        return Err(format!(
            "HTTP {status}：{}",
            text.chars().take(300).collect::<String>()
        ));
    }

    if opts.stream {
        parse_sse(resp, on_delta)
    } else {
        let text = resp.text().map_err(|e| format!("读响应失败：{e}"))?;
        let c = parse_json(&text)?;
        // 非流式也回调一次，调用方只写一条路径
        if let Some(cb) = on_delta.as_mut() {
            if !c.reasoning.is_empty() {
                cb.delta(true, &c.reasoning);
            }
            if !c.content.is_empty() {
                cb.delta(false, &c.content);
            }
        }
        Ok(c)
    }
}

fn parse_json(text: &str) -> Result<Completion, String> {
    let v: Value = serde_json::from_str(text)
        .map_err(|_| format!("响应不是 JSON：{}", text.chars().take(200).collect::<String>()))?;
    let choice = v
        .get("choices")
        .and_then(|c| c.get(0))
        .ok_or_else(|| format!("响应里没有 choices：{}", text.chars().take(200).collect::<String>()))?;
    let msg = choice.get("message").cloned().unwrap_or(Value::Null);
    Ok(Completion {
        content: msg.get("content").and_then(Value::as_str).unwrap_or("").to_string(),
        reasoning: msg
            .get("reasoning_content")
            .or_else(|| msg.get("reasoning"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        model: v.get("model").and_then(Value::as_str).unwrap_or("").to_string(),
        usage: v.get("usage").cloned().unwrap_or(Value::Null),
    })
}

/// 读 SSE 流并聚合。
///
/// 只取 `choices[0].delta` 里的 `content` 与 `reasoning_content` ——
/// 界面上要展示的是"AI 想了什么、答了什么"，其余事件（role/心跳）忽略。
fn parse_sse(
    resp: reqwest::blocking::Response,
    mut on_delta: Option<&mut dyn DeltaSink>,
) -> Result<Completion, String> {
    use std::io::{BufRead, BufReader};
    let reader = BufReader::new(resp);
    let mut out = Completion::default();
    for line in reader.lines() {
        let line = line.map_err(|e| format!("读流失败：{e}"))?;
        let Some(rest) = line.strip_prefix("data:") else { continue };
        let data = rest.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else { continue };
        if out.model.is_empty() {
            if let Some(m) = v.get("model").and_then(Value::as_str) {
                out.model = m.to_string();
            }
        }
        if v.get("usage").is_some() {
            out.usage = v["usage"].clone();
        }
        let Some(delta) = v.get("choices").and_then(|c| c.get(0)).and_then(|c| c.get("delta")) else {
            continue;
        };
        if let Some(c) = delta.get("content").and_then(Value::as_str) {
            if !c.is_empty() {
                out.content.push_str(c);
                if let Some(cb) = on_delta.as_mut() {
                    cb.delta(false, c);
                }
            }
        }
        if let Some(r) = delta
            .get("reasoning_content")
            .or_else(|| delta.get("reasoning"))
            .and_then(Value::as_str)
        {
            if !r.is_empty() {
                out.reasoning.push_str(r);
                if let Some(cb) = on_delta.as_mut() {
                    cb.delta(true, r);
                }
            }
        }
    }
    if out.content.is_empty() && out.reasoning.is_empty() {
        return Err("流式响应里没有内容（可能是模型不支持 stream，关掉试试）".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effort_roundtrip() {
        for e in [Effort::Off, Effort::Minimal, Effort::Low, Effort::Medium, Effort::High, Effort::Max] {
            assert_eq!(Effort::parse(e.as_str()), Some(e));
        }
        assert_eq!(Effort::parse("none"), Some(Effort::Off));
        assert_eq!(Effort::parse("HIGH"), Some(Effort::High));
        assert_eq!(Effort::parse("乱写"), None);
    }

    #[test]
    fn off_disables_all_three_styles() {
        let o = Options {
            base_url: "http://x".into(),
            api_key: "k".into(),
            model: "m".into(),
            effort: Effort::Off,
            stream: false,
            max_tokens: None,
            timeout_secs: 1,
        };
        let b = o.body(&[Message::user_text("hi")]);
        // 三种关法都要发出去，总有一个厂商认
        assert_eq!(b["reasoning_effort"], "none");
        assert_eq!(b["thinking"]["type"], "disabled");
        assert_eq!(b["enable_thinking"], false);
    }

    #[test]
    fn effort_enables_thinking() {
        let o = Options {
            base_url: "http://x".into(),
            api_key: "k".into(),
            model: "m".into(),
            effort: Effort::High,
            stream: true,
            max_tokens: Some(100),
            timeout_secs: 1,
        };
        let b = o.body(&[Message::user_text("hi")]);
        assert_eq!(b["reasoning_effort"], "high");
        assert_eq!(b["thinking"]["type"], "enabled");
        assert_eq!(b["enable_thinking"], true);
        assert_eq!(b["temperature"], 0);
        assert_eq!(b["stream"], true);
        assert_eq!(b["max_tokens"], 100);
    }

    #[test]
    fn images_become_multimodal_parts() {
        let m = Message::user_text_with_images(
            "看图答题",
            &["data:image/png;base64,AAA".to_string()],
        );
        let arr = m.content.as_array().expect("多模态应是数组");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["type"], "text");
        assert_eq!(arr[1]["type"], "image_url");
        assert_eq!(arr[1]["image_url"]["url"], "data:image/png;base64,AAA");
    }

    #[test]
    fn no_images_stays_plain_text() {
        let m = Message::user_text_with_images("纯文字", &[]);
        assert!(m.content.is_string());
    }
}
