//! 考勤 / 签到 —— 从 Python 版移植，协议见 `docs/ATTENDANCE-PROTOCOL.md`。
//!
//! 这个模块把文档里那几个坑**编进类型与判定**，而不是留给调用方记：
//!
//! 1. `status` 的 `null` 算「进行中」（新发布的场次服务端就这么返回）
//! 2. 已签的判据是 `signRole ∈ {3,4}` **或** `signTime` 非空 —— 只看一个会重复提交
//! 3. `longitude`/`latitude` 是**平台要求**的坐标（考勤点），
//!    `signLongitude`/`signLatitude` 是**实际提交**的值，两者不是一回事

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::client::Client;
use crate::de::{de_num_or_str, de_str};
use crate::error::{Error, Result};

const ATT: &str = "/web-api/teachmanager/teach-course-attendance-detail";

/// 已签的角色。3=教师代签 4=学生自签。
const SIGNED_ROLES: [i64; 2] = [3, 4];

pub const SIGN_STATUS_PRESENT: i64 = 1;

/// 一场考勤。
///
/// ⚠️ 字符串字段全部用 `de_str`（null → 空串），坐标用 `de_num_or_str`
/// （数字或字符串都收）—— **真实接口比文档脏**，这两条都是被真实响应打出来的。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Attendance {
    #[serde(default, rename = "id", deserialize_with = "de_str")]
    pub detail_id: String,
    #[serde(default, rename = "attendanceId", deserialize_with = "de_str")]
    pub attendance_id: String,
    #[serde(default, deserialize_with = "de_str")]
    pub title: String,
    #[serde(default, rename = "courseName", deserialize_with = "de_str")]
    pub course_name: String,
    #[serde(default, rename = "type")]
    pub attendance_type: i64,
    /// ⚠️ 可能是 `null` —— 新发布的场次服务端就这么返回。见 [`Attendance::is_open`]
    #[serde(default)]
    pub status: Option<i64>,
    #[serde(default, rename = "openTime")]
    pub open_time: i64,
    #[serde(default, rename = "finishTime")]
    pub finish_time: i64,
    #[serde(default, rename = "signRole")]
    pub sign_role: Option<i64>,
    #[serde(default, rename = "signTime")]
    pub sign_time: Option<i64>,

    // ---- 定位 ----
    #[serde(default, rename = "attendanceLocationType")]
    pub location_type: Option<i64>,
    /// **平台要求**的坐标与半径（不是学生提交的那个）
    #[serde(default, deserialize_with = "de_num_or_str")]
    pub longitude: Option<String>,
    #[serde(default, deserialize_with = "de_num_or_str")]
    pub latitude: Option<String>,
    #[serde(default, rename = "addressName", deserialize_with = "de_str")]
    pub address_name: String,
    #[serde(default, rename = "attendanceRadius")]
    pub radius: Option<i64>,

    #[serde(default, rename = "teachCourseId", deserialize_with = "de_str")]
    pub teach_course_id: String,
    #[serde(default, rename = "teachClassId", deserialize_with = "de_str")]
    pub teach_class_id: String,
}

impl Attendance {
    /// 是否已签。
    ///
    /// **两个判据缺一不可**：`signRole ∈ {3,4}` 或 `signTime` 非空。
    /// 只看 `signRole` 会漏掉「签了但角色字段为空」的记录，导致重复提交。
    pub fn is_signed(&self) -> bool {
        self.sign_role.is_some_and(|r| SIGNED_ROLES.contains(&r)) || self.sign_time.is_some()
    }

    /// 窗口是否开着（已开始且未结束）。
    ///
    /// ⚠️ `status` 为 `null` **也算进行中** —— 新发布的场次服务端就是这么返回的，
    /// 只判 `== 1` 会漏掉它们。
    pub fn is_open(&self, now_ms: i64) -> bool {
        if !matches!(self.status, None | Some(1)) {
            return false;
        }
        let started = self.open_time == 0 || now_ms >= self.open_time;
        let not_ended = self.finish_time == 0 || now_ms <= self.finish_time;
        started && not_ended
    }

