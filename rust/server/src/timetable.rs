//! 课表驱动的轮询窗口。
//!
//! ## 为什么需要课表
//!
//! 考勤只会在**上课时间**发生 —— 没有课的时候问平台纯属浪费，
//! 而且正是这种空转让我们被 WAF 判定成爬虫（一天几千次无差别请求）。
//!
//! 早先的实现按「历史考勤的小时分布」猜（`poll_policy`），够用但粗糙：
//! 只知道"上午下午可能有课"，不知道**今天这节课到底上不上**、
//! 也不知道**这节课第几周**。有了教务课表就能精确到分钟。
//!
//! ## 数据来源
//!
//! 金智教务系统的 `getMyScheduleDetail.do`，
//! **校外可访问**（不需要校园网/VPN），所以这条路不会被校园网封禁影响。
//!
//! 每条课有：`dayOfWeek`（1-7）、`beginTime`/`endTime`（"10:00"/"11:40"）、
//! `weeksAndTeachers`（"6周[讲授]/某老师[主讲]" 或 "1-16周[讲授]/…"）。
//!
//! ## 窗口怎么算
//!
//! 一次考勤通常在上课后**几分钟内**发起、持续约 10 分钟。
//! 所以在 `[beginTime, endTime]` 之外各留 `MARGIN` 分钟余量，
//! 只有落在窗口内才轮询。**课时之外一次都不查。**

use serde::{Deserialize, Serialize};

/// 课堂开始**前**留多少分钟余量。
///
/// ## 按 383 条历史考勤实测定的，不是拍脑袋
///
/// 课前签到的提前量分布：
///
/// ```
/// 提前  5 分钟: 147 次   ← 最多，老师课前发二维码
/// 提前  0 分钟:  47 次
/// 提前 10 分钟:  26 次
/// 提前 15 分钟:   8 次   ← 最长的一批
/// ```
///
/// **10 分钟不够**：覆盖率只有 89.6%，会漏掉「提前 10~15 分钟」那批。
/// 改 20 分钟后覆盖率升到 96.6%。
pub const PRE_MARGIN_MIN: i32 = 20;

/// 课堂结束**后**留多少分钟余量。
///
/// 课后签到（老师在**下课时**发考勤）的延后量：
///
/// ```
/// 延后  0 分钟: 18 次   ← 下课签
/// 延后 10 分钟:  1 次
/// ```
///
/// 用户在群里指出过：「第二节这个课是**下课签到**，
/// 不是上课二十分钟签」。所以尾部余量必须留够，否则下课那波全漏。
pub const POST_MARGIN_MIN: i32 = 15;

/// 兼容旧的单值写法（外部还引用 `MARGIN_MIN`）。
#[deprecated(note = "改用 PRE_MARGIN_MIN / POST_MARGIN_MIN，前后余量不再相同")]
pub const MARGIN_MIN: i32 = PRE_MARGIN_MIN;

/// 一门课的一节课（一条课表格子）。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Lesson {
    /// 1 = 周一 … 7 = 周日
    #[serde(default, rename = "dayOfWeek")]
    pub day_of_week: i32,
    /// "10:00"
    ///
    /// 同时接受 `beginTime`（教务原始字段）和 `begin`（抓取脚本写出的精简名）。
    /// **两种都要认** —— 曾经只写了 `beginTime`，而脚本输出的是 `begin`，
    /// 反序列化时静默变成空串，所有窗口计算失效、自动签到一次都不跑，
    /// 但日志里毫无报错（`serde(default)` 把字段缺失当成"合法空值"）。
    #[serde(default, alias = "begin", rename = "beginTime")]
    pub begin: String,
    /// "11:40"
    #[serde(default, alias = "end", rename = "endTime")]
    pub end: String,
    /// "6周[讲授]/某老师[主讲]" / "1-16周[讲授]/…"
    #[serde(default, rename = "weeksAndTeachers")]
    pub weeks: String,
    #[serde(default, rename = "courseName")]
    pub course: String,
    #[serde(default, rename = "placeName")]
    pub place: String,
}

