//! 考试（智慧考试模块）—— 已交卷列表 + 试卷详情（题目 / 我的答案 / 标准答案 / 得分）。
//!
//! ## 这条链路上要小心的两个地方
//!
//! 1. **`candidateAnswer` 的类型随题型变化** —— 单选是字符串，多选是**字符串数组**。
//!    按字符串硬解多选会直接报错（而不是返回空），所以这里用 `de_answer` 两种都接。
//! 2. **`standardAnswer` 是逗号拼接的字符串**（多选时形如 `id1,id2`），
//!    而 `candidateAnswer` 多选时给的是数组 —— 两者格式**不对称**，别指望能直接比。
//!
//! 题干有 `stem`（HTML）与 `stemText`（纯文本）两份，展示一律用 `stemText`，
//! 拿不到才剥 `stem` 的标签。

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{json, Value};

use crate::client::Client;
use crate::de::{de_f64, de_i64, de_opt_i64, de_str};
use crate::error::Result;

const MY_EXAM: &str = "/web-api/smartexam/my-exam";
/// 试卷详情在**另一个**前缀下（`exam-mark` 而非 `my-exam`）
const EXAM_MARK: &str = "/web-api/smartexam/exam-mark";

/// 一页已交卷考试。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExamList {
    #[serde(default)]
    pub items: Vec<ExamSummary>,
    #[serde(default)]
    pub total: i64,
}

/// 列表里的一条考试。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExamSummary {
    #[serde(default, rename = "examId", deserialize_with = "de_str")]
    pub exam_id: String,
    /// 试卷/考生维度 id —— 取试卷详情要**它**，不是 examId
    #[serde(default, rename = "candidateId", deserialize_with = "de_str")]
    pub candidate_id: String,
    #[serde(default, rename = "examName", deserialize_with = "de_str")]
    pub exam_name: String,
    #[serde(default, rename = "startTime", deserialize_with = "de_i64")]
    pub start_time: i64,
    #[serde(default, rename = "endTime", deserialize_with = "de_i64")]
    pub end_time: i64,
    /// 考试时长（分钟）。平台给的是字符串。
    #[serde(default, rename = "duration", deserialize_with = "de_str")]
    pub duration: String,
    /// 卷面分
    #[serde(default, rename = "score", deserialize_with = "de_str")]
    pub score: String,
    #[serde(default, rename = "status", deserialize_with = "de_str")]
    pub status: String,
    #[serde(default, rename = "statusValue", deserialize_with = "de_str")]
    pub status_value: String,
    #[serde(default, rename = "submitPaperTime", deserialize_with = "de_opt_i64")]
    pub submit_time: Option<i64>,
}

/// 试卷详情（含逐题得分与答案）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExamPaper {
    /// 实际得分（平台算出来的最终分）
    #[serde(default, rename = "calculateScore", deserialize_with = "de_f64")]
    pub calculate_score: f64,
    /// 试卷是否已公布（未公布时平台可能不下发题目）
    #[serde(default, rename = "publishPaper")]
    pub publish_paper: bool,
    /// 分数是否已公布
    #[serde(default, rename = "publishScore")]
    pub publish_score: bool,
    #[serde(default, rename = "resExamCandidatePaper")]
    pub paper: PaperInner,
}

