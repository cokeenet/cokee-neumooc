//! 请求层：统一请求头、响应外壳解包、**以及自动重登重试**。
//!
//! ## 为什么 `with_reauth` 是必需品而不是可选糖
//!
//! 这个平台的 token 会被**服务端提前吊销**，而本地 `expiresTime` 还显示有效。
//! 本项目为此踩过三次（Python 版 `task_answer` / `_cmd_tasks` / `_pending_tasks`），
//! 症状都是「同一个 client 上，前一个请求能过、后一个 401」。
//!
//! 所以这层刻意做成：**凭据交给 `Client`，`with_reauth` 自己重登**。
//! 调用方不需要（也没机会）忘记包重试。

use std::collections::BTreeMap;

use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::session::Session;
use crate::signing::build_id_code;

pub const WEB_API: &str = "/web-api";

/// 东软接口客户端。**阻塞式**：协议层够简单，不值得为它引入 async 传染。
pub struct Client {
    http: reqwest::blocking::Client,
    /// 业务域名，如 `https://study.example.edu.cn`
    base: String,
    tenant_id: String,
    session: Session,
    /// 自动重登用的凭据。`None` 时 `with_reauth` 无法重登，会直接抛出原错误。
    creds: Option<(String, String)>,
}

impl Client {
    /// **主动限速**：两个请求之间至少间隔 `MIN_GAP_MS`。
    ///
    /// ## 为什么需要
    ///
    /// 学校平台对我们的出口 IP 做了封禁（平台所在网段整段不通，
    /// 而从 VPS 访问正常）—— 起因大概率是请求过密：
    /// 10 个账号 × 每分钟一次考勤查询 = 每天 14400 次，这个量级像爬虫。
    ///
    /// 限速是**最后一道保险**：即使上层调度配错了间隔，
    /// 也不会瞬间打出一堆请求。代价是批量操作会慢一点，值得。
    ///
    /// 用全局静态锁而不是实例字段 —— 多个账号的 client 是各自创建的，
    /// 按实例限速等于没限（10 个实例各打各的）。
    /// 可用 `NEUMOOC_MIN_GAP_MS` 调整，`0` 关闭。
    fn throttle() {
        use std::sync::{Mutex, OnceLock};
        use std::time::{Duration, Instant};

        static LAST: OnceLock<Mutex<Instant>> = OnceLock::new();
        let min_gap_ms: u64 = std::env::var("NEUMOOC_MIN_GAP_MS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(200); // 默认 200ms → 每秒最多 5 个请求
        if min_gap_ms == 0 {
            return;
        }
        let lock = LAST.get_or_init(|| Mutex::new(Instant::now() - Duration::from_secs(60)));
        if let Ok(mut last) = lock.lock() {
            let gap = Duration::from_millis(min_gap_ms);
            let elapsed = last.elapsed();
            if elapsed < gap {
                std::thread::sleep(gap - elapsed);
            }
            *last = Instant::now();
        }
    }

    pub fn new(base: impl Into<String>, tenant_id: impl Into<String>) -> Result<Self> {
        let mut builder = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .user_agent(
                "Mozilla/5.0 (Linux; Android 13) AppleWebKit/537.36 \
                 (KHTML, like Gecko) Chrome/120.0 Mobile Safari/537.36",
            );

        // **可选出口代理**。
        //
        // ## 为什么需要
        //
        // 校园网出口 IP 会被学校 WAF 封（实测整段不通），
        // 而从 VPS 访问同一平台完全正常。让请求经 VPS 转发后，
        // 学校看到的源 IP 就与校园网脱钩。
        //
        // ## 为什么只给这个 client 加
        //
        // `core::Client` **只用于学校平台**。LLM 调用在别处自己建 client，
        // 不受影响 —— 否则开代理后连国内 LLM 都要绕一圈。
        //
        // ## 配置
        //
        // `NEUMOOC_PROXY`，形如 `socks5://HOST:PORT` 或 `http://HOST:PORT`
        // （实际地址由部署方填，不进仓库）。
        // 空值 = 直连（默认）。**注意**：走代理时不要开
        // `danger_accept_invalid_certs` —— 那会让中间人攻击变得可能。
        if let Ok(proxy) = std::env::var("NEUMOOC_PROXY") {
            let proxy = proxy.trim();
            if !proxy.is_empty() {
                let p = reqwest::Proxy::all(proxy)
                    .map_err(|e| Error::Invalid(format!("代理配置无效（{proxy}）：{e}")))?;
                builder = builder.proxy(p);
                log_once_proxy(proxy);
            }
        }
        let http = builder.build()?;
        Ok(Self {
            http,
            base: base.into().trim_end_matches('/').to_string(),
            tenant_id: tenant_id.into(),
            session: Session::default(),
            creds: None,
        })
    }

    /// 取一个**二进制资源**（图片/附件）。
    ///
    /// `path` 可以是相对路径（`/content/s3/getObjectIO?...`）或绝对 URL。
    ///
    /// ## 为什么不能用 `request()`
    ///
    /// `request()` 假定响应是 `{code,msg,data}` 的 JSON，会尝试解析并报错。
    /// 图片是裸字节，必须走这条独立的通道。
    ///
    /// ## 鉴权
    ///
    /// 平台的对象存储**也要 `Authorization`** —— 不带 token 拿到的是一段
    /// JSON 错误（HTTP 200），再按魔数一判就知道不是图片。
    pub fn fetch_bytes(&self, path: &str) -> Result<Vec<u8>> {
        let url = if path.starts_with("http://") || path.starts_with("https://") {
            path.to_string()
        } else if path.starts_with('/') {
            format!("{}{}", self.base, path)
        } else {
            format!("{}/{}", self.base, path)
        };
        let mut req = self
            .http
            .get(&url)
            .header("Accept", "image/*,*/*");
        if !self.session.access_token.is_empty() {
            req = req.header("Authorization", format!("Bearer {}", self.session.access_token));
        }
        if !self.tenant_id.is_empty() {
            req = req.header("Tenant-Id", &self.tenant_id);
        }
        Self::throttle();
        let resp = req.send()?;
        if !resp.status().is_success() {
            return Err(Error::Biz {
                code: resp.status().as_u16() as i64,
                msg: format!("取资源失败：{}", url),
            });
        }
        Ok(resp.bytes()?.to_vec())
    }

    pub fn base(&self) -> &str {
        &self.base
    }
    pub fn tenant_id(&self) -> &str {
        &self.tenant_id
    }
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// 记下凭据，供 `with_reauth` 自动重登。
    pub fn remember_credentials(&mut self, username: &str, password: &str) {
        self.creds = Some((username.to_string(), password.to_string()));
    }

    /// 恢复已保存的会话（如从数据库读出的 token）。
    pub fn load_session(&mut self, sess: Session) {
        if !sess.tenant_id.is_empty() {
            self.tenant_id = sess.tenant_id.clone();
        }
        self.session = sess;
    }

    // ---------------------------------------------------------------- 登录

    /// App 登录（免滑块）。
    pub fn login(&mut self, username: &str, password: &str) -> Result<Session> {
        let data = self.request(
            "POST",
            &format!("{WEB_API}/system/auth/app/login"),
            &[],
            Some(&json!({ "username": username, "password": password })),
            false,
        )?;
        let sess: Session = serde_json::from_value(data)
            .map_err(|e| Error::Shape(format!("登录返回无法解析：{e}")))?;
        if sess.access_token.is_empty() {
            return Err(Error::Invalid("登录返回 code=0 但未取到 accessToken".into()));
        }
        // 登录返回的 tenantId 才是权威值
        if !sess.tenant_id.is_empty() {
            self.tenant_id = sess.tenant_id.clone();
        }
        self.session = sess;
        self.session.tenant_id = self.tenant_id.clone();
        Ok(self.session.clone())
    }

    // ---------------------------------------------------------------- 请求

    /// 发一次请求并解包 `data`。
    ///
    /// * `params` —— query（GET 用；其它方法也允许，服务端不介意）
    /// * `body` —— JSON body
    /// * `auth` —— 是否带 `Authorization`
    pub fn request(
        &self,
        method: &str,
        path: &str,
        params: &[(&str, String)],
        body: Option<&Value>,
        auth: bool,
    ) -> Result<Value> {
        // 签名只覆盖**相对路径**（APK 用 options.getUrl()），不含域名与 query
        let sign_path = if path.starts_with('/') {
            path.to_string()
        } else {
            format!("/{path}")
        };

        let mut req = self
            .http
            .request(method.parse().map_err(|_| Error::Invalid(format!("非法方法 {method}")))?, format!("{}{}", self.base, sign_path))
            .header("Content-Type", "application/json;charset=UTF-8")
            .header("Accept", "application/json, text/plain, */*")
            .header(
                "Id-Code",
                build_id_code(Some(self.session.user_id.as_str()), &sign_path),
            );

        if !self.tenant_id.is_empty() {
            req = req.header("Tenant-Id", &self.tenant_id);
        }
        if auth && !self.session.access_token.is_empty() {
            req = req.header("Authorization", format!("Bearer {}", self.session.access_token));
        }
        let mut q: BTreeMap<&str, &String> = BTreeMap::new();
        for (k, v) in params {
            q.insert(k, v);
        }
        if !q.is_empty() {
            req = req.query(&q);
        }
        if let Some(b) = body {
            req = req.json(b);
        }

        Self::throttle();
        let resp = req.send()?;
        let status = resp.status();
        let text = resp.text().unwrap_or_default();

        if !status.is_success() {
            // 服务端会用 HTTP 状态码表达一部分失败；把原文带上，否则排查只剩一个数字
            return Err(Error::Biz {
                code: status.as_u16() as i64,
                msg: format!("HTTP {status} {}", text.chars().take(200).collect::<String>()),
            });
        }
        let payload: Value = serde_json::from_str(&text)
            .map_err(|_| Error::Shape(format!("响应不是 JSON：{}", text.chars().take(200).collect::<String>())))?;

        let code = payload.get("code").and_then(Value::as_i64).ok_or_else(|| {
            Error::Shape(format!("响应缺少 code：{}", text.chars().take(200).collect::<String>()))
        })?;
        if code == 0 {
            return Ok(payload.get("data").cloned().unwrap_or(Value::Null));
        }
        Err(Error::Biz {
            code,
            msg: payload
                .get("msg")
                .and_then(Value::as_str)
                .unwrap_or("未知错误")
                .to_string(),
        })
    }

    /// 构造带公共请求头的请求（`request` / `request_bytes` / `request_multipart` 共用）。
    fn prepare(&self, method: &str, path: &str) -> Result<reqwest::blocking::RequestBuilder> {
        self.prepare_ex(method, path, true)
    }

    /// `json_ct=false` 时不设 `Content-Type` —— multipart 要用自己的（带 boundary）。
    fn prepare_ex(
        &self,
        method: &str,
        path: &str,
        json_ct: bool,
    ) -> Result<reqwest::blocking::RequestBuilder> {
        // 签名只覆盖**相对路径**（APK 用 options.getUrl()），不含域名与 query
        let sign_path = if path.starts_with('/') {
            path.to_string()
        } else {
            format!("/{path}")
        };
        let m = method
            .parse()
            .map_err(|_| Error::Invalid(format!("非法方法 {method}")))?;
        let mut req = self
            .http
            .request(m, format!("{}{}", self.base, sign_path))
            .header("Accept", "application/json, text/plain, */*");
        if json_ct {
            req = req.header("Content-Type", "application/json;charset=UTF-8");
        }
        let mut req = req.header(
            "Id-Code",
            build_id_code(Some(self.session.user_id.as_str()), &sign_path),
        );
        if !self.tenant_id.is_empty() {
            req = req.header("Tenant-Id", &self.tenant_id);
        }
        if !self.session.access_token.is_empty() {
            req = req.header("Authorization", format!("Bearer {}", self.session.access_token));
        }
        Ok(req)
    }

    /// 下载原始字节（文件接口用）。
    pub fn request_bytes(
        &self,
        method: &str,
        path: &str,
        params: &[(&str, String)],
    ) -> Result<Vec<u8>> {
        let mut req = self.prepare(method, path)?;
        if !params.is_empty() {
            req = req.query(params);
        }
        Self::throttle();
        let resp = req.send()?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().unwrap_or_default();
            return Err(Error::Biz {
                code: status.as_u16() as i64,
                msg: format!("HTTP {status} {}", text.chars().take(200).collect::<String>()),
            });
        }
        Ok(resp.bytes()?.to_vec())
    }