impl Lesson {
    /// 这节课在指定的「第几周」上不上。
    ///
    /// 解析三种写法（教务实际都会出现）：
    ///
    /// * `6周[...]`        → 只在第 6 周
    /// * `1-16周[...]`     → 第 1~16 周（含两端）
    /// * `1-16周(单)` / `(双)` → 单双周（部分课程有）
    ///
    /// **解析不出来时返回 `true`**（宁可多查一次，也不要因为格式没料到就漏签）。
    pub fn active_in_week(&self, week: i32) -> bool {
        let Some(range) = parse_weeks(&self.weeks) else {
            return true; // 格式不认识 → 保守地认为要上
        };
        match range {
            WeekRange::All(a, b) => week >= a && week <= b,
            WeekRange::Odd(a, b) => week >= a && week <= b && week % 2 == 1,
            WeekRange::Even(a, b) => week >= a && week <= b && week % 2 == 0,
        }
    }

    /// 这节课的分钟级区间（当天 0 点起的分钟数）。
    pub fn minutes(&self) -> Option<(i32, i32)> {
        Some((parse_hm(&self.begin)?, parse_hm(&self.end)?))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WeekRange {
    All(i32, i32),
    Odd(i32, i32),
    Even(i32, i32),
}

/// 从 `weeksAndTeachers` 里抽周次范围。
///
/// 只取**第一段**（`6周[讲授]` 里的 `6周`）—— 后面是教师名，可能也含数字。
fn parse_weeks(s: &str) -> Option<WeekRange> {
    // 找第一个「数字 或 数字-数字 后面紧跟 周」
    let bytes: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start_i = i;
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == '-') {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == '周' {
                let spec: String = bytes[start_i..i].iter().collect();
                // 周字后面可能有 (单)/(双)
                let tail: String = bytes[(i + 1).min(bytes.len())..]
                    .iter()
                    .take(4)
                    .collect();
                let odd = tail.contains("单");
                let even = tail.contains("双");
                let (a, b) = match spec.split_once('-') {
                    Some((x, y)) => (x.parse().ok()?, y.parse().ok()?),
                    None => {
                        let n: i32 = spec.parse().ok()?;
                        (n, n)
                    }
                };
                return Some(if odd {
                    WeekRange::Odd(a, b)
                } else if even {
                    WeekRange::Even(a, b)
                } else {
                    WeekRange::All(a, b)
                });
            }
        } else {
            i += 1;
        }
    }
    None
}

/// "10:00" → 600（当天第几分钟）。
fn parse_hm(s: &str) -> Option<i32> {
    let (h, m) = s.trim().split_once(':')?;
    Some(h.trim().parse::<i32>().ok()? * 60 + m.trim().parse::<i32>().ok()?)
}

/// 判断「现在」要不要轮询。
///
/// `lessons` 是当天的课（调用方已按 `dayOfWeek` 过滤）；
/// `now` 是本地时间的 (小时, 分钟)。
///
/// 返回 `Some(课程名)` 表示在窗口内、且这门课的时间匹配 ——
/// 带上课程名是为了日志里能看出「为什么现在查」。
pub fn in_class_window(lessons: &[Lesson], now_h: i32, now_m: i32) -> Option<&Lesson> {
    let now = now_h * 60 + now_m;
    lessons.iter().find(|l| {
        let Some((b, e)) = l.minutes() else {
            // 时间解析不出来：当成全天要查（保守）
            return true;
        };
        now >= b - PRE_MARGIN_MIN && now <= e + POST_MARGIN_MIN
    })
}

