//! 作业 / 学习任务 —— 从 Python 版移植，协议见 `docs/HOMEWORK-PROTOCOL.md`。
//!
//! ## 这条链路上最容易踩的三个点（都编进类型里了）
//!
//! 1. **作业在「学习任务」，不在「课程资源列表」** —— 两个独立入口
//! 2. **`study-task/page` 要 `termId`，不要 `currentClassId`** ——
//!    按资源类接口的习惯传课程上下文会得到 `403`，极易误判成「没权限」
//! 3. **作答页的 `testId` 就是列表里的 `ctlId`**（= `resCtl.id`），
//!    **不是 `resId`、不是 `dataId`** —— 三个 ID 长得一样，用错报「作业不存在」

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::client::Client;
use crate::de::{de_f64, de_i64, de_opt_i64, de_str};
use crate::exam::de_answer;
use crate::error::{Error, Result};

const STU: &str = "/web-api/teachmanager/teach-course-res-stu";
const EXAM: &str = "/web-api/smartexam/test/student";

/// 学习任务的类型。列表里混着视频和作业，**必须过滤**。
pub const TASK_TYPE_VIDEO: i64 = 1;
/// 作业 / 测验。
pub const TASK_TYPE_HOMEWORK: i64 = 3;

/// 一个学习任务。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StudyTask {
    #[serde(default, rename = "resId", deserialize_with = "de_str")]
    pub res_id: String,
    /// ★ **这就是作答用的 `testId`**（列表里叫 `ctlId`，等于 `resCtl.id`）
    #[serde(default, rename = "ctlId", deserialize_with = "de_str")]
    pub ctl_id: String,
    #[serde(default, rename = "dataId", deserialize_with = "de_str")]
    pub data_id: String,
    #[serde(default, rename = "resName", deserialize_with = "de_str")]
    pub res_name: String,
    #[serde(default, rename = "teachCourseName", deserialize_with = "de_str")]
    pub course_name: String,
    #[serde(default, rename = "type", deserialize_with = "de_i64")]
    pub task_kind: i64,
    #[serde(default, rename = "taskType")]
    pub task_type: Option<i64>,
    /// 0 = 未作答
    #[serde(default, rename = "studyStatus", deserialize_with = "de_i64")]
    pub study_status: i64,
    #[serde(default, rename = "openTime", deserialize_with = "de_i64")]
    pub open_time: i64,
    #[serde(default, rename = "closeTime", deserialize_with = "de_i64")]
    pub close_time: i64,
}

impl StudyTask {
    /// 是否作业/测验（`type == 3`）。列表里混着视频，不过滤会把每个 mp4 都算成待办。
    pub fn is_homework(&self) -> bool {
        self.task_kind == TASK_TYPE_HOMEWORK
    }

    pub fn is_done(&self) -> bool {
        self.study_status != 0
    }

    /// 待作答。
    pub fn is_pending(&self) -> bool {
        self.is_homework() && !self.is_done()
    }
}

/// 一道题。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Question {
    /// `paperId` —— 提交答案时要带上
    #[serde(default, deserialize_with = "de_str")]
    pub id: String,
    #[serde(default, rename = "questionType", deserialize_with = "de_str")]
    pub question_type: String,
    #[serde(default, deserialize_with = "de_str")]
    pub stem: String,
    #[serde(default, rename = "stemText", deserialize_with = "de_str")]
    pub stem_text: String,
    #[serde(default, rename = "questionScore")]
    pub score: Option<f64>,
    #[serde(default, rename = "questionNo", deserialize_with = "de_str")]
    pub question_no: String,
    #[serde(default, rename = "candidateAnswer")]
    pub candidate_answer: Option<String>,
    /// 我的得分（`candidateScore`）—— 未作答 / 未批阅时是 0 或 null
    #[serde(default, rename = "candidateScore", deserialize_with = "de_f64")]
    pub candidate_score: f64,
    /// 标准答案（单选 `id`、多选 `id1,id2`；主观题可能为空）
    #[serde(default, rename = "standardAnswer", deserialize_with = "de_answer")]
    pub standard_answer: Vec<String>,
    /// 解析（可能为空）
    #[serde(default, rename = "analysis", deserialize_with = "de_str")]
    pub analysis: String,
    #[serde(default, rename = "taskEid", deserialize_with = "de_str")]
    pub task_eid: String,
    #[serde(default, rename = "taskResName", deserialize_with = "de_str")]
    pub task_res_name: String,
    #[serde(default, rename = "answerEid", deserialize_with = "de_str")]
    pub answer_eid: String,
    #[serde(default)]
    pub choices: Vec<Choice>,
}

