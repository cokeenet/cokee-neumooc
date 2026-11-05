//! 请求签名（Id-Code）—— 从 Python 版一比一移植。
//!
//! 算法已由 APK dex 指令级证实（v1.0.74）：
//!
//! ```text
//! Id-Code = base64(AES-CBC(key=iv="neuedu_nse_12345", 零填充,
//!                          明文 "{userId}_{uuid}_{path}"))
//! ```
//!
//! 要点（每一条都是踩过的）：
//!
//! * key 与 iv **是同一个常量**（`IndexKt;::<clinit>` 里同一个寄存器 sput 两次）
//! * **零填充**：长度正好是 16 的倍数时**不额外补块**；
//!   明文为空时补一个整块（与 CryptoJS `ZeroPadding` 一致）
//! * 明文用**相对路径**（`options.getUrl()`），**不含域名与 query**
//! * 无 sign / nonce / timestamp 参与
//!
//! 服务端**不校验 uuid 唯一性**，只校验格式可解密，所以用随机 uuid4 即可。

use aes::cipher::{block_padding::NoPadding, BlockEncryptMut, KeyIvInit};
use base64::Engine;

/// 与 APK 一致的固定密钥。注意 **key == iv**。
pub const ID_CODE_KEY: &[u8] = b"neuedu_nse_12345";

const BLOCK: usize = 16;

type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;

/// AES-CBC + 零填充 + base64。
///
/// 零填充语义：先按 16 向上取整，**恰好整除时不补块**；
/// 明文为空时补一个整块。这与 PKCS#7 不同 —— 用错会导致签名全部无效。
pub fn aes_cbc_zero_padding_b64(text: &str) -> String {
    let raw = text.as_bytes();
    // 向上取整；空输入也至少一个块
    let padded_len = if raw.is_empty() {
        BLOCK
    } else {
        raw.len().div_ceil(BLOCK) * BLOCK
    };

    let mut buf = vec![0u8; padded_len];
    buf[..raw.len()].copy_from_slice(raw);

    let ct = Aes128CbcEnc::new(ID_CODE_KEY.into(), ID_CODE_KEY.into())
        .encrypt_padded_mut::<NoPadding>(&mut buf, padded_len)
        .expect("长度已补齐到块大小，不会失败");

    base64::engine::general_purpose::STANDARD.encode(ct)
}

/// 构造 `Id-Code` 请求头。
///
/// * `user_id` —— 当前登录用户 ID；未登录传 `None`（按 APK 记为 0）
/// * `path` —— **相对路径**（如 `/web-api/system/auth/app/login`），
///   **不要带域名与 query**，签名只覆盖 path 部分
pub fn build_id_code(user_id: Option<&str>, path: &str) -> String {
    build_id_code_with_uuid(user_id, path, &uuid::Uuid::new_v4().to_string())
}