/// 距离下一节课开始还有多少分钟（用于决定睡多久）。
///
/// 没有课了返回 `None`。
pub fn minutes_to_next(lessons: &[Lesson], now_h: i32, now_m: i32) -> Option<i32> {
    let now = now_h * 60 + now_m;
    lessons
        .iter()
        .filter_map(|l| l.minutes().map(|(b, _)| b))
        .filter(|b| *b > now)
        .min()
        .map(|b| b - now)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lesson(dow: i32, b: &str, e: &str, weeks: &str) -> Lesson {
        Lesson {
            day_of_week: dow,
            begin: b.into(),
            end: e.into(),
            weeks: weeks.into(),
            course: "测试课".into(),
            place: "A1-101".into(),
        }
    }

    #[test]
    fn parses_single_week() {
        let l = lesson(1, "10:00", "11:40", "6周[讲授]/某老师[主讲]");
        assert!(!l.active_in_week(5));
        assert!(l.active_in_week(6));
        assert!(!l.active_in_week(7));
    }

    #[test]
    fn parses_week_range() {
        let l = lesson(1, "08:00", "09:40", "1-16周[讲授]/某老师[主讲]");
        assert!(l.active_in_week(1));
        assert!(l.active_in_week(16));
        assert!(!l.active_in_week(17));
        assert!(!l.active_in_week(0));
    }

    #[test]
    fn parses_odd_even() {
        let odd = lesson(2, "10:00", "11:40", "1-16周(单)");
        assert!(odd.active_in_week(1));
        assert!(!odd.active_in_week(2));
        let even = lesson(2, "10:00", "11:40", "1-16周(双)");
        assert!(!even.active_in_week(1));
        assert!(even.active_in_week(2));
    }

    #[test]
    fn unknown_weeks_is_permissive() {
        // 解析不出来宁可多查 —— 漏签比多查一次严重得多
        let l = lesson(1, "10:00", "11:40", "乱写的格式");
        assert!(l.active_in_week(1));
        assert!(l.active_in_week(20));
    }

    #[test]
    fn window_has_margin() {
        let ls = vec![lesson(1, "10:00", "11:40", "1-16周")];
        // 09:40 = 开始前 20 分钟，正好在边界内
        assert!(in_class_window(&ls, 9, 40).is_some());
        // 09:39 就出去了
        assert!(in_class_window(&ls, 9, 39).is_none());
        // 11:55 = 结束后 15 分钟，边界内
        assert!(in_class_window(&ls, 11, 55).is_some());
        assert!(in_class_window(&ls, 11, 56).is_none());
        // 课中间当然在
        assert!(in_class_window(&ls, 10, 30).is_some());
    }

    #[test]
    fn no_lessons_means_no_poll() {
        assert!(in_class_window(&[], 10, 0).is_none());
    }

    #[test]
    fn next_lesson_gap() {
        let ls = vec![
            lesson(1, "08:00", "09:40", "1-16周"),
            lesson(1, "10:00", "11:40", "1-16周"),
        ];
        // 07:00 → 距 08:00 还有 60 分钟
        assert_eq!(minutes_to_next(&ls, 7, 0), Some(60));
        // 09:00 → 距 10:00 还有 60（08:00 那节已过）
        assert_eq!(minutes_to_next(&ls, 9, 0), Some(60));
        // 12:00 → 今天没课了
        assert_eq!(minutes_to_next(&ls, 12, 0), None);
    }
}

// ───────────────────── 从教务快照生成轮询时段 ─────────────────────

/// 一份课表快照：`dayOfWeek(1-7)` → 当天的所有课。
///
/// **刻意忽略周次**。原因：教务 API 只给"6周"这种相对周次，
/// 不给学期起始日，所以算不出"今天是第几周"。
/// 与其依赖一个不可靠的推算，不如接受"没课的日子多查几次" ——
/// 代价是每天几十次，收益是**不再需要任何额外数据源**。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Timetable {
    /// 1 = 周一 … 7 = 周日
    pub by_weekday: std::collections::HashMap<i32, Vec<Lesson>>,
    /// 生成这份时段表的时间（毫秒），用于判断是否过期
    #[serde(default)]
    pub built_at: i64,
    /// 学期标识（"2026-2027-1"），学期变了要重新抓
    #[serde(default)]
    pub term: String,
}