/// 试卷本体。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PaperInner {
    #[serde(default, rename = "examId", deserialize_with = "de_str")]
    pub exam_id: String,
    #[serde(default, rename = "candidateId", deserialize_with = "de_str")]
    pub candidate_id: String,
    #[serde(default, rename = "examName", deserialize_with = "de_str")]
    pub exam_name: String,
    #[serde(default, rename = "nickName", deserialize_with = "de_str")]
    pub nick_name: String,
    #[serde(default, rename = "number", deserialize_with = "de_str")]
    pub number: String,
    #[serde(default, rename = "id", deserialize_with = "de_str")]
    pub id: String,
    #[serde(default, rename = "name", deserialize_with = "de_str")]
    pub name: String,
    #[serde(default, rename = "paperScore", deserialize_with = "de_f64")]
    pub paper_score: f64,
    #[serde(default, rename = "paperQuestionQty", deserialize_with = "de_i64")]
    pub question_qty: i64,
    #[serde(default, rename = "objectiveScore", deserialize_with = "de_f64")]
    pub objective_score: f64,
    #[serde(default, rename = "subjectiveScore", deserialize_with = "de_f64")]
    pub subjective_score: f64,
    /// 最终得分 —— 这个和 `calculateScore` 通常一致，保留两者以防不一致
    #[serde(default, rename = "finalScore", deserialize_with = "de_f64")]
    pub final_score: f64,
    #[serde(default, rename = "submitTime", deserialize_with = "de_opt_i64")]
    pub submit_time: Option<i64>,
    #[serde(default, rename = "markCompleteTime", deserialize_with = "de_opt_i64")]
    pub mark_complete_time: Option<i64>,
    #[serde(default, rename = "paperParts")]
    pub parts: Vec<ExamPart>,
}

/// 一个大题（单选题 / 多选题 / 判断题 …）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExamPart {
    #[serde(default, rename = "partId", deserialize_with = "de_str")]
    pub part_id: String,
    #[serde(default, rename = "partName", deserialize_with = "de_str")]
    pub part_name: String,
    #[serde(default, rename = "partDesc", deserialize_with = "de_str")]
    pub part_desc: String,
    #[serde(default, rename = "paperQuestionQty", deserialize_with = "de_opt_i64")]
    pub question_qty: Option<i64>,
    #[serde(default, rename = "paperQuestionList")]
    pub questions: Vec<QuestionRow>,
}

/// 题目行（平台把题目包在 `paperQuestion` 里）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QuestionRow {
    #[serde(default, rename = "paperQuestion")]
    pub question: Question,
}

/// 一道题。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Question {
    #[serde(default, rename = "questionType", deserialize_with = "de_str")]
    pub question_type: String,
    #[serde(default, rename = "questionNo", deserialize_with = "de_str")]
    pub no: String,
    /// 题干 HTML
    #[serde(default, rename = "stem", deserialize_with = "de_str")]
    pub stem: String,
    /// 题干纯文本（优先用它）
    #[serde(default, rename = "stemText", deserialize_with = "de_str")]
    pub stem_text: String,
    #[serde(default, rename = "questionScore", deserialize_with = "de_f64")]
    pub score: f64,
    #[serde(default, rename = "candidateScore", deserialize_with = "de_f64")]
    pub my_score: f64,
    /// ⚠️ 单选是字符串、多选是数组 —— 两种都要接
    #[serde(default, rename = "candidateAnswer", deserialize_with = "de_answer")]
    pub my_answer: Vec<String>,
    /// ⚠️ 逗号拼接的字符串（多选形如 `id1,id2`），不是数组
    #[serde(default, rename = "standardAnswer", deserialize_with = "de_answer")]
    pub standard_answer: Vec<String>,
    #[serde(default, rename = "analysis", deserialize_with = "de_str")]
    pub analysis: String,
    #[serde(default, rename = "choices")]
    pub choices: Vec<Choice>,
}

/// 一个选项。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Choice {
    #[serde(default, rename = "choiceId", deserialize_with = "de_str")]
    pub id: String,
    #[serde(default, rename = "choiceContent", deserialize_with = "de_str")]
    pub content: String,
}

/// `candidateAnswer` / `standardAnswer` 三态：数组 / 逗号串 / null。
pub fn de_answer<'de, D>(d: D) -> std::result::Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let v = Option::<Value>::deserialize(d)?;
    Ok(match v {
        Some(Value::Array(a)) => a
            .into_iter()
            .filter_map(|x| x.as_str().map(|s| s.to_string()))
            .filter(|s| !s.is_empty())
            .collect(),
        Some(Value::String(s)) => s
            .split(',')
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect(),
        _ => Vec::new(),
    })
}

