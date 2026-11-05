//! 探测：**没有密码、只有有效 token 时，能不能建立 client 并签到？**
use std::sync::{Arc, Mutex};
use neumooc_server::crypto::CredentialCipher;
use neumooc_server::db;
use neumooc_server::tasks::TaskCtx;
use neumooc_server::worker::CancelToken;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut a = std::env::args().skip(1);
    let dbp = a.next().expect("库路径");
    let keyp = a.next().expect("secret.key 路径");
    let id: i64 = a.next().unwrap_or_else(|| "1".into()).parse()?;

    let cipher = Arc::new(CredentialCipher::from_file(&keyp)?);
    let conn = db::open(&dbp)?;
    let ctx = TaskCtx::new(Arc::new(Mutex::new(conn)), cipher);

    println!("▶ 尝试为账号 {id} 建立 client（密码列是空的，token 有效）…");
    match ctx.client(id) {
        Ok(c) => {
            println!("✅ 建立成功（没用到密码）");
            println!("   user_id={:?} tenant={:?}", c.session().user_id, c.tenant_id());
            // 再试试真的调一个接口
            let uid = c.session().user_id.clone();
            match c.list_terms() {
                Ok(t) => println!("✅ 还能拉学期：{} 个", t.len()),
                Err(e) => println!("❌ 拉学期失败：{e}"),
            }
            let _ = uid;
        }
        Err(e) => println!("❌ 建立失败：{e}"),
    }
    let _ = CancelToken::new();
    Ok(())
}