    /// 需要现场定位（GPS）。
    pub fn needs_location(&self) -> bool {
        self.location_type == Some(1)
    }

    /// 「进行中且未签」—— 自动签到的目标。
    pub fn needs_auto_sign(&self, now_ms: i64) -> bool {
        !self.is_signed() && self.is_open(now_ms)
    }

    pub fn seconds_left(&self, now_ms: i64) -> i64 {
        if self.finish_time == 0 {
            0
        } else {
            ((self.finish_time - now_ms) / 1000).max(0)
        }
    }
}

/// 一次签到的参数。
#[derive(Debug, Clone)]
pub struct SignRequest {
    pub detail_id: String,
    pub attendance_id: String,
    pub attendance_type: i64,
    /// 4=学生自签（默认），3=教师代签
    pub sign_role: i64,
    pub sign_user_id: String,
    pub status: i64,
    /// 定位签到用。**传平台要求的坐标**就不需要人到现场。
    pub longitude: Option<String>,
    pub latitude: Option<String>,
    pub address: Option<String>,
    pub location_type: Option<i64>,
    /// 二维码考勤用；该直签通道不校验，固定 "0"
    pub refresh_seed: Option<String>,
    /// **模拟设备 ID（UUID）**。
    ///
    /// 真 App 的 `deviceId` 是 `UUID.randomUUID()` 生成后存 SharedPreferences
    /// （见 `AppUtil.getDeviceId()`），**与硬件无关**，所以填随机值在流量上
    /// 与真机无法区分。
    ///
    /// 平台只用它判「同一 deviceId 对应多个学号」→ 标考勤异常。
    /// **空 = 不带该字段**（默认），此时不参与该判定。
    pub device_id: Option<String>,
}

impl SignRequest {
    /// 从一场考勤构造「自动签到」请求。
    ///
    /// 定位场次**自动带上平台要求的坐标** —— 有它就不需要人到现场。
    pub fn for_auto_sign(a: &Attendance, student_id: &str) -> Result<Self> {
        if a.detail_id.is_empty() || a.attendance_id.is_empty() {
            return Err(Error::Invalid("缺少 detail_id 或 attendance_id".into()));
        }
        let loc = a.needs_location();
        Ok(Self {
            detail_id: a.detail_id.clone(),
            attendance_id: a.attendance_id.clone(),
            attendance_type: a.attendance_type,
            sign_role: 4,
            sign_user_id: student_id.to_string(),
            status: SIGN_STATUS_PRESENT,
            // 非定位场次给 None 而不是空串 —— 空串会被当成「给了坐标」
            longitude: if loc { a.longitude.clone() } else { None },
            latitude: if loc { a.latitude.clone() } else { None },
            address: if loc {
                Some(a.address_name.clone()).filter(|s| !s.is_empty())
            } else {
                None
            },
            location_type: a.location_type,
            // 二维码考勤（type=1）走直签通道，refreshSeed 固定 "0"
            refresh_seed: if a.attendance_type == 1 {
                Some("0".into())
            } else {
                None
            },
            // 由调用方按账号配置填充（见 `TaskCtx::device_id_for`）
            device_id: None,
        })
    }