impl Client {
    /// 已交卷考试列表（分页）。
    ///
    /// `page` 从 **0** 开始（平台的 `pageNum` 是 0 基）。
    pub fn list_finished_exams(&self, page: i64, page_size: i64) -> Result<ExamList> {
        let body = json!({
            "time": "", "keywords": "", "startTime": "", "endTime": "",
            "pageNum": page, "pageSize": page_size,
        });
        let data = self.request(
            "POST",
            &format!("{MY_EXAM}/finished/list"),
            &[],
            Some(&body),
            true,
        )?;
        let items: Vec<ExamSummary> =
            serde_json::from_value(data.get("content").cloned().unwrap_or(Value::Array(vec![])))
                .unwrap_or_default();
        Ok(ExamList {
            items,
            total: data
                .get("totalElements")
                .and_then(Value::as_i64)
                .unwrap_or(0),
        })
    }

    /// 取某场考试的试卷详情（题目、我的答案、标准答案、逐题得分）。
    ///
    /// 路径参数是 `examId` + **`candidateId`**（不是 `examId` 两次，也不是 `paperId`）。
    pub fn get_exam_paper(&self, exam_id: &str, candidate_id: &str) -> Result<ExamPaper> {
        let data = self.request(
            "GET",
            &format!("{EXAM_MARK}/getPaperByExamIdAndPaperId/{exam_id}/{candidate_id}"),
            &[],
            None,
            true,
        )?;
        serde_json::from_value(data)
            .map_err(|e| crate::error::Error::Invalid(format!("试卷结构解析失败：{e}")))
    }
}

/// 题型 → 人话。平台的 `questionType` 是数字串。
pub fn type_label(t: &str) -> &'static str {
    match t {
        "1" => "单选题",
        "2" => "多选题",
        "3" => "判断题",
        "4" => "填空题",
        "5" => "简答题",
        "6" => "论述题",
        "7" => "计算题",
        "8" => "名词解释",
        _ => "题目",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 单选（字符串）、多选（数组）、逗号串三种都要能解。
    ///
    /// 这条链路最容易错的地方就是 `candidateAnswer` 的类型随题型变化 ——
    /// 按字符串硬解多选会**整个响应解析失败**（不是某道题空），
    /// 表现是「考试页一片空白」。
    #[test]
    fn answer_accepts_string_array_and_comma() {
        let q: Question = serde_json::from_value(serde_json::json!({
            "questionType": "1",
            "questionNo": "17",
            "candidateAnswer": "abc",
            "standardAnswer": "abc",
            "choices": [{"choiceId": "abc", "choiceContent": "孙中山"}],
        }))
        .unwrap();
        assert_eq!(q.my_answer, vec!["abc"]);
        assert_eq!(q.standard_answer, vec!["abc"]);
        assert_eq!(q.choices[0].content, "孙中山");

        let q2: Question = serde_json::from_value(serde_json::json!({
            "questionType": "2",
            "candidateAnswer": ["a", "b"],
            "standardAnswer": "a,b",
        }))
        .unwrap();
        assert_eq!(q2.my_answer, vec!["a", "b"]);
        assert_eq!(q2.standard_answer, vec!["a", "b"]);

        // null / 缺失 → 空数组，不 panic
        let q3: Question = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(q3.my_answer.is_empty());
        assert!(q3.standard_answer.is_empty());
    }

    /// 分数既可能是数字也可能是字符串。
    #[test]
    fn score_accepts_number_and_string() {
        let p: PaperInner = serde_json::from_value(serde_json::json!({
            "paperScore": "100",
            "finalScore": 91.0,
            "objectiveScore": 91,
        }))
        .unwrap();
        assert_eq!(p.paper_score, 100.0);
        assert_eq!(p.final_score, 91.0);
        assert_eq!(p.objective_score, 91.0);
    }

    /// 题型映射。
    #[test]
    fn type_labels() {
        assert_eq!(type_label("1"), "单选题");
        assert_eq!(type_label("2"), "多选题");
        assert_eq!(type_label("99"), "题目");
    }
}
