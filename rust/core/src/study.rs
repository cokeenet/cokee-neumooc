//! 刷课（学习记录上报）—— 从 Python 版移植。
//!
//! ## 完成判定的唯一依据是**回读**
//!
//! 每个 `complete_*` 都在最后回读一次 `getRecordInfo`，**只有 `studyStatus == 2`
//! 才算成功**。接口返回 `code=0` 不代表完成 —— 文档类资源实测就是这个情况：
//! 全链路 code=0，但 `studyStatus` 纹丝不动。
//!
//! 所以这里的函数返回 `bool`（是否**确认**完成），而不是 `Result<()>`。
//! 调用方必须看这个布尔值，别把「没报错」当成「做完了」。
//!
//! ## 视频 vs 文档
//!
//! | 类型 | 流程 | 实测 |
//! | --- | --- | --- |
//! | 视频（resType=1） | `startStudy` → 三段进度上报 → 回读 | **6/6 成功** |
//! | 文档（resType=3） | `startStudy` → `startStudyForDocPicMd` → 等 → `endStudyForDocPicMd` → 回读 | **0/6 成功**（保留实现待校准） |

use serde_json::{json, Value};

use crate::client::Client;
use crate::error::Result;
use crate::resources::{Course, Resource, STUDY_STATUS_DONE};

const RECORD: &str = "/web-api/teachmanager/teach-course-res-stu-record";

/// 播放状态。
pub const PLAY_START: i64 = 1;
pub const PLAY_PAUSE: i64 = 2;
pub const PLAY_END: i64 = 3;

/// 三段上报的 `(playStatus, watchProgress)`。
///
/// 顺序与数值都是实测出来的，别改：进度用 `99999` 而不是 100 ——
/// 服务端按「已看完」处理的是这个大数。
const PROGRESS_STEPS: [(i64, i64); 3] =
    [(PLAY_START, 0), (PLAY_PAUSE, 99999), (PLAY_END, 99999)];

fn res_payload(course: &Course, res_id: &str) -> Value {
    json!({
        "teachCourseId": course.teach_course_id,
        "currentClassId": course.teach_class_id,
        "resId": res_id,
    })
}

impl Client {
    /// 回读资源学习记录 —— **完成判定的唯一依据**。
    pub fn record_info(&self, course: &Course, res_id: &str) -> Result<Value> {
        self.request(
            "POST",
            &format!("{RECORD}/getRecordInfo"),
            &[],
            Some(&res_payload(course, res_id)),
            true,
        )
    }

    /// 回读 `studyStatus`。拿不到就返回 0（未完成），**不把失败当成功**。
    pub fn study_status(&self, course: &Course, res_id: &str) -> i64 {
        let Ok(v) = self.record_info(course, res_id) else {
            return 0;
        };
        // 服务端这个字段有时是数字、有时是字符串
        match v.get("studyStatus") {
            Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
            Some(Value::String(s)) => s.parse().unwrap_or(0),
            _ => 0,
        }
    }

    /// 校验资源是否可学（有些资源会返回「未开放」）。
    pub fn validate_resource(&self, course: &Course, res_id: &str) -> Result<Value> {
        self.request(
            "POST",
            &format!("{RECORD}/validateStuResInfo"),
            &[],
            Some(&res_payload(course, res_id)),
            true,
        )
    }

    /// 完成一个视频：`startStudy` → 三段进度上报 → **回读确认**。
    ///
    /// :returns: 是否**确认**完成（回读 `studyStatus == 2`）
    pub fn complete_video(&self, course: &Course, res_id: &str) -> Result<bool> {
        let p = res_payload(course, res_id);
        self.request("POST", &format!("{RECORD}/startStudy"), &[], Some(&p), true)?;
        for (status, progress) in PROGRESS_STEPS {
            let mut body = p.clone();
            if let Some(o) = body.as_object_mut() {
                o.insert("playStatus".into(), json!(status));
                o.insert("watchProgress".into(), json!(progress));
            }
            self.request(
                "POST",
                &format!("{RECORD}/studyForAudioOrVideo"),
                &[],
                Some(&body),
                true,
            )?;
        }
        Ok(self.study_status(course, res_id) == STUDY_STATUS_DONE)
    }

    /// 尝试完成文档（pptx/pdf/xlsx）。
    ///
    /// ⚠️ **实测 0/6 成功**：接口全链路 `code=0`，但 `studyStatus` 不变。
    /// 保留实现待校准，**调用方必须检查返回值**，别把 code=0 当成功。
    pub fn complete_doc(&self, course: &Course, res_id: &str) -> Result<bool> {
        let p = res_payload(course, res_id);
        self.request("POST", &format!("{RECORD}/startStudy"), &[], Some(&p), true)?;
        self.request(
            "POST",
            &format!("{RECORD}/startStudyForDocPicMd"),
            &[],
            Some(&p),
            true,
        )?;
        std::thread::sleep(std::time::Duration::from_millis(1600));
        self.request(
            "POST",
            &format!("{RECORD}/endStudyForDocPicMd"),
            &[],
            Some(&p),
            true,
        )?;
        std::thread::sleep(std::time::Duration::from_millis(1200));
        Ok(self.study_status(course, res_id) == STUDY_STATUS_DONE)
    }

    /// 按类型分派，**以回读结果为准**。
    ///
    /// 不认识的类型返回 `false`（不是错误）—— 调用方按「没做成」处理即可。
    pub fn complete_resource(&self, course: &Course, res: &Resource) -> Result<bool> {
        if res.is_video() {
            return self.complete_video(course, &res.res_id);
        }
        if res.is_doc() {
            return self.complete_doc(course, &res.res_id);
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_steps_are_ordered() {
        // 顺序是实测出来的：开始 → 暂停(满进度) → 结束(满进度)
        assert_eq!(PROGRESS_STEPS[0], (PLAY_START, 0));
        assert_eq!(PROGRESS_STEPS[1], (PLAY_PAUSE, 99999));
        assert_eq!(PROGRESS_STEPS[2], (PLAY_END, 99999));
    }

    #[test]
    fn payload_has_both_ids() {
        // 资源类接口漏了 currentClassId 会一律 403 —— 极易误判成没权限
        let co = Course {
            teach_course_id: "T1".into(),
            teach_class_id: "C1".into(),
            ..Default::default()
        };
        let p = res_payload(&co, "R1");
        assert_eq!(p["teachCourseId"], "T1");
        assert_eq!(p["currentClassId"], "C1");
        assert_eq!(p["resId"], "R1");
    }
}
