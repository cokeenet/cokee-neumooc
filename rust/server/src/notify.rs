//! 通知通道（QQ 群）。
//!
//! 走 DSH-IM 的主动投递接口，与 同类平台 同一条通道：
//!
//! ```text
//! POST http://127.0.0.1:3081/api/dsh-im/delivery/messages
//! { "botId": "...", "targetId": "...", "text": "..." }
//! ```
//!
//! 请求体**严格只接受这三个字段**，多传会被拒。成功返回 `{"sent": true}`。
//!
//! ## 三种模式
//!
//! | 模式 | 行为 |
//! | --- | --- |
//! | `off` | 不发 |
//! | `summary`（默认） | **失败立即发**，成功攒着每天汇总一条 |
//! | `each` | 每条都发 |
//!
//! 为什么默认 summary：例行成功汇报逐条推会把群刷屏，反而没人看。
//!
//! ## `urgent` 绕过模式
//!
//! 考勤告警是**时间敏感**的（签到窗口常只开几分钟），攒到晚上汇总等于没提醒。
//! 所以 `urgent` 不看 mode 直接发 —— 只有 `off` 才真的静默。
//!
//! ## 绝不阻塞、绝不抛错
//!
//! 发通知失败**不能影响主流程**。发送在后台线程里做，失败只记日志。

use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

pub const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:3081/api/dsh-im/delivery/messages";

/// 缓冲上限 —— 长时间不发汇总也不能让内存无限涨。
const MAX_PENDING: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Off,
    Summary,
    Each,
}

impl Mode {
    /// 解析模式。**拼错时退回 summary**，而不是意外变成 off（静默）或 each（刷屏）。
    pub fn parse(raw: &str) -> Mode {
        match raw.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "disable" | "disabled" => Mode::Off,
            "each" | "all" | "verbose" => Mode::Each,
            _ => Mode::Summary,
        }
    }
}

struct Inner {
    endpoint: String,
    bot_id: String,
    target_id: String,
    mode: Mode,
    pending: Mutex<Vec<String>>,
    http: reqwest::blocking::Client,
}

#[derive(Clone)]
pub struct Notifier(Arc<Inner>);

impl Notifier {
    /// 从环境变量构造。未配置 botId/targetId 时返回 `Mode::Off` 的实例。
    pub fn from_env() -> Self {
        let bot = std::env::var("NEUMOOC_QQ_BOT_ID").unwrap_or_default();
        let target = std::env::var("NEUMOOC_QQ_TARGET_ID").unwrap_or_default();
        let endpoint = std::env::var("NEUMOOC_NOTIFY_ENDPOINT")
            .unwrap_or_else(|_| DEFAULT_ENDPOINT.into());
        // 未配全 → 直接关闭。绝不「往空目标发」
        let mode = if bot.trim().is_empty() || target.trim().is_empty() {
            Mode::Off
        } else {
            Mode::parse(&std::env::var("NEUMOOC_NOTIFY_MODE").unwrap_or_else(|_| "summary".into()))
        };
        if mode == Mode::Off {
            log::info!("通知通道：未配置或已关闭");
        } else {
            log::info!("通知通道已启用：mode={mode:?}");
        }
        Self(Arc::new(Inner {
            endpoint,
            bot_id: bot,
            target_id: target,
            mode,
            pending: Mutex::new(Vec::new()),
            http: reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
        }))
    }

    pub fn mode(&self) -> Mode {
        self.0.mode
    }

    pub fn enabled(&self) -> bool {
        self.0.mode != Mode::Off
            && !self.0.bot_id.is_empty()
            && !self.0.target_id.is_empty()
    }

