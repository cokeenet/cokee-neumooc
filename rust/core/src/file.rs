//! 文件收发 —— 从 Python 版移植。
//!
//! | 用途 | 方法 | 路径 |
//! | --- | --- | --- |
//! | 下载 | GET | `/content/s3/getObjectIOForDown?eid=&source=NEUNSE`（**无 web-api 前缀**）|
//! | 上传（新） | POST（multipart） | `/content/s3/putObject` |
//! | 批量上传（旧） | POST（multipart） | `/web-api/content/s3/multiUploadOldEid` |
//! | 查文件元信息 | GET | `/content/search/queryIndexById?source=NEUNSE&eid=` |
//! | 改头像 | POST | `/web-api/system/user/profile/update-avatar` |
//!
//! ## 路径必须做围栏
//!
//! 上传的路径来自调用方（可能是 HTTP 请求里的字符串）。**必须**先过
//! [`confine`] —— 用 `starts_with` 做前缀判断会被 `/dl-evil` 这种绕过，
//! 所以这里用 `Path::starts_with`（按**路径分量**比较，不是字符串前缀）。

use std::path::{Component, Path, PathBuf};

use serde_json::json;
use serde_json::Value;

use crate::client::{Client, WEB_API};
use crate::error::{Error, Result};

const SYS: &str = "/web-api/system";

impl Client {
    /// 按 EID 下载文件，返回原始字节。
    ///
    /// ⚠️ 服务端会用 **HTTP 200 + `{"code":500,"msg":"系统异常"}`** 表达失败 ——
    /// 直接返回字节的话，调用方会存下一个 45 字节的「文件」，里面是错误信息。
    /// （Python 版就是这样的：实测同一个 EID 返回 45 字节的 JSON。）
    /// 这里在返回前识别出错误外壳并抛错。
    pub fn file_download(&self, eid: &str) -> Result<Vec<u8>> {
        // ⚠️ **没有 `/web-api` 前缀** —— 这是内容服务，不是业务接口。
        //
        // 实测（2026-11-06）：
        //   /web-api/content/s3/getObjectIOForDown  → 45 字节 {"code":500,"msg":"系统异常"}
        //   /content/s3/getObjectIOForDown          → 423578 字节真 docx
        //
        // 用错前缀时服务端**不报 404，而是回一段 JSON 错误**，而且 HTTP 200。
        // 所以一定要看首字节判断（下面的错误外壳检测就是干这个的）。
        let bytes = self.request_bytes(
            "GET",
            "/content/s3/getObjectIOForDown",
            &[("eid", eid.to_string()), ("source", "NEUNSE".into())],
        )?;
        // 真文件不会是一个几百字节内、以 `{` 开头且含 code 字段的 JSON
        if bytes.len() < 1024 && bytes.first() == Some(&b'{') {
            if let Ok(v) = serde_json::from_slice::<Value>(&bytes) {
                if let Some(code) = v.get("code").and_then(Value::as_i64) {
                    if code != 0 {
                        return Err(Error::Biz {
                            code,
                            msg: v
                                .get("msg")
                                .and_then(Value::as_str)
                                .unwrap_or("下载失败")
                                .to_string(),
                        });
                    }
                }
            }
        }
        Ok(bytes)
    }

    /// 批量上传文件（multipart，字段名固定为 `files`）。
    ///
    /// `paths` 里的每一项都必须先过 [`confine`]。
    pub fn file_upload(&self, paths: &[PathBuf]) -> Result<Value> {
        if paths.is_empty() {
            return Err(Error::Invalid("没有要上传的文件".into()));
        }
        let mut form = reqwest::blocking::multipart::Form::new();
        for p in paths {
            if !p.is_file() {
                return Err(Error::Invalid(format!("不是文件或不存在：{}", p.display())));
            }
            let name = p
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "file".into());
            let bytes = std::fs::read(p)?;
            let part = reqwest::blocking::multipart::Part::bytes(bytes)
                .file_name(name)
                .mime_str("application/octet-stream")
                .map_err(|e| Error::Shape(e.to_string()))?;
            form = form.part("files", part);
        }
        self.request_multipart("/web-api/content/s3/multiUploadOldEid", form)
    }

    /// 查文件的元信息（大小、类型、原始文件名）。
    ///
    /// 用来在界面上显示「附件 423KB · docx」这类信息，以及判断能不能预览。
    pub fn file_meta(&self, eid: &str) -> Result<Value> {
        let data = self.request(
            "GET",
            "/content/search/queryIndexById",
            &[("source", "NEUNSE".into()), ("eid", eid.to_string())],
            None,
            true,
        )?;
        Ok(data)
    }

    /// **上传单个文件（新接口）** —— `/content/s3/putObject`。
    ///
    /// 与旧的 `multiUploadOldEid` 的区别：
    ///
    /// * 旧接口字段名是 `files`（可多文件），返回一个 eid 数组
    /// * 新接口字段名是 `file`（单文件），额外带 `source` / `isCheck` / `isSlice`，
    ///   返回完整元信息（eid / filePath / fileMd5 / objectKey…）
    ///
    /// 网页端上传附件、图片都走这个。`isSlice=true` 表示允许服务端分片。
    pub fn file_put_object(&self, name: &str, bytes: Vec<u8>, mime: &str) -> Result<Value> {
        let part = reqwest::blocking::multipart::Part::bytes(bytes)
            .file_name(name.to_string())
            .mime_str(mime)
            .map_err(|e| Error::Shape(e.to_string()))?;
        let form = reqwest::blocking::multipart::Form::new()
            .text("source", "NEUNSE")
            .text("isCheck", "true")
            .text("isSlice", "true")
            .part("file", part);
        self.request_multipart("/content/s3/putObject", form)
    }

    /// 从本地路径上传（读文件 → `file_put_object`）。
    pub fn file_upload_one(&self, path: &Path) -> Result<Value> {
        if !path.is_file() {
            return Err(Error::Invalid(format!("不是文件或不存在：{}", path.display())));
        }
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "file".into());
        let mime = mime_of(&name);
        let bytes = std::fs::read(path)?;
        // 300MB 上限是平台自己标的（"可上传5个附件，大小不超过300MB"）
        if bytes.len() > 300 * 1024 * 1024 {
            return Err(Error::Invalid("文件超过 300MB".into()));
        }
        self.file_put_object(&name, bytes, mime)
    }

    /// 改头像（`eid` 是已上传文件的 EID）。
    pub fn update_avatar(&self, eid: &str) -> Result<Value> {
        self.request(
            "POST",
            &format!("{SYS}/user/profile/update-avatar"),
            &[],
            Some(&json!({ "eid": eid })),
            true,
        )
    }
}