    /// multipart 上传。
    ///
    /// ⚠️ **不要**复用 `prepare` 里的 `Content-Type` —— multipart 的 boundary
    /// 由 reqwest 自己生成，覆盖成 `application/json` 服务端会解析不出文件。
    pub fn request_multipart(
        &self,
        path: &str,
        form: reqwest::blocking::multipart::Form,
    ) -> Result<Value> {
        // ⚠️ **不要手动设 Content-Type**。
        //
        // reqwest 的 `.multipart()` 会自己写 `multipart/form-data; boundary=...`。
        // 但 `prepare()` 已经设了 `Content-Type: application/json`，
        // 而 `.header()` 在 `.multipart()` 之前调用 —— 结果是 json 那个头
        // **覆盖掉了 boundary**，服务端解析不出任何字段。
        //
        // 实测症状：`/content/s3/putObject` 报「[1] source不能为空」，
        // 明明 form 里带了 source。去掉手动头之后立刻成功。
        // `json_ct=false`：不设 Content-Type，交给 `.multipart()` 写带 boundary 的
        let req = self.prepare_ex("POST", path, false)?.multipart(form);
        Self::throttle();
        let resp = req.send()?;
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        if !status.is_success() {
            return Err(Error::Biz {
                code: status.as_u16() as i64,
                msg: format!("HTTP {status} {}", text.chars().take(200).collect::<String>()),
            });
        }
        let payload: Value = serde_json::from_str(&text)
            .map_err(|_| Error::Shape(format!("响应不是 JSON：{}", text.chars().take(200).collect::<String>())))?;
        let code = payload.get("code").and_then(Value::as_i64).unwrap_or(-1);
        if code == 0 {
            return Ok(payload.get("data").cloned().unwrap_or(Value::Null));
        }
        Err(Error::Biz {
            code,
            msg: payload.get("msg").and_then(Value::as_str).unwrap_or("未知错误").to_string(),
        })
    }