impl Question {
    /// 是否为文件上传题（题型 40）—— 答案不是文本，而是附件 EID。
    pub fn is_file_upload(&self) -> bool {
        self.question_type == "40"
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Choice {
    #[serde(default, rename = "choiceId", deserialize_with = "de_str")]
    pub id: String,
    #[serde(default, rename = "choiceContent", deserialize_with = "de_str")]
    pub content: String,
}

/// 一份试卷。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Paper {
    #[serde(default, deserialize_with = "de_str")]
    pub name: String,
    #[serde(default, rename = "paperScore", deserialize_with = "de_str")]
    pub score: String,
    /// 题目总数（平台字段 `paperQuestionQty`）
    #[serde(default, rename = "paperQuestionQty", deserialize_with = "de_opt_i64")]
    pub question_qty: Option<i64>,
    #[serde(default, rename = "openTimeStr", deserialize_with = "de_str")]
    pub open_time_str: String,
    #[serde(default, rename = "closeTimeStr", deserialize_with = "de_str")]
    pub close_time_str: String,
    #[serde(default, rename = "paperParts")]
    pub parts: Vec<PaperPart>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PaperPart {
    #[serde(default, rename = "partName", deserialize_with = "de_str")]
    pub name: String,
    #[serde(default, rename = "partDesc", deserialize_with = "de_str")]
    pub desc: String,
    #[serde(default, rename = "paperQuestionList")]
    pub questions: Vec<PaperQuestion>,
}

/// 试卷里的一道题。
///
/// ⚠️ 题目数据**嵌在 `paperQuestion` 子对象里**，不是平铺的。
/// 用 `#[serde(flatten)]` 取不到 —— 会静默得到全空的题（实测踩过）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PaperQuestion {
    #[serde(default, rename = "paperQuestion")]
    pub question: Question,
}

impl Paper {
    /// 摊平所有题目。
    pub fn questions(&self) -> Vec<Question> {
        self.parts
            .iter()
            .flat_map(|p| p.questions.iter().map(|q| q.question.clone()))
            .collect()
    }
}

/// `enter` 的返回：作答所需的全部标识。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Entered {
    #[serde(default, rename = "candidateId", deserialize_with = "de_str")]
    pub candidate_id: String,
    /// 提交时用它，**不是 testId** —— 用错得到 `500 无法找到历史记录`
    #[serde(default, rename = "historyId", deserialize_with = "de_str")]
    pub history_id: String,
    #[serde(default, rename = "testName", deserialize_with = "de_str")]
    pub test_name: String,
    #[serde(default, rename = "studentName", deserialize_with = "de_str")]
    pub student_name: String,
    #[serde(default, rename = "paper")]
    pub paper: Paper,
}

/// 一条待保存的答案。
#[derive(Debug, Clone, Serialize)]
pub struct Answer {
    #[serde(rename = "paperId")]
    pub paper_id: String,
    #[serde(rename = "questionNo")]
    pub question_no: String,
    #[serde(rename = "questionType")]
    pub question_type: String,
    /// 答案。
    ///
    /// * 选择题 / 填空 / 主观：`String`（多选逗号分隔，多空 `|` 分隔）
    /// * **文件题（type=40）**：`{"stuAnswer":"","attachmentList":[{"eid","filename"}]}`
    ///
    /// 所以这里是 `Value` 而不是 `String` —— 早先定成 String 时文件题传不上去。
    pub answer: Value,
}

