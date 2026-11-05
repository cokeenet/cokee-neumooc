//! 冒烟测试：拿真实账号打一次真实接口。
//!
//! 证明的不是「代码能编译」，而是**这套请求头 + 签名 + 解包在真实服务端被接受**。
//! 凭据从环境变量读，**不写进代码**。
//!
//! ```bash
//! NEUMOOC_USER=学号 NEUMOOC_PASS=密码 NEUMOOC_TENANT=租户ID \
//!   cargo run -p neumooc-core --example smoke
//! ```

use neumooc_core::Client;

fn main() -> neumooc_core::Result<()> {
    env_logger_init();

    let user = std::env::var("NEUMOOC_USER").expect("需要 NEUMOOC_USER");
    let pass = std::env::var("NEUMOOC_PASS").expect("需要 NEUMOOC_PASS");
    let tenant = std::env::var("NEUMOOC_TENANT").expect("需要 NEUMOOC_TENANT");
    let base = std::env::var("NEUMOOC_BASE")
        .unwrap_or_else(|_| "https://study.example.edu.cn".into());

    let mut c = Client::new(base, tenant)?;
    c.remember_credentials(&user, &pass);

    println!("① 登录 …");
    let s = c.login(&user, &pass)?;
    println!(
        "   ✅ userId={} tenantId={} token={}…",
        s.user_id,
        s.tenant_id,
        s.access_token.chars().take(12).collect::<String>()
    );

    println!("\n② 列学期（GET，走签名）…");
    let data = c.request(
        "GET",
        "/web-api/teachmanager/teach-dropdown/getTeachTermDropDown",
        &[],
        None,
        true,
    )?;
    let arr = data.as_array().cloned().unwrap_or_default();
    println!("   ✅ 拿到 {} 个学期", arr.len());
    for t in arr.iter().take(4) {
        println!(
            "      id={} name={}",
            t.get("id").and_then(|v| v.as_str()).unwrap_or("?"),
            t.get("name").and_then(|v| v.as_str()).unwrap_or("?")
        );
    }

    println!("\n③ 考勤列表（POST，走签名）…");
    let term = arr
        .first()
        .and_then(|t| t.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let atts = c.list_attendance(&s.user_id, &term)?;
    println!("   ✅ 拿到 {} 场考勤", atts.len());
    let now = neumooc_core::now_ms();
    let pending: Vec<_> = atts.iter().filter(|a| a.needs_auto_sign(now)).collect();
    println!("      进行中且未签：{} 场", pending.len());
    for a in atts.iter().take(3) {
        println!(
            "      [{}] {} 已签={} 需定位={} 半径={:?}",
            a.course_name, a.title, a.is_signed(), a.needs_location(), a.radius
        );
    }
    if let Some(a) = atts.iter().find(|a| a.needs_location()) {
        println!(
            "      定位样本：要求坐标 ({}, {}) 半径 {:?}m",
            a.longitude.as_deref().unwrap_or("-"),
            a.latitude.as_deref().unwrap_or("-"),
            a.radius
        );
    }

    println!("\n③b 课程与资源扫描…");
    let courses = c.list_courses(&term)?;
    println!("   ✅ {} 门课", courses.len());
    let mut total_res = 0;
    let mut by_type: std::collections::BTreeMap<i64, usize> = Default::default();
    for co in courses.iter().take(4) {
        match c.list_resources(co) {
            Ok(rs) => {
                total_res += rs.len();
                for r in &rs {
                    *by_type.entry(r.res_type).or_default() += 1;
                }
                let todo = rs.iter().filter(|r| r.is_todo()).count();
                println!("      [{}] {} 个资源，待完成 {}", co.name, rs.len(), todo);
            }
            Err(e) => println!("      [{}] 扫描失败：{e}", co.name),
        }
    }
    println!(
        "      前 4 门课共 {} 个资源，类型分布 {:?}",
        total_res, by_type
    );

    println!("\n③c 刷课链路（只挑**已完成**的资源跑，状态不变）…");
    'outer: for co in courses.iter() {
        let rs = c.list_resources(co).unwrap_or_default();
        if let Some(r) = rs.iter().find(|r| r.is_video() && r.is_done()) {
            let before = c.study_status(co, &r.res_id);
            let ok = c.complete_video(co, &r.res_id)?;
            let after = c.study_status(co, &r.res_id);
            println!(
                "      [{}] {} 回读前={} 完成={} 回读后={}",
                co.name, r.name, before, ok, after
            );
            assert_eq!(before, after, "已完成的资源不该被改状态");
            break 'outer;
        }
    }

    println!("\n④ 学习任务（作业在「学习任务」，不在课程资源列表）…");
    let tasks = c.list_study_tasks(&term)?;
    let hw: Vec<_> = tasks.iter().filter(|t| t.is_homework()).collect();
    let pending: Vec<_> = tasks.iter().filter(|t| t.is_pending()).collect();
    println!(
        "   ✅ 共 {} 项任务，其中作业 {} 项，待作答 {} 项",
        tasks.len(),
        hw.len(),
        pending.len()
    );
    for t in hw.iter().take(3) {
        println!(
            "      [{}] {}  {}",
            t.course_name,
            t.res_name,
            if t.is_done() { "已作答" } else { "待作答" }
        );
    }
    if let Some(t) = pending.first() {
        println!("\n⑤ 读题目（testId = ctlId = {}）…", t.ctl_id);
        let paper = c.get_task_paper(&t.ctl_id)?;
        println!(
            "   ✅ {} 满分 {} 截止 {}",
            paper.name, paper.score, paper.close_time_str
        );
        for q in paper.questions().iter().take(2) {
            println!(
                "      第{}题 题型={} {}分{}",
                q.question_no,
                q.question_type,
                q.score.unwrap_or(0.0),
                if q.is_file_upload() { "（文件上传）" } else { "" }
            );
            let stem: String = q.stem_text.chars().take(50).collect();
            println!("        题干：{stem}");
            for ch in q.choices.iter().take(4) {
                println!("          {}. {}", ch.id, ch.content.trim());
            }
            if !q.task_res_name.is_empty() {
                println!("        附件：{}", q.task_res_name);
                match c.file_download(&q.task_eid) {
                    Ok(b) => println!(
                        "        ↓ 下载附件成功：{} 字节（前 4 字节 {:?}）",
                        b.len(),
                        &b[..4.min(b.len())]
                    ),
                    Err(e) => println!("        ↓ 下载附件失败：{e}"),
                }
            }
        }
    } else {
        println!("\n⑤ 没有待作答的作业 —— 跳过读题（账号已全部交完）");
    }

    println!("\n⑥ with_reauth：故意用坏 token 触发重登 …");
    c.load_session(neumooc_core::Session {
        user_id: s.user_id.clone(),
        tenant_id: s.tenant_id.clone(),
        access_token: "definitely-not-a-valid-token".into(),
        ..Default::default()
    });
    let n = c.with_reauth(|cl| {
        cl.request(
            "GET",
            "/web-api/teachmanager/teach-dropdown/getTeachTermDropDown",
            &[],
            None,
            true,
        )
        .map(|d| d.as_array().map(|a| a.len()).unwrap_or(0))
    })?;
    println!("   ✅ 重登后仍然拿到 {n} 个学期 —— 重试生效");

    Ok(())
}

/// 极简日志初始化：不引 env_logger，避免为一个示例多一个依赖。
fn env_logger_init() {}