    /// 执行 `op`；遇登录态失效则**重新登录并重试一次**。
    ///
    /// ⚠️ 把**整个业务序列**放进来，不要只包第一个请求 ——
    /// 只包第一个的话，重登拿到的新 token 会在外面被旧 `self` 用掉，
    /// 于是「enter 成功、紧接着的 save 仍然 401」。
    pub fn with_reauth<T>(&mut self, op: impl Fn(&Client) -> Result<T>) -> Result<T> {
        match op(self) {
            Ok(v) => Ok(v),
            Err(e) if e.is_auth_error() && self.creds.is_some() => {
                let (u, p) = self.creds.clone().expect("已判断 Some");
                log::warn!("登录态失效，重新登录：{u}");
                self.session = Session::default();
                self.login(&u, &p)?;
                op(self)
            }
            Err(e) => Err(e),
        }
    }
}

impl Client {
    /// 权限信息（含 `user.nickname` / `number` / `deptName`）。
    ///
    /// **拿真实姓名用这个，不要用 `profile/get`** —— 后者的 `data`
    /// 在有些账号下是**加密串**，读不出东西。
    /// 这个接口的 `user.nickname` 才是可读的姓名。
    pub fn permission_info(&self) -> Result<Value> {
        self.request(
            "GET",
            &format!("{WEB_API}/system/auth/get-permission-info"),
            &[],
            None,
            true,
        )
    }

    /// 取当前登录用户的真实姓名。拿不到就返回空串。
    pub fn real_name(&self) -> String {
        self.permission_info()
            .ok()
            .and_then(|v| v.get("user").cloned())
            .and_then(|u| {
                u.get("nickname")
                    .or_else(|| u.get("name"))
                    .and_then(Value::as_str)
                    .map(|s| s.trim().to_string())
            })
            .unwrap_or_default()
    }
}

/// 只在第一次用到代理时打一行日志。
///
/// 每个账号都会新建 client，不管一下会刷屏。
fn log_once_proxy(proxy: &str) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static ONCE: AtomicBool = AtomicBool::new(false);
    if !ONCE.swap(true, Ordering::Relaxed) {
        // 只打协议与主机，不打印可能的认证信息
        let shown = proxy.split('@').last().unwrap_or(proxy);
        eprintln!("[neumooc-core] 平台请求走代理：{shown}");
    }
}