    fn to_body(&self) -> Value {
        let mut b = json!({
            "attendanceId": self.attendance_id,
            "id": self.detail_id,
            "status": self.status,
            "type": self.attendance_type,
            "signRole": self.sign_role,
            "signUserId": self.sign_user_id,
        });
        // 定位签到：带坐标与地址
        if self.location_type == Some(1) || (self.longitude.is_some() && self.latitude.is_some()) {
            let m = b.as_object_mut().expect("上面刚构造的对象");
            m.insert("signLongitude".into(), json!(self.longitude.clone().unwrap_or_default()));
            m.insert("signLatitude".into(), json!(self.latitude.clone().unwrap_or_default()));
            m.insert("signAddressName".into(), json!(self.address.clone().unwrap_or_default()));
        }
        // 设备 ID：只有配置了才带（空串视为未配置）
        if let Some(d) = &self.device_id {
            if !d.trim().is_empty() {
                b.as_object_mut()
                    .expect("上面刚构造的对象")
                    .insert("deviceId".into(), json!(d.trim()));
            }
        }
        if let Some(seed) = &self.refresh_seed {
            b.as_object_mut()
                .expect("同上")
                .insert("refreshSeed".into(), json!(seed));
        }
        b
    }
}

impl Client {
    /// 查某学期的全部考勤场次（App 版接口）。
    pub fn list_attendance(&self, student_id: &str, term_id: &str) -> Result<Vec<Attendance>> {
        let body = json!({
            "studentId": student_id,
            "termId": term_id,
            "courseId": null,
            "attendanceStatus": null,
            "status": null,
        });
        let data = self.request(
            "POST",
            &format!("{ATT}/getAppStuAttendancePage"),
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

    /// 提交签到。
    pub fn submit_sign(&self, req: &SignRequest) -> Result<Value> {
        self.request("PUT", &format!("{ATT}/update"), &[], Some(&req.to_body()), true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Attendance {
        Attendance {
            detail_id: "d1".into(),
            attendance_id: "a1".into(),
            status: Some(1),
            open_time: 1000,
            finish_time: 2000,
            ..Default::default()
        }
    }

    #[test]
    fn null_status_counts_as_open() {
        // 新发布的场次服务端返回 status=null —— 只判 ==1 会整批漏掉
        let mut a = base();
        a.status = None;
        assert!(a.is_open(1500), "status=null 应当算进行中");
    }

    #[test]
    fn ended_status_not_open() {
        let mut a = base();
        a.status = Some(2);
        assert!(!a.is_open(1500));
    }

    #[test]
    fn signed_by_role_or_time() {
        let mut a = base();
        assert!(!a.is_signed());
        a.sign_role = Some(4);
        assert!(a.is_signed(), "signRole=4 应算已签");
        let mut b = base();
        b.sign_time = Some(123);
        assert!(b.is_signed(), "只有 signTime 也应算已签");
        let mut c = base();
        c.sign_role = Some(1);
        assert!(!c.is_signed(), "signRole=1 不是已签");
    }

    #[test]
    fn auto_sign_target() {
        let a = base();
        assert!(a.needs_auto_sign(1500));
        assert!(!a.needs_auto_sign(500), "未开始不该签");
        assert!(!a.needs_auto_sign(2500), "已结束不该签");
        let mut s = base();
        s.sign_role = Some(4);
        assert!(!s.needs_auto_sign(1500), "已签不该重复签");
    }

    /// **回归**：字段名与 JSON 不一致时必须显式 rename。
    ///
    /// 真实接口用驼峰（`attendanceId` / `courseName`），Rust 字段用下划线；
    /// 漏了 rename 不会报错，只会**静默变成空串** —— 比报错更难发现。
    #[test]
    fn camel_case_fields_are_mapped() {
        let raw = serde_json::json!({
            "id": "d1", "attendanceId": "a1", "courseName": "电子技术基础",
            "teachCourseId": "T1", "teachClassId": "C1",
            "attendanceLocationType": 1, "attendanceRadius": 300
        });
        let a: Attendance = serde_json::from_value(raw).expect("应能解析");
        assert_eq!(a.attendance_id, "a1", "attendanceId 未映射");
        assert_eq!(a.course_name, "电子技术基础", "courseName 未映射");
        assert_eq!(a.teach_course_id, "T1");
        assert_eq!(a.teach_class_id, "C1");
        assert_eq!(a.radius, Some(300));
    }

    #[test]
    fn coords_accept_both_number_and_string() {
        // **真实响应就是这样**：latitude 是浮点、longitude 是字符串。
        // 直接声明成 Option<String> 会在真实接口上直接报类型错。
        let raw = serde_json::json!({
            "id": "d1", "attendanceId": "a1",
            "longitude": "121.5426", "latitude": 38.8977,
            "attendanceRadius": 300, "attendanceLocationType": 1,
            "addressName": "教学楼 A6"
        });
        let a: Attendance = serde_json::from_value(raw).expect("应能解析");
        assert_eq!(a.longitude.as_deref(), Some("121.5426"));
        assert_eq!(a.latitude.as_deref(), Some("38.8977"));
        assert_eq!(a.radius, Some(300));
        assert!(a.needs_location());
    }

    #[test]
    fn null_strings_become_empty() {
        // 真实接口会返回 null 的字符串字段 —— 直接声明 String 会解析失败
        let raw = serde_json::json!({
            "id": "d1", "attendanceId": null, "title": null,
            "courseName": null, "addressName": null
        });
        let a: Attendance = serde_json::from_value(raw).expect("null 应收成空串");
        assert_eq!(a.attendance_id, "");
        assert_eq!(a.title, "");
        assert_eq!(a.course_name, "");
        assert_eq!(a.address_name, "");
    }

    #[test]
    fn missing_coords_are_none() {
        let raw = serde_json::json!({"id": "d1", "attendanceId": "a1"});
        let a: Attendance = serde_json::from_value(raw).expect("应能解析");
        assert!(a.longitude.is_none() && a.latitude.is_none());
    }

    #[test]
    fn location_coords_are_carried() {
        let mut a = base();
        a.location_type = Some(1);
        a.longitude = Some("121.5426".into());
        a.latitude = Some("38.8977".into());
        a.address_name = "教学楼 A6".into();
        let r = SignRequest::for_auto_sign(&a, "u1").unwrap();
        assert_eq!(r.longitude.as_deref(), Some("121.5426"));
        assert_eq!(r.latitude.as_deref(), Some("38.8977"));
        assert_eq!(r.address.as_deref(), Some("教学楼 A6"));
        let b = r.to_body();
        assert_eq!(b["signLongitude"], "121.5426");
        assert_eq!(b["signAddressName"], "教学楼 A6");
    }

    #[test]
    fn non_location_sends_no_coords() {
        // 空串会被当成「给了坐标」而带上错误定位，必须是 None
        let a = base();
        let r = SignRequest::for_auto_sign(&a, "u1").unwrap();
        assert!(r.longitude.is_none() && r.latitude.is_none());
        let b = r.to_body();
        assert!(b.get("signLongitude").is_none(), "非定位场次不该带坐标字段");
    }

    #[test]
    fn qr_attendance_carries_refresh_seed() {
        let mut a = base();
        a.attendance_type = 1;
        let r = SignRequest::for_auto_sign(&a, "u1").unwrap();
        assert_eq!(r.refresh_seed.as_deref(), Some("0"));
        assert_eq!(r.to_body()["refreshSeed"], "0");
    }
}

// ---------------------------------------------------------------- 补签
//
// ## 「重开已结束的场次」为什么必须还原
//
// 服务端对 `detail/update` 的校验顺序是「考勤存在 → **是否已结束** → 才轮到明细」，
// 与 signRole / signUserId / payload 形态无关。所以要补签已结束的场次，
// 必须先把它**临时重开**。
//
// 而重开的那几十秒里，**该场对全班显示为「进行中」** —— 所以还原不是可选项。
// 这里用 RAII 守卫（[`ReopenGuard`]）而不是「记得写 finally」：
// 只要守卫离开作用域就还原，提前 return、`?` 提前传播错误都不影响。

/// 考勤主表快照 —— 重开前记下，用于还原。
#[derive(Debug, Clone)]
pub struct AttendanceSnapshot {
    pub id: String,
    pub status: Option<i64>,
    pub finish_time: Option<i64>,
}

/// 重开守卫：离开作用域自动还原。
///
/// 比 `finally` 更难写错 —— Rust 里提前 `return`、`?` 传播错误、
/// 甚至 panic 都会触发 `Drop`。
pub struct ReopenGuard<'a> {
    client: &'a Client,
    snap: AttendanceSnapshot,
    restored: bool,
}

impl ReopenGuard<'_> {
    /// 还原原状态。成功返回 `true`。
    ///
    /// 一般不需要手动调（`Drop` 会做）；显式调用可以拿到结果用于日志。
    pub fn restore(&mut self) -> bool {
        if self.restored {
            return true;
        }
        self.restored = true;
        self.client
            .update_attendance(&self.snap.id, self.snap.status, self.snap.finish_time)
            .is_ok()
    }
}

impl Drop for ReopenGuard<'_> {
    fn drop(&mut self) {
        if !self.restored {
            // 还原失败只能记日志 —— Drop 里不能抛
            let ok = self.restore();
            if !ok {
                log::error!("考勤 {} 还原失败！该场可能仍显示为「进行中」", self.snap.id);
            }
        }
    }
}

impl Client {
    /// 读考勤主表（重开前取原状态用）。
    pub fn get_attendance(&self, attendance_id: &str) -> Result<Value> {
        self.request(
            "GET",
            &format!("/web-api/teachmanager/teach-course-attendance/get"),
            &[("id", attendance_id.to_string())],
            None,
            true,
        )
    }

