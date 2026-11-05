//! 三级权限。
//!
//! | 角色 | 等级 | 能看到 |
//! | --- | ---: | --- |
//! | `root` | 3 | 全部账号 + 用户管理 |
//! | `admin` | 2 | 全部账号 |
//! | `user` | 1 | **仅绑定给自己的账号** |
//!
//! ## 「未知角色按最低权限处理」
//!
//! [`rank`] 对空值/拼错的值返回 `user` 的等级。反过来（未知当管理员）是
//! 一次拼写错误就等于提权。
//!
//! ## `None` 与 `[]` 的区别（这个区分很重要）
//!
//! [`accessible_account_ids`] 返回 `Option<Vec<i64>>`：
//!
//! * `None` —— **不受限**（admin 及以上）
//! * `Some(vec![])` —— **一个都不能访问**（user 但没绑定任何账号）
//!
//! 把这两者混起来写成「空 vec 就是不受限」，会让没绑账号的用户看到全部账号。
//! 本项目为此修过一次水平越权。

/// 角色等级。
pub fn rank(role: &str) -> u8 {
    match role {
        "root" => 3,
        "admin" => 2,
        // 空值、拼错的值一律按最低权限
        _ => 1,
    }
}

/// 角色值是否合法（**大小写敏感** —— 否则 `ADMIN` 能绕过校验）。
pub fn valid_role(role: &str) -> bool {
    matches!(role, "root" | "admin" | "user")
}

pub fn role_label(role: &str) -> &'static str {
    match role {
        "root" => "超级管理员",
        "admin" => "管理员",
        _ => "普通用户",
    }
}

/// 该角色是否**不受账号限制**。
pub fn unrestricted(role: &str) -> bool {
    rank(role) >= 2
}

/// 权限不足的提示语（直接展示给用户）。
pub fn deny_message(minimum: &str) -> &'static str {
    if minimum == "root" {
        "需要超级管理员权限"
    } else {
        "需要管理员权限"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rank_ordering() {
        assert!(rank("root") > rank("admin"));
        assert!(rank("admin") > rank("user"));
    }

    #[test]
    fn unknown_role_is_lowest_not_highest() {
        // 反向（未知当管理员）意味着一次拼写错误就提权
        for bad in ["", "ADMIN", "Admin", "superuser", "root "] {
            assert_eq!(rank(bad), rank("user"), "{bad:?} 必须按最低权限处理");
        }
    }

    #[test]
    fn role_is_case_sensitive() {
        assert!(valid_role("admin"));
        assert!(!valid_role("ADMIN"), "大小写不敏感会让 ADMIN 绕过校验");
        assert!(!valid_role(""));
    }

    #[test]
    fn only_admin_and_above_are_unrestricted() {
        assert!(!unrestricted("user"));
        assert!(unrestricted("admin"));
        assert!(unrestricted("root"));
        assert!(!unrestricted("nonsense"));
    }
}