impl Timetable {
    /// 从教务的 `arrangedList` 构造。
    pub fn from_arranged(list: &[Lesson]) -> Self {
        let mut by_weekday: std::collections::HashMap<i32, Vec<Lesson>> = Default::default();
        for l in list {
            if (1..=7).contains(&l.day_of_week) {
                by_weekday.entry(l.day_of_week).or_default().push(l.clone());
            }
        }
        // 每天内按开始时间排好，日志里好读
        for v in by_weekday.values_mut() {
            v.sort_by_key(|l| l.minutes().map(|(b, _)| b).unwrap_or(i32::MAX));
        }
        Self { by_weekday, built_at: 0, term: String::new() }
    }

    /// 今天（按 `weekday` 1-7）的课。
    pub fn today(&self, weekday: i32) -> &[Lesson] {
        self.by_weekday.get(&weekday).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// 今天要轮询的分钟区间（含 margin）。
    ///
    /// 重叠的区间会合并 —— 连堂课（比如 10:00-11:40 和 11:00-12:40）
    /// 不该让调用方在两个窗口之间来回切。
    pub fn today_windows(&self, weekday: i32) -> Vec<(i32, i32)> {
        let mut v: Vec<(i32, i32)> = self
            .today(weekday)
            .iter()
            .filter_map(|l| l.minutes())
            .map(|(b, e)| (b - PRE_MARGIN_MIN, e + POST_MARGIN_MIN))
            .collect();
        v.sort();
        let mut merged: Vec<(i32, i32)> = Vec::new();
        for (b, e) in v {
            match merged.last_mut() {
                Some(last) if b <= last.1 => last.1 = last.1.max(e),
                _ => merged.push((b, e)),
            }
        }
        merged
    }

    /// 当前时刻是否落在某个上课窗口里。
    pub fn in_window(&self, weekday: i32, now_min: i32) -> Option<(i32, i32)> {
        self.today_windows(weekday)
            .into_iter()
            .find(|(b, e)| now_min >= *b && now_min <= *e)
    }

    /// 距离下一个窗口开始还有多少分钟（今天没有了返回 `None`）。
    pub fn minutes_to_window(&self, weekday: i32, now_min: i32) -> Option<i32> {
        self.today_windows(weekday)
            .into_iter()
            .filter(|(b, _)| *b > now_min)
            .map(|(b, _)| b - now_min)
            .min()
    }

    /// 今天还有没有课了。
    pub fn has_more_today(&self, weekday: i32, now_min: i32) -> bool {
        self.today_windows(weekday).iter().any(|(_, e)| *e >= now_min)
    }
}

/// 从 JSON 文件读课表。读不到 / 解析失败返回 `None`（调用方退回启发式）。
///
/// **不 panic**：课表是"锦上添花"的优化，它坏了不该让整个调度停摆。
pub fn load_from_file(path: &str) -> Option<Timetable> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<Timetable>(&text).ok()
}

#[cfg(test)]
mod timetable_tests {
    use super::*;

    fn l(dow: i32, b: &str, e: &str) -> Lesson {
        Lesson {
            day_of_week: dow,
            begin: b.into(),
            end: e.into(),
            weeks: "1-16周".into(),
            course: format!("课{dow}"),
            place: String::new(),
        }
    }

    #[test]
    fn groups_by_weekday() {
        let tt = Timetable::from_arranged(&[l(1, "08:00", "09:40"), l(1, "10:00", "11:40"), l(6, "10:00", "11:40")]);
        assert_eq!(tt.today(1).len(), 2);
        assert_eq!(tt.today(6).len(), 1);
        assert_eq!(tt.today(3).len(), 0);
        // 排好序
        assert_eq!(tt.today(1)[0].begin, "08:00");
    }