impl Client {
    /// 列出某学期的学习任务。
    ///
    /// ⚠️ **要 `termId`，不要 `currentClassId`** —— 它是**跨课程的学期任务列表**
    /// （对应 App 首页「学习任务」那一栏）。传课程上下文会得到 403。
    pub fn list_study_tasks(&self, term_id: &str) -> Result<Vec<StudyTask>> {
        let body = json!({ "pageNo": 1, "pageSize": 200, "termId": term_id });
        let data = self.request(
            "POST",
            &format!("{STU}/study-task/page"),
            &[],
            Some(&body),
            true,
        )?;
        let rows = data
            .get("list")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        rows.into_iter()
            .map(|r| serde_json::from_value(r).map_err(|e| Error::Shape(e.to_string())))
            .collect()
    }

    /// 取题目正文。
    ///
    /// `test_id` 用 [`StudyTask::ctl_id`]。**不需要先 `enter`**。
    pub fn get_task_paper(&self, test_id: &str) -> Result<Paper> {
        let data = self.request(
            "GET",
            &format!("{EXAM}/paper"),
            &[("testId", test_id.to_string())],
            None,
            true,
        )?;
        serde_json::from_value(data).map_err(|e| Error::Shape(format!("试卷结构异常：{e}")))
    }

    /// 进入作答，拿 `candidateId` / `historyId` / 完整试卷。
    ///
    /// **幂等**：连调多次返回同一个 `historyId`，不消耗作答次数。
    /// 已交过的报「剩余作答次数不足」，过期的报「作答时间已截止」——
    /// 两者都是业务提示，不是权限问题。
    pub fn enter_task(&self, test_id: &str) -> Result<Entered> {
        let data = self.request(
            "GET",
            &format!("{EXAM}/enter"),
            &[("testId", test_id.to_string())],
            None,
            true,
        )?;
        serde_json::from_value(data).map_err(|e| Error::Shape(format!("enter 结构异常：{e}")))
    }

    /// 保存作答（**草稿**，不是交卷）。
    ///
    /// 返回体里的 `isSubmitPaper` 非空才表示同时交卷了；存草稿时为 null。
    pub fn save_task_answer(
        &self,
        candidate_id: &str,
        test_id: &str,
        history_id: &str,
        answers: &[Answer],
    ) -> Result<Value> {
        if answers.is_empty() {
            return Err(Error::Invalid("没有可保存的答案".into()));
        }
        let body = json!({
            "candidateId": candidate_id,
            "testId": test_id,
            "historyId": history_id,
            "answers": answers,
        });
        self.request("POST", &format!("{EXAM}/answer/save"), &[], Some(&body), true)
    }

    /// 交卷。**不可逆**。
    ///
    /// 参数是 `historyId`（从 [`Client::enter_task`] 取），**不是 `testId`**。
    pub fn submit_task(&self, history_id: &str) -> Result<Value> {
        self.request(
            "PUT",
            &format!("{EXAM}/submit"),
            &[("historyId", history_id.to_string())],
            None,
            true,
        )
    }