    /// 改考勤主表 —— 用于**临时重开**已结束的场次，以及事后还原。
    pub fn update_attendance(
        &self,
        attendance_id: &str,
        status: Option<i64>,
        finish_time: Option<i64>,
    ) -> Result<Value> {
        let body = json!({
            "id": attendance_id,
            "status": status,
            "finishTime": finish_time,
        });
        self.request(
            "PUT",
            "/web-api/teachmanager/teach-course-attendance/update",
            &[],
            Some(&body),
            true,
        )
    }

    /// 取本人在该场次的明细 ID。空串表示不在该教学班。
    pub fn attendance_detail_id(&self, attendance_id: &str, student_id: &str) -> Result<String> {
        let v = self.request(
            "GET",
            &format!("{ATT}/getAttendanceDetailId/{attendance_id}/{student_id}"),
            &[],
            None,
            true,
        )?;
        Ok(match v {
            Value::String(s) => s,
            Value::Number(n) => n.to_string(),
            _ => String::new(),
        })
    }

    /// **临时重开**一个已结束的场次，返回守卫。
    ///
    /// 守卫离开作用域即还原。`window_seconds` 是重开窗口（越小越不显眼，
    /// 但太短可能来不及提交）。
    pub fn reopen_ended(&self, attendance_id: &str, window_seconds: i64) -> Result<ReopenGuard<'_>> {
        let orig = self.get_attendance(attendance_id)?;
        let snap = AttendanceSnapshot {
            id: attendance_id.to_string(),
            status: orig.get("status").and_then(Value::as_i64),
            finish_time: orig.get("finishTime").and_then(Value::as_i64),
        };
        let now = crate::now_ms();
        self.update_attendance(attendance_id, Some(1), Some(now + window_seconds * 1000))?;
        log::warn!(
            "已临时重开考勤 {}（{} 秒）—— 这段时间它对全班显示为「进行中」",
            attendance_id,
            window_seconds
        );
        Ok(ReopenGuard { client: self, snap, restored: false })
    }

