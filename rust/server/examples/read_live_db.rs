//! **兼容性验收**：用 Rust 直接打开 Python 版的线上库，解开已存的账号密码。
//!
//! 这是数据迁移成立与否的判据 —— 解不开就意味着所有账号要重录。
//!
//! ```bash
//! cargo run -p neumooc-server --example read_live_db -- <库路径> <secret.key 路径>
//! ```

use neumooc_server::crypto::CredentialCipher;
use neumooc_server::db;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut a = std::env::args().skip(1);
    let dbp = a.next().expect("需要库路径");
    let keyp = a.next().expect("需要 secret.key 路径");

    let cipher = CredentialCipher::from_file(&keyp)?;
    println!("① secret.key 读取成功");

    let conn = db::open(&dbp)?;
    println!("② 线上库打开成功（schema 未改动）");

    let n: i64 = conn.query_row("SELECT count(*) FROM accounts", [], |r| r.get(0))?;
    println!("   accounts 共 {n} 行");

    let mut ok = 0;
    let mut stmt = conn.prepare(
        "SELECT id, label, username, password_enc, tenant_id FROM accounts ORDER BY id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Vec<u8>>(3)?,
            r.get::<_, String>(4)?,
        ))
    })?;
    for row in rows {
        let (id, label, username, enc, tenant) = row?;
        match cipher.decrypt(&enc) {
            Ok(pw) => {
                ok += 1;
                // **不打印明文**，只报长度与指纹
                let fp: u32 = pw.iter().fold(0u32, |h, &b| h.wrapping_mul(31).wrapping_add(b as u32));
                println!(
                    "   #{id} {label} ({username}) tenant={tenant} 解密 ✅ 长度={} 指纹={fp:08x}",
                    pw.len()
                );
            }
            Err(e) => println!("   #{id} {label} 解密 ❌ {e}"),
        }
    }
    println!("\n③ {ok}/{n} 个账号密码可解密");
    assert_eq!(ok, n, "有账号解不开 —— 迁移不成立");

    // 顺带确认其它表读得动
    for t in ["attendance", "courses", "resources", "executions", "logs", "users"] {
        let c: i64 = conn.query_row(&format!("SELECT count(*) FROM {t}"), [], |r| r.get(0))?;
        println!("   {t:12} {c:>5} 行");
    }
    Ok(())
}