    /// **同步**投递。返回 `(是否成功, 说明)`。
    pub fn send_now(&self, text: &str) -> (bool, String) {
        if !self.enabled() {
            return (false, "通知未启用".into());
        }
        if text.trim().is_empty() {
            return (false, "内容为空".into());
        }
        let body = json!({
            "botId": self.0.bot_id, "targetId": self.0.target_id, "text": text
        });
        let r = match self.0.http.post(&self.0.endpoint).json(&body).send() {
            Ok(r) => r,
            // 发通知失败绝不往上抛
            Err(e) => return (false, format!("投递请求失败：{e}")),
        };
        let status = r.status();
        let txt = r.text().unwrap_or_default();
        if !status.is_success() {
            return (false, format!("投递被拒 HTTP {status}：{}", &txt.chars().take(200).collect::<String>()));
        }
        match serde_json::from_str::<Value>(&txt) {
            // 契约是 {"sent": true}；返回别的说明没真正发出去
            Ok(v) if v.get("sent").and_then(Value::as_bool) == Some(true) => (true, "已投递".into()),
            _ => (false, format!("投递未确认：{}", &txt.chars().take(200).collect::<String>())),
        }
    }

    /// 后台投递，绝不阻塞调用方。
    fn send_bg(&self, text: String) {
        if !self.enabled() {
            return;
        }
        let me = self.clone();
        std::thread::spawn(move || {
            let (ok, detail) = me.send_now(&text);
            if !ok {
                log::warn!("通知投递失败：{detail}");
            }
        });
    }

    /// **立即**发送，不受 summary 模式影响。考勤告警走这条。
    pub fn urgent(&self, text: &str) {
        if self.0.mode == Mode::Off {
            return;
        }
        self.send_bg(text.to_string());
    }

    /// 汇报一次任务结果。
    ///
    /// * `each` —— 无论成败都立即发
    /// * `summary` —— **失败立即发**（这类需要人马上看到），成功攒进缓冲
    pub fn report(&self, label: &str, ok: bool, line: &str, biz_date: &str) {
        if self.0.mode == Mode::Off {
            return;
        }
        let date_part = if biz_date.is_empty() {
            String::new()
        } else {
            format!("\n日期：{biz_date}")
        };
        if !ok {
            self.send_bg(format!("⚠️ 执行失败\n{label}{date_part}\n{line}"));
            return;
        }
        if self.0.mode == Mode::Each {
            self.send_bg(format!("✅ 执行完成\n{label}{date_part}\n{line}"));
            return;
        }
        let item = if biz_date.is_empty() {
            format!("✅ {label}：{line}")
        } else {
            format!("✅ {label}（{biz_date}）：{line}")
        };
        let mut p = self.0.pending.lock().expect("锁中毒");
        p.push(item);
        let overflow = p.len().saturating_sub(MAX_PENDING);
        if overflow > 0 {
            p.drain(..overflow);
        }
    }

    /// **查看**待汇总条数（非破坏性）。
    ///
    /// ⚠️ 同类平台 在这里踩过坑：状态接口用了 `take_pending().len()` ——
    /// 那是**取走**不是**查看**，结果「群里问一句状态，当晚的汇总就没了」，
    /// 而且没有任何报错。凡是要显示数量，一律用这个。
    pub fn pending_len(&self) -> usize {
        self.0.pending.lock().expect("锁中毒").len()
    }

    /// 取出并清空汇总缓冲。**只在真正要发送时调用**。
    pub fn take_pending(&self) -> Vec<String> {
        let mut p = self.0.pending.lock().expect("锁中毒");
        std::mem::take(&mut *p)
    }