    /// 取课程的教师 user id —— **重开已结束场次后补签要用它当 `signUserId`**。
    ///
    /// 教师形态补签（`signRole=3`）才是「不受时间窗校验」的那条路径；
    /// 用学生自己的 uid 会被按学生路径校验「已结束」。
    pub fn course_teacher_id(&self, teach_course_id: &str) -> Result<String> {
        let v = self.request(
            "GET",
            "/web-api/teachmanager/teach-course/get",
            &[("id", teach_course_id.to_string())],
            None,
            true,
        )?;
        Ok(match v.get("teacherId") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Number(n)) => n.to_string(),
            _ => String::new(),
        })
    }

    /// **教师最小形态**补签 —— 重开已结束场次后用它。
    ///
    /// 与 [`Client::submit_sign`] 的区别：
    /// * `signRole = 3`（教师代签），`signUserId` 是**教师 ID**
    /// * **不带** `type` / `refreshSeed` / 坐标 —— 带上会被按学生路径校验时间窗，
    ///   重开就白做了（Python 版注释里专门记过这个坑）
    pub fn makeup_as_teacher(
        &self,
        detail_id: &str,
        attendance_id: &str,
        teacher_id: &str,
        status: i64,
        sign_time: Option<i64>,
    ) -> Result<Value> {
        let mut body = json!({
            "attendanceId": attendance_id,
            "id": detail_id,
            "status": status,
            "signRole": 3,
            "signUserId": teacher_id,
        });
        if let Some(t) = sign_time {
            body["signTime"] = json!(t);
        }
        self.request(
            "PUT",
            &format!("{ATT}/update"),
            &[],
            Some(&body),
            true,
        )
    }
}