    /// 是否为该任务的分组组长（非分组作业会报错，按 false 处理）。
    pub fn is_task_group_leader(&self, test_id: &str) -> bool {
        let uid = self.session().user_id.clone();
        self.request(
            "GET",
            &format!("{EXAM}/checkIsLeader/{test_id}/{uid}"),
            &[],
            None,
            true,
        )
        .map(|v| v.as_bool().unwrap_or(false))
        .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_homework_counts_as_pending() {
        // 列表里混着视频（type=1）与作业（type=3），不过滤会把每个 mp4 都算成待办
        let video = StudyTask { task_kind: 1, study_status: 0, ..Default::default() };
        assert!(!video.is_pending(), "视频不该算待办作业");
        let hw = StudyTask { task_kind: 3, study_status: 0, ..Default::default() };
        assert!(hw.is_pending());
        let done = StudyTask { task_kind: 3, study_status: 1, ..Default::default() };
        assert!(!done.is_pending(), "已作答不该算待办");
    }

    #[test]
    fn ctl_id_is_the_test_id() {
        // 三个 ID 长得一样，只有 ctlId 是 testId —— 用错报「作业不存在」
        let raw = serde_json::json!({
            "resId": "2102205596197261314",
            "ctlId": "2104363518783819778",
            "dataId": "2102205596155318274",
            "resName": "作业1：实现注册页面",
            "teachCourseName": "嵌入式应用开发",
            "type": 3, "taskType": 1, "studyStatus": 0
        });
        let t: StudyTask = serde_json::from_value(raw).unwrap();
        assert_eq!(t.ctl_id, "2104363518783819778");
        assert_eq!(t.res_id, "2102205596197261314");
        assert_ne!(t.ctl_id, t.res_id, "ctlId 与 resId 不是一回事");
        assert!(t.is_pending());
        assert_eq!(t.course_name, "嵌入式应用开发");
    }

    #[test]
    fn paper_flattens_multiple_parts() {
        // 一份卷子可能有多个大题，questions() 要把它们摊平
        let raw = serde_json::json!({
            "name": "作业1：实现注册页面", "paperScore": "100",
            "paperParts": [
                {"paperQuestionList": [{"paperQuestion": {"questionNo": "1", "questionType": "40"}}]},
                {"paperQuestionList": [
                    {"paperQuestion": {"questionNo": "2", "questionType": "1"}},
                    {"paperQuestion": {"questionNo": "3", "questionType": "1"}}
                ]}
            ]
        });
        let p: Paper = serde_json::from_value(raw).unwrap();
        let qs = p.questions();
        assert_eq!(qs.len(), 3, "两个大题共 3 题应被摊平");
        assert_eq!(qs[0].question_no, "1");
        assert_eq!(qs[2].question_no, "3");
        assert!(qs[0].is_file_upload());
    }

    /// **回归**：题目嵌在 `paperQuestion` 子对象里。
    ///
    /// 用 `flatten` 会静默得到全空的题 —— 实测就是「题型对、题干空、分值 0」。
    #[test]
    fn question_is_nested_under_paper_question() {
        let raw = serde_json::json!({
            "paperParts": [{
                "paperQuestionList": [{
                    "id": "6ab1d79cc61c2b65519c061c",
                    "partId": "p1",
                    "questionType": "40",
                    "paperQuestion": {
                        "questionType": "40",
                        "stem": "<p>仿照已实现的登录页面，实现注册页面。</p>",
                        "stemText": "仿照已实现的登录页面，实现注册页面。",
                        "questionScore": 100.0,
                        "questionNo": "1",
                        "taskEid": "db68a409"
                    }
                }]
            }]
        });
        let p: Paper = serde_json::from_value(raw).unwrap();
        let qs = p.questions();
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0].question_no, "1", "题号没取到（嵌套没解开）");
        assert_eq!(qs[0].score, Some(100.0));
        assert!(qs[0].stem_text.contains("实现注册页面"));
        assert!(qs[0].is_file_upload());
    }

    #[test]
    fn null_integers_become_zero() {
        let raw = serde_json::json!({
            "resId": "r", "ctlId": "c", "type": null,
            "studyStatus": null, "openTime": null, "closeTime": null
        });
        let t: StudyTask = serde_json::from_value(raw).expect("null 数字应收成 0");
        assert_eq!(t.task_kind, 0);
        assert_eq!(t.study_status, 0);
        assert_eq!(t.open_time, 0);
    }

    #[test]
    fn choices_are_parsed() {
        let raw = serde_json::json!({
            "id": "q1", "questionType": "1", "questionNo": "12",
            "candidateAnswer": null,
            "choices": [
                {"choiceId": "D", "choiceContent": " 习近平"},
                {"choiceId": "B", "choiceContent": " 江泽民"}
            ]
        });
        let q: Question = serde_json::from_value(raw).unwrap();
        assert_eq!(q.choices.len(), 2);
        assert_eq!(q.choices[0].id, "D");
        assert!(q.candidate_answer.is_none());
        assert!(!q.is_file_upload());
    }

    #[test]
    fn null_fields_tolerated() {
        let raw = serde_json::json!({"resName": null, "teachCourseName": null, "ctlId": null});
        let t: StudyTask = serde_json::from_value(raw).unwrap();
        assert_eq!(t.res_name, "");
        assert_eq!(t.ctl_id, "");
    }
}