    #[test]
    fn windows_include_margin() {
        let tt = Timetable::from_arranged(&[l(1, "10:00", "11:40")]);
        let w = tt.today_windows(1);
        assert_eq!(w.len(), 1);
        // 课前 20 分钟、课后 15 分钟（按 383 条实测数据定的）
        assert_eq!(w[0], (10 * 60 - PRE_MARGIN_MIN, 11 * 60 + 40 + POST_MARGIN_MIN));
    }

    #[test]
    fn overlapping_windows_merge() {
        // 10:00-11:40 与 11:00-12:40 重叠 → 合成一个窗口
        let tt = Timetable::from_arranged(&[l(1, "10:00", "11:40"), l(1, "11:00", "12:40")]);
        let w = tt.today_windows(1);
        assert_eq!(w.len(), 1, "重叠窗口应合并，实际 {w:?}");
        assert_eq!(w[0].0, 10 * 60 - PRE_MARGIN_MIN);
        assert_eq!(w[0].1, 12 * 60 + 40 + POST_MARGIN_MIN);
    }

    #[test]
    fn gap_between_windows_preserved() {
        // 08:00-09:40 与 14:00-15:40 中间有空 → 两个窗口
        let tt = Timetable::from_arranged(&[l(1, "08:00", "09:40"), l(1, "14:00", "15:40")]);
        assert_eq!(tt.today_windows(1).len(), 2);
    }

    #[test]
    fn in_window_and_next() {
        let tt = Timetable::from_arranged(&[l(6, "10:00", "11:40")]);
        assert!(tt.in_window(6, 10 * 60).is_some());
        assert!(tt.in_window(6, 9 * 60).is_none(), "9 点还没到窗口");
        // 10:00 前 20 分钟 = 09:40 起是窗口
        assert_eq!(tt.minutes_to_window(6, 9 * 60), Some(40));
        assert_eq!(tt.minutes_to_window(6, 12 * 60), None, "12 点已过窗口（11:55 结束）");
        assert!(!tt.has_more_today(6, 12 * 60));
        assert!(tt.has_more_today(6, 11 * 60));
    }

    /// **回归**：抓取脚本写出的 JSON 必须能被 `load_from_file` 解析出可用窗口。
    ///
    /// 踩过一次：脚本输出 `begin`/`end`，结构体只认 `beginTime`/`endTime`，
    /// `serde(default)` 把缺失字段当空串 —— **解析成功、窗口全空、一声不响**。
    /// 自动签到因此一次都没跑，而日志里没有任何错误。
    #[test]
    fn parses_real_fetcher_output() {
        let json = r#"{
          "term": "2026-2027-1",
          "built_at": 1793971200000,
          "by_weekday": {
            "4": [
              {"day_of_week":4,"begin":"08:00","end":"09:40","weeks":"3-14周","course":"某课","place":"A1"}
            ]
          }
        }"#;
        let tt: Timetable = serde_json::from_str(json).expect("应能解析");
        // 关键：窗口必须真的算得出来，不是空
        let w = tt.today_windows(4);
        assert!(!w.is_empty(), "窗口为空说明 begin/end 没解出来");
        assert_eq!(
            w[0],
            (8 * 60 - PRE_MARGIN_MIN, 9 * 60 + 40 + POST_MARGIN_MIN),
            "08:00-09:40 前 20 / 后 15 分钟"
        );
        // 以及窗口内判定
        assert!(tt.in_window(4, 8 * 60).is_some());
    }

    #[test]
    fn empty_timetable_polls_nothing() {
        let tt = Timetable::default();
        assert!(tt.in_window(1, 600).is_none());
        assert_eq!(tt.minutes_to_window(1, 600), None);
        assert!(!tt.has_more_today(1, 600));
    }
}