/// 签到失败时，把平台错误码翻译成**可操作**的说明。
///
/// ## 为什么需要它
///
/// 这个平台的签到错误码都是 `1020065xxx`，界面上只会显示
/// 「[1020065005] 考勤已结束」这种原文 —— 用户不知道该怎么办。
///
/// ## 关于「设备码校验」
///
/// 2026-11 有反馈称签到接口开始校验设备码。实测（2026-11-05）：
///
/// * 5 个账号当天自动签到 2 场全部成功，`signRole=4`（学生自签）
/// * 考勤列表返回里**没有**任何设备字段
/// * 签到请求体按上游文档只有 `attendanceId/id/status/type/signRole/signUserId`
///   + 定位字段，**没有设备码**
/// * `/system/user-device/*` 三个路径全部 500（接口不存在于这个租户）
///
/// 所以当前**没有**设备校验。但如果将来平台加了，错误码很可能落在
/// `1020065xxx` 段里，且不会明说「设备」。这里预先把「可能跟设备有关」的
/// 响应识别出来，给出明确提示，而不是让用户对着一个数字干瞪眼。
pub fn explain_sign_error(code: Option<i64>, msg: &str) -> String {
    let m = msg.trim();
    // 已明确的业务状态
    if m.contains("考勤已结束") {
        return "考勤已结束 —— 需要在补签里勾选「临时重开已结束场次」".to_string();
    }
    if m.contains("考勤已被删除") {
        return "考勤已被删除 —— 该场次已从平台移除".to_string();
    }
    if m.contains("教师已手动修改") {
        return "教师已手动改过签到状态，不允许再提交".to_string();
    }
    if m.contains("已签到") || m.contains("重复") {
        return "这一场已经签过了".to_string();
    }
    // 可能跟设备/风控有关（关键词或错误码段）
    let device_hint = m.contains("设备")
        || m.contains("device")
        || m.contains("Device")
        || m.contains("未授权")
        || m.contains("非法请求")
        || m.contains("风险");
    if device_hint {
        return format!(
            "{m} —— 疑似平台新增的设备/风控校验。\
             当前签到请求不带设备码；需要抓一次网页端签到请求，\
             对比它的 body 与 headers 缺少了什么"
        );
    }
    if let Some(c) = code {
        if (1020065000..1020066000).contains(&c) && !m.is_empty() {
            return format!("{m}（错误码 {c}）");
        }
    }
    m.to_string()
}