/// 可注入 uuid 的版本 —— 让签名**可复现**，测试与对账都靠它。
pub fn build_id_code_with_uuid(user_id: Option<&str>, path: &str, uid_uuid: &str) -> String {
    let uid = match user_id {
        None | Some("") => "0",
        Some(v) => v,
    };
    aes_cbc_zero_padding_b64(&format!("{uid}_{uid_uuid}_{path}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::BlockDecryptMut;

    type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

    fn unpad(b64: &str) -> String {
        let ct = base64::engine::general_purpose::STANDARD.decode(b64).unwrap();
        let mut buf = ct.clone();
        let pt = Aes128CbcDec::new(ID_CODE_KEY.into(), ID_CODE_KEY.into())
            .decrypt_padded_mut::<NoPadding>(&mut buf)
            .unwrap();
        String::from_utf8_lossy(pt).trim_end_matches('\0').to_string()
    }

    #[test]
    fn round_trip() {
        let code = build_id_code_with_uuid(Some("12345"), "/web-api/x/y", "fixed-uuid");
        assert_eq!(unpad(&code), "12345_fixed-uuid_/web-api/x/y");
    }

    #[test]
    fn unlogged_in_uses_zero() {
        let code = build_id_code_with_uuid(None, "/p", "u");
        assert_eq!(unpad(&code), "0_u_/p");
        let code2 = build_id_code_with_uuid(Some(""), "/p", "u");
        assert_eq!(unpad(&code2), "0_u_/p");
    }

    #[test]
    fn output_is_always_block_aligned_base64() {
        // base64 of a 16-multiple is always a multiple of 4 chars, no padding '='
        for n in [0usize, 1, 15, 16, 17, 31, 32, 33] {
            let s = "x".repeat(n);
            let code = build_id_code_with_uuid(Some("1"), &s, "u");
            let raw = base64::engine::general_purpose::STANDARD.decode(&code).unwrap();
            assert_eq!(raw.len() % 16, 0, "明文长度 {n} 时密文未对齐");
        }
    }

    #[test]
    fn exact_multiple_does_not_add_block() {
        // 零填充的要点：恰好整除时不补块。明文太长会立刻暴露（多一个块）。
        // "1_u_" + path，令总长为 16
        let code = build_id_code_with_uuid(Some("1"), &"a".repeat(12), "u");
        let raw = base64::engine::general_purpose::STANDARD.decode(&code).unwrap();
        assert_eq!(raw.len(), 16, "恰好 16 字节时不该补成 32");
    }

    /// **跨语言对账**：这些值是 Python 版 `neumooc/signing.py` 的**真实输出**，
    /// 逐字节抄进来的。
    ///
    /// 为什么必须有这一条：签名算法错一位，服务端只回一个笼统的 401/403，
    /// 排查时完全看不出是签名问题。有了固定向量，移植错了立刻能定位。
    ///
    /// 生成方式（改协议时重新生成）：
    /// ```text
    /// python -c "from neumooc.signing import build_id_code_with_uuid; \
    ///   print(build_id_code_with_uuid('12345','/web-api/system/auth/app/login','fixed-uuid-0001'))"
    /// ```
    #[test]
    fn matches_python_reference_vectors() {
        let cases: &[(Option<&str>, &str, &str, &str)] = &[
            (
                Some("12345"),
                "/web-api/system/auth/app/login",
                "fixed-uuid-0001",
                "iotSqIjwRj9OC7SfH9vpkVjC76EgCGj1RuuOS+76swm2y68/IZFbVQ7SVSGz+ef5k2311/J9jhXeiFr5DDkjzg==",
            ),
            (
                None,
                "/web-api/teachmanager/teach-dropdown/getTeachTermDropDown",
                "u2",
                "8lNAHr1Ibg4XtmgXn7oVPGinXR9fYqYgtRHXNQAH3RH6iNIc8qWYK+6xoCenJOpzakc13MmsJNzcJg6qa5nN0w==",
            ),
            (Some(""), "/p", "u3", "HNDaHxTig8HRbqXtnH2Etg=="),
            (
                Some("1965315534179860482"),
                "/web-api/teachmanager/teach-course-res-stu/study-task/page",
                "abcdef",
                "GSEjtzPehn+4nxluBSmY637vQxi0ljFeBJSJcajrpbBLc5waxb/+DauJbc1xkeDNvvOXNAi+XhMumsFuM4wxaZYkg/qH29tpKk+v1bNMsyXQ9lhCJQmvz7NCKnPpa+df",
            ),
        ];
        for (uid, path, u, want) in cases {
            assert_eq!(
                &build_id_code_with_uuid(*uid, path, u),
                want,
                "与 Python 不一致：uid={uid:?} path={path}"
            );
        }
    }

    #[test]
    fn random_uuid_differs_each_time() {
        let a = build_id_code(Some("1"), "/p");
        let b = build_id_code(Some("1"), "/p");
        assert_ne!(a, b, "每次应使用不同 uuid");
    }
}