// ───────────────────────── 作业/测验（含已交） ─────────────────────────

impl Client {
    /// 列某门课下的**全部作业/测验**（含已交、未交）。
    ///
    /// ⚠️ 与 `study-task/page` 的区别：那个只返回**待完成**的；
    /// 一个学期作业全交完之后它返回 0 项，界面上就成了「没有作业」。
    /// 这个接口才是课程页「作业」列表的真实来源。
    ///
    /// `source_type`：实测 2 / 3 返回的集合不同（2 更像「任务/测试」列表，
    /// 3 偏「作业」），调用方要**都取一次再按 `resCtl.id` 去重**。
    pub fn course_tests(
        &self,
        teach_course_id: &str,
        class_id: &str,
        source_type: i64,
    ) -> Result<Value> {
        let body = serde_json::json!({
            "teachCourseId": teach_course_id,
            "currentClassId": class_id,
            "sourceType": source_type,
            "dirIds": null,
            "teachTags": [],
            "name": "",
            "isAssess": 0,
            "studyStatus": "",
            "sortType": 5,
        });
        self.request(
            "POST",
            "/web-api/teachmanager/teach-course-res-stu/test/list",
            &[],
            Some(&body),
            true,
        )
    }

    /// 某场测试/作业的**提交概况**（含 `hasSubmitted`、`attemptList[].historyId`）。
    pub fn test_overview(&self, test_id: &str) -> Result<Value> {
        self.request(
            "GET",
            "/web-api/smartexam/test/student/submit/overview",
            &[("testId", test_id.to_string())],
            None,
            true,
        )
    }

    /// 某场测试/作业的**提交记录**（含 `historyId`、客观/主观/最终分）。
    pub fn test_submissions(&self, test_id: &str) -> Result<Value> {
        self.request(
            "GET",
            "/web-api/smartexam/test/student/submit/list",
            &[("sort", "1".to_string()), ("testId", test_id.to_string())],
            None,
            true,
        )
    }

    /// **已交**试卷详情（逐题我的答案 / 标准答案 / 得分 / 解析）。
    ///
    /// 三个 id 缺一不可：`testId` + `candidateId` + `historyId`。
    /// `historyId` 来自 `test_submissions`（**不是** testId。
    pub fn test_mark_paper(
        &self,
        test_id: &str,
        candidate_id: &str,
        history_id: &str,
    ) -> Result<Value> {
        let body = serde_json::json!({
            "testId": test_id,
            "candidateId": candidate_id,
            "historyId": history_id,
            "objectiveQuestionFlag": true,
        });
        self.request(
            "POST",
            "/web-api/smartexam/test/mark/paper/detail",
            &[],
            Some(&body),
            true,
        )
    }
}

/// 从题干/选项的富文本里抽出 `<img src="...">` 的原始地址。
///
/// 平台返回的 `src` 可能是相对路径（`/content/s3/getObjectIO?eid=...`），
/// 也可能是 absolute URL。**不做任何改写**，调用方自己决定怎么取。
///
/// 抽不出来（没有 `<img`）就返回空 vec —— 不是错误，大部分题本来就没有图。
pub fn extract_img_srcs(q: &Question) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push_from = |html: &str| {
        let mut rest = html;
        while let Some(i) = rest.find("<img") {
            rest = &rest[i..];
            let Some(j) = rest.find('>') else { break };
            let tag = &rest[..j];
            for quote in ['"', '\''] {
                if let Some(k) = tag.find(&format!("src={quote}")) {
                    let after = &tag[k + 5..];
                    if let Some(e) = after.find(quote) {
                        let url = after[..e].trim();
                        if !url.is_empty() && !out.iter().any(|x| x == url) {
                            out.push(url.to_string());
                        }
                    }
                }
            }
            rest = &rest[j..];
        }
    };
    push_from(&q.stem);
    push_from(&q.stem_text);
    for c in &q.choices {
        push_from(&c.content);
    }
    out
}