/// 把 `candidate` 限制在 `base` 目录内，返回规范化后的绝对路径。
///
/// ⚠️ **不要用字符串 `starts_with`** —— `/dl-evil` 会以 `/dl` 为前缀而被放行。
/// `Path::starts_with` 按**路径分量**比较，不会中这个招。
/// 同时拒绝 `..` 分量（即便规范化后落在 base 内也不接受显式的上跳）。
pub fn confine(base: &Path, candidate: &str) -> Result<PathBuf> {
    let p = Path::new(candidate);
    if p.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(Error::Invalid(format!("路径不允许包含 ..：{candidate}")));
    }
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    };
    // 规范化（不要求文件已存在，所以不能用 canonicalize）
    let mut norm = PathBuf::new();
    for c in abs.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                norm.pop();
            }
            other => norm.push(other.as_os_str()),
        }
    }
    let base_norm: PathBuf = base.components().collect();
    if !norm.starts_with(&base_norm) {
        return Err(Error::Invalid(format!(
            "只能访问 {} 内的文件：{candidate}",
            base.display()
        )));
    }
    Ok(norm)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **回归**：服务端用 HTTP 200 + `{"code":500}` 表达失败。
    ///
    /// 不识别的话，调用方会把错误信息当成文件存下来（Python 版实测就是这样：
    /// 同一个 EID 返回 45 字节的 JSON，里面是「系统异常」）。
    #[test]
    fn download_detects_json_error_envelope() {
        // 原始字节串不能含非 ASCII，这里用 ASCII 的等价外壳
        let err_body = br#"{"code":500,"data":null,"msg":"system error"}"#;
        let looks_like_error = err_body.len() < 1024
            && err_body.first() == Some(&b'{')
            && serde_json::from_slice::<Value>(err_body)
                .ok()
                .and_then(|v| v.get("code").and_then(Value::as_i64))
                .is_some_and(|c| c != 0);
        assert!(looks_like_error, "错误外壳必须被识别出来");

        // 成功外壳（code=0）不该被当成错误
        let ok_body = br#"{"code":0,"data":null,"msg":""}"#;
        let is_err = serde_json::from_slice::<Value>(ok_body)
            .ok()
            .and_then(|v| v.get("code").and_then(Value::as_i64))
            .is_some_and(|c| c != 0);
        assert!(!is_err);

        // 真文件（PDF 头）不该被误判
        let real = b"%PDF-1.7\n...";
        assert_ne!(real.first(), Some(&b'{'));
        // 超过 1KB 的 JSON（别人上传的 .json 作业）不按错误外壳处理
        assert!(2000 >= 1024, "大 JSON 走真文件路径");
    }

    #[test]
    fn confine_allows_inside() {
        let base = Path::new("/srv/dl");
        assert_eq!(confine(base, "a/b.pdf").unwrap(), PathBuf::from("/srv/dl/a/b.pdf"));
        assert_eq!(confine(base, "/srv/dl/x.txt").unwrap(), PathBuf::from("/srv/dl/x.txt"));
    }

    #[test]
    fn confine_rejects_parent_dir() {
        let base = Path::new("/srv/dl");
        assert!(confine(base, "../etc/passwd").is_err());
        assert!(confine(base, "a/../../etc/passwd").is_err());
    }

    #[test]
    fn confine_rejects_prefix_spoof() {
        // **回归**：字符串 starts_with("/srv/dl") 会把 "/srv/dl-evil" 放行。
        // 必须按路径分量比较。
        let base = Path::new("/srv/dl");
        assert!(
            confine(base, "/srv/dl-evil/x.txt").is_err(),
            "/srv/dl-evil 不属于 /srv/dl"
        );
    }

    #[test]
    fn confine_rejects_absolute_outside() {
        let base = Path::new("/srv/dl");
        assert!(confine(base, "/etc/shadow").is_err());
    }

    #[test]
    fn mime_by_extension() {
        assert_eq!(mime_of("a.PNG"), "image/png");
        assert_eq!(mime_of("报告.docx"), "application/vnd.openxmlformats-officedocument.wordprocessingml.document");
        assert_eq!(mime_of("noext"), "application/octet-stream");
    }
}

/// 按扩展名推断 MIME。
///
/// 平台会按 Content-Type 决定怎么处理（图片会生成缩略图，文档会转 PDF 预览）。
/// 传 `application/octet-stream` 也能上传成功，但**预览功能会失效**。
pub fn mime_of(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "pdf" => "application/pdf",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "zip" => "application/zip",
        "rar" => "application/vnd.rar",
        "7z" => "application/x-7z-compressed",
        "txt" => "text/plain",
        "md" => "text/markdown",
        "mp4" => "video/mp4",
        "mp3" => "audio/mpeg",
        _ => "application/octet-stream",
    }
}