    /// 发送汇总。无内容时不发，避免空消息。返回发送的条数。
    pub fn send_summary(&self, date: &str) -> usize {
        if self.0.mode != Mode::Summary {
            return 0;
        }
        let items = self.take_pending();
        if items.is_empty() {
            return 0;
        }
        let show = &items[..items.len().min(30)]; // 群里消息不宜过长
        let mut text = format!("📋 {date} 汇总（{} 项）\n{}", items.len(), show.join("\n"));
        if items.len() > show.len() {
            text.push_str(&format!("\n…另有 {} 项", items.len() - show.len()));
        }
        self.send_bg(text);
        items.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_parsing() {
        assert_eq!(Mode::parse("off"), Mode::Off);
        assert_eq!(Mode::parse("OFF"), Mode::Off);
        assert_eq!(Mode::parse("none"), Mode::Off);
        assert_eq!(Mode::parse("each"), Mode::Each);
        assert_eq!(Mode::parse("all"), Mode::Each);
        assert_eq!(Mode::parse("summary"), Mode::Summary);
    }

    #[test]
    fn typo_falls_back_to_summary() {
        // 拼错时不能意外变成 off（静默）或 each（刷屏）
        for bad in ["", "summry", "???", "  "] {
            assert_eq!(Mode::parse(bad), Mode::Summary, "{bad:?} 应退回 summary");
        }
    }

    #[test]
    fn unconfigured_is_off() {
        // 不读真实环境变量，直接构造一个未配置的实例
        let n = Notifier(Arc::new(Inner {
            endpoint: DEFAULT_ENDPOINT.into(),
            bot_id: String::new(),
            target_id: String::new(),
            mode: Mode::Off,
            pending: Mutex::new(Vec::new()),
            http: reqwest::blocking::Client::new(),
        }));
        assert!(!n.enabled());
        // 任何入口都不该留下痕迹
        n.urgent("紧急");
        n.report("甲", true, "好了", "");
        n.report("甲", false, "炸了", "");
        assert_eq!(n.pending_len(), 0);
        assert_eq!(n.send_summary("2026-09-29"), 0);
        assert!(!n.send_now("x").0);
    }

    fn test_notifier(mode: Mode) -> Notifier {
        Notifier(Arc::new(Inner {
            endpoint: "http://127.0.0.1:1/never".into(),
            bot_id: "b".into(),
            target_id: "t".into(),
            mode,
            pending: Mutex::new(Vec::new()),
            // 指向一个必然连不上的地址，send_bg 里的失败只会记日志
            http: reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_millis(50))
                .build()
                .unwrap(),
        }))
    }

    #[test]
    fn summary_buffers_success_and_sends_failure_immediately() {
        let n = test_notifier(Mode::Summary);
        n.report("甲", true, "刷完 3 个", "2026-09-29");
        n.report("乙", true, "刷完 2 个", "");
        assert_eq!(n.pending_len(), 2, "成功应进缓冲");
        n.report("丙", false, "登录失败", "");
        assert_eq!(n.pending_len(), 2, "失败不该进缓冲");
    }

    #[test]
    fn pending_len_is_not_destructive() {
        // **回归**：查看数量不能清空缓冲。
        // 同类平台 曾用 take_pending().len() 做状态接口 ——
        // 「群里问一句状态，当晚的汇总就没了」，且无任何报错。
        let n = test_notifier(Mode::Summary);
        n.report("甲", true, "a", "");
        n.report("乙", true, "b", "");
        assert_eq!(n.pending_len(), 2);
        assert_eq!(n.pending_len(), 2, "查看不该清空");
        assert_eq!(n.pending_len(), 2);
        assert_eq!(n.take_pending().len(), 2, "只有 take 才清空");
        assert_eq!(n.pending_len(), 0);
    }

    #[test]
    fn buffer_is_bounded() {
        let n = test_notifier(Mode::Summary);
        for i in 0..MAX_PENDING + 50 {
            n.report("甲", true, &i.to_string(), "");
        }
        assert_eq!(n.pending_len(), MAX_PENDING);
    }

    #[test]
    fn empty_summary_sends_nothing() {
        let n = test_notifier(Mode::Summary);
        assert_eq!(n.send_summary("2026-09-29"), 0);
    }

    #[test]
    fn summary_truncates_long_list() {
        let n = test_notifier(Mode::Summary);
        for i in 0..40 {
            n.report("甲", true, &format!("第{i}项"), "");
        }
        assert_eq!(n.send_summary("2026-09-29"), 40);
        assert_eq!(n.pending_len(), 0);
    }

    #[test]
    fn each_mode_does_not_buffer() {
        let n = test_notifier(Mode::Each);
        n.report("甲", true, "好了", "");
        n.report("乙", false, "炸了", "");
        assert_eq!(n.pending_len(), 0, "each 模式不该攒");
    }
}
