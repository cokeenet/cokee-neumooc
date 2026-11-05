//! 学期 / 课程 / 课程资源 —— 从 Python 版移植。
//!
//! ## 两个入口别搞混
//!
//! * **课程资源列表**（`listResource`）—— 视频、文档、图片这些「要刷」的东西
//! * **学习任务**（`study-task/page`，见 [`crate::homework`]）—— 作业 / 测验
//!
//! 作业**不在**资源列表里。早期在资源列表里找作业找了很久，方向就是错的。
//!
//! ## `currentClassId` 是必需的
//!
//! 资源类接口必须带 `teachCourseId` + `currentClassId`，**漏了会一律 403** ——
//! 而 403 在这个平台上极易被误读成「没权限」。

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::client::Client;
use crate::de::{de_i64, de_opt_i64, de_str};
use crate::error::{Error, Result};

const TM: &str = "/web-api/teachmanager";

pub const RES_TYPE_VIDEO: i64 = 1;
pub const RES_TYPE_DOC: i64 = 3;
pub const RES_TYPE_IMAGE: i64 = 4;

pub const STUDY_STATUS_TODO: i64 = 0;
pub const STUDY_STATUS_DONE: i64 = 2;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Term {
    #[serde(default, deserialize_with = "de_str")]
    pub id: String,
    #[serde(default, deserialize_with = "de_str")]
    pub name: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Course {
    #[serde(default, rename = "teachCourseId", deserialize_with = "de_str")]
    pub teach_course_id: String,
    /// 资源接口要的 `currentClassId`（APK 里叫 `teachClassId`，值相同）
    #[serde(default, rename = "teachClassId", deserialize_with = "de_str")]
    pub teach_class_id: String,
    #[serde(default, rename = "courseName", deserialize_with = "de_str")]
    pub name: String,
    /// 由 `list_courses` 补上（接口不返回）
    #[serde(default)]
    pub term_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Resource {
    #[serde(default, rename = "id", deserialize_with = "de_str")]
    pub res_id: String,
    #[serde(default, deserialize_with = "de_str")]
    pub name: String,
    #[serde(default, rename = "resType", deserialize_with = "de_i64")]
    pub res_type: i64,
    #[serde(default, rename = "studyStatus", deserialize_with = "de_i64")]
    pub study_status: i64,
    #[serde(default, rename = "resExt", deserialize_with = "de_str")]
    pub res_ext: String,
    #[serde(default, rename = "playProgress", deserialize_with = "de_opt_i64")]
    pub play_progress: Option<i64>,

    /// **是否计入考核**（1 = 计入）。原始响应里叫 `isAssess`。
    ///
    /// ⚠️ 这是最该关注的字段之一：实测某学期 **107 个资源计入考核、
    /// 382 个不计入** —— 只刷不计入的那些，考核分一分不涨。
    /// 本项目一开始完全没抓这个字段（结构体只取了 6 个字段，
    /// 原始响应有 47 个），所以「刷完了但没分」这种问题根本看不出来。
    #[serde(default, rename = "isAssess", deserialize_with = "de_i64")]
    pub is_assess: i64,

    /// 开放/截止时间（毫秒）。`closeTime` 可能是 null。
    #[serde(default, rename = "openTime", deserialize_with = "de_opt_i64")]
    pub open_time: Option<i64>,
    #[serde(default, rename = "closeTime", deserialize_with = "de_opt_i64")]
    pub close_time: Option<i64>,

    /// 资源说明（`resDesc`）。
    #[serde(default, rename = "resDesc", deserialize_with = "de_str")]
    pub res_desc: String,

    /// 上游的状态（`status`，与 `studyStatus` 不同）。
    #[serde(default, deserialize_with = "de_i64")]
    pub status: i64,

    /// 是否已过期（`resIsExpire`）。
    #[serde(default, rename = "resIsExpire", deserialize_with = "de_i64")]
    pub is_expire: i64,
}

impl Resource {
    pub fn is_done(&self) -> bool {
        self.study_status == STUDY_STATUS_DONE
    }
    pub fn is_video(&self) -> bool {
        self.res_type == RES_TYPE_VIDEO
    }
    pub fn is_doc(&self) -> bool {
        self.res_type == RES_TYPE_DOC
    }
    /// 待完成。
    /// 是否计入考核。
    pub fn is_assessed(&self) -> bool {
        self.is_assess == 1
    }

    /// 是否已过截止时间。**过期了再刷通常不计分** —— 优先级要降下来。
    pub fn is_expired(&self, now_ms: i64) -> bool {
        matches!(self.close_time, Some(t) if t > 0 && now_ms > t)
    }

    pub fn is_todo(&self) -> bool {
        !self.is_done()
    }
}

/// 目录树节点。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TreeNode {
    #[serde(default, deserialize_with = "de_str")]
    pub id: String,
    #[serde(default, deserialize_with = "de_str")]
    pub name: String,
    #[serde(default, rename = "dirType", deserialize_with = "de_str")]
    pub dir_type: String,
    #[serde(default, rename = "resCount", deserialize_with = "de_i64")]
    pub res_count: i64,
}

impl Client {
    /// 列学期。
    pub fn list_terms(&self) -> Result<Vec<Term>> {
        let data = self.request(
            "GET",
            &format!("{TM}/teach-dropdown/getTeachTermDropDown"),
            &[],
            None,
            true,
        )?;
        // 接口可能返回 `[{id,name}]` 或 `{list:[...]}` —— 两种都收
        let rows = data
            .as_array()
            .cloned()
            .or_else(|| data.get("list").and_then(Value::as_array).cloned())
            .unwrap_or_default();
        let mut out = Vec::new();
        for r in rows {
            let t: Term = serde_json::from_value(r).map_err(|e| Error::Shape(e.to_string()))?;
            if !t.id.is_empty() {
                out.push(t);
            }
        }
        Ok(out)
    }

    /// 取**最新学期**。
    ///
    /// ⚠️ 不能直接用 `list_terms().first()` —— 接口**不保证顺序**。
    /// 实测它现在返回「新 → 旧」，但只要哪天变成「旧 → 新」：
    /// 自动签到会去同步**最老的学期**，永远看不到当前考勤，
    /// 而且**一个错都不报**（列表非空、请求 200、就是没有待签场次）。
    ///
    /// `term_id` 是递增的数字串，按「长度优先、再字典序」比较。
    ///
    /// ⚠️ **别用它来选「当前学期」去查考勤/课程** —— 平台里存在**未来学期**，
    /// 它的 id 比当前学期更大但数据全空（实测 `2026-2027-第3学期`）。
    /// 查考勤请用 `server::tasks` 里的 `term_with_attendance`（按平台顺序试到有数据为止）。
    pub fn latest_term(&self) -> Result<Term> {
        let terms = self.list_terms()?;
        terms
            .into_iter()
            .max_by(|a, b| {
                if term_is_newer(&a.id, &b.id) {
                    std::cmp::Ordering::Greater
                } else if term_is_newer(&b.id, &a.id) {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .ok_or_else(|| Error::Invalid("拿不到任何学期".into()))
    }

    /// 列某学期的课程。
    ///
    /// `teachClassId` 就是资源接口要的 `currentClassId`。
    pub fn list_courses(&self, term_id: &str) -> Result<Vec<Course>> {
        let data = self.request(
            "GET",
            &format!("{TM}/teach-arrangement-stu/page"),
            &[
                ("termId", term_id.to_string()),
                ("pageNo", "1".into()),
                ("pageSize", "100".into()),
            ],
            None,
            true,
        )?;
        let rows = data
            .get("list")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut out = Vec::new();
        for r in rows {
            let mut c: Course =
                serde_json::from_value(r).map_err(|e| Error::Shape(e.to_string()))?;
            if c.teach_course_id.is_empty() || c.teach_class_id.is_empty() {
                continue; // 缺任一个都没法查资源
            }
            c.term_id = term_id.to_string();
            out.push(c);
        }
        Ok(out)
    }

    /// 列课程资源（含目录递归）。
    ///
    /// 实测多数课程的 `folderList` 为空、`resList` 直接给全部资源；
    /// 仍保留递归以兼容有目录的课程。
    pub fn list_resources(&self, course: &Course) -> Result<Vec<Resource>> {
        let mut out = Vec::new();
        let mut seen_folders: Vec<String> = Vec::new();
        self.walk_resources(course, "0", &mut out, &mut seen_folders)?;
        Ok(out)
    }

    fn walk_resources(
        &self,
        course: &Course,
        parent_id: &str,
        out: &mut Vec<Resource>,
        seen: &mut Vec<String>,
    ) -> Result<()> {
        let body = json!({
            "teachCourseId": course.teach_course_id,
            "currentClassId": course.teach_class_id,
            "name": "", "resType": "", "studyStatus": "", "sortType": 1,
            "isAssess": "", "parentId": parent_id,
        });
        let data = self.request(
            "POST",
            &format!("{TM}/teach-course-res-stu/listResource"),
            &[],
            Some(&body),
            true,
        )?;
        for r in data.get("resList").and_then(Value::as_array).cloned().unwrap_or_default() {
            out.push(serde_json::from_value(r).map_err(|e| Error::Shape(e.to_string()))?);
        }
        // 递归子目录；用 seen 防环（服务端理论上不会成环，但白拿的保险）
        for f in data.get("folderList").and_then(Value::as_array).cloned().unwrap_or_default() {
            let id = f.get("id").and_then(Value::as_str).unwrap_or("").to_string();
            if id.is_empty() || seen.contains(&id) {
                continue;
            }
            seen.push(id.clone());
            self.walk_resources(course, &id, out, seen)?;
        }
        Ok(())
    }

    /// 课程目录树。
    ///
    /// ⚠️ 树里 `resCount` 常为 0，**资源不在这里** —— 它给的是课程结构（第几次课），
    /// 别指望靠它找到资源或作业。
    pub fn list_tree(&self, course: &Course) -> Result<Vec<TreeNode>> {
        let body = json!({
            "teachCourseId": course.teach_course_id,
            "currentClassId": course.teach_class_id,
        });
        let data = self.request(
            "POST",
            &format!("{TM}/teach-course-directory-stu/listTree"),
            &[],
            Some(&body),
            true,
        )?;
        let rows = data
            .get("treeList")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        rows.into_iter()
            .map(|r| serde_json::from_value(r).map_err(|e| Error::Shape(e.to_string())))
            .collect()
    }
}

/// 按资源类型过滤（刷课默认只要视频 + 文档）。
pub fn filter_by_types(resources: &[Resource], types: &[i64]) -> Vec<Resource> {
    resources
        .iter()
        .filter(|r| types.contains(&r.res_type))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terms_accept_list_and_wrapped() {
        // 接口两种形态都出现过：裸数组 与 {list:[...]}
        let bare = serde_json::json!([{"id": "1", "name": "甲"}, {"id": "2", "name": "乙"}]);
        let wrapped = serde_json::json!({"list": [{"id": "3", "name": "丙"}]});
        for (v, want) in [(bare, 2), (wrapped, 1)] {
            let rows = v
                .as_array()
                .cloned()
                .or_else(|| v.get("list").and_then(Value::as_array).cloned())
                .unwrap_or_default();
            assert_eq!(rows.len(), want);
        }
    }

    #[test]
    fn course_requires_both_ids() {
        // 缺 teachClassId 的课程没法查资源，应被丢掉
        let raw = serde_json::json!({"teachCourseId": "T1", "courseName": "只有课程 id"});
        let c: Course = serde_json::from_value(raw).unwrap();
        assert!(c.teach_class_id.is_empty());
    }

    #[test]
    fn resource_flags() {
        let v: Resource = serde_json::from_value(serde_json::json!({
            "id": "r1", "name": "视频", "resType": 1, "studyStatus": 0
        }))
        .unwrap();
        assert!(v.is_video() && v.is_todo() && !v.is_done());
        let d: Resource = serde_json::from_value(serde_json::json!({
            "id": "r2", "resType": 3, "studyStatus": 2
        }))
        .unwrap();
        assert!(d.is_doc() && d.is_done() && !d.is_todo());
    }

    #[test]
    fn filter_by_types_keeps_wanted() {
        let mk = |t: i64| Resource { res_type: t, ..Default::default() };
        let all = vec![mk(1), mk(3), mk(4), mk(1)];
        let got = filter_by_types(&all, &[1, 3]);
        assert_eq!(got.len(), 3, "只要视频与文档");
        assert!(got.iter().all(|r| r.res_type == 1 || r.res_type == 3));
    }

    #[test]
    fn null_numerics_tolerated() {
        let r: Resource = serde_json::from_value(serde_json::json!({
            "id": "r", "resType": null, "studyStatus": null, "playProgress": null
        }))
        .unwrap();
        assert_eq!(r.res_type, 0);
        assert_eq!(r.study_status, 0);
        assert!(r.play_progress.is_none());
    }
}

/// 学期 id 的「新旧」比较：**长度优先，再字典序**。
///
/// 这些 id 是递增的雪花号（数字串），位数会随时间增长
/// （`1946...` → `2089...`），所以不能纯字典序：
/// 短的那个数字更小，年份更早。
pub fn term_is_newer(a: &str, b: &str) -> bool {
    match a.len().cmp(&b.len()) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Equal => a > b,
    }
}

#[cfg(test)]
mod term_order_tests {
    use super::term_is_newer;

    #[test]
    fn longer_id_is_newer() {
        // 位数变长的真实例子
        assert!(term_is_newer("2089598998973100033", "1946114260626235393"));
        assert!(!term_is_newer("1946114260626235393", "2089598998973100033"));
    }

    #[test]
    fn same_length_compares_lexicographically() {
        assert!(term_is_newer("2027181732793139202", "1946114260626235393"));
        assert!(!term_is_newer("2027181732793139202", "2089598998973100033"));
    }

    #[test]
    fn equal_is_not_newer() {
        assert!(!term_is_newer("123", "123"));
    }
}
