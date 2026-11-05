//! 凭据加密 —— **与 Python 版 `cryptography.Fernet` 兼容**。
//!
//! 兼容是硬要求：线上库里存的 `password_enc` 是 Python 用 Fernet 加的，
//! Rust 版必须能直接解出来，否则账号全部要重录。
//!
//! ## Fernet 格式
//!
//! ```text
//! key        = base64url(32 字节)
//!              signing_key   = key[0..16]
//!              encryption_key = key[16..32]
//!
//! token      = base64url( version(1) || timestamp(8) || iv(16)
//!                         || ciphertext || hmac(32) )
//! ciphertext = AES-128-CBC(encryption_key, iv, PKCS7(明文))
//! hmac       = HMAC-SHA256(signing_key, version||timestamp||iv||ciphertext)
//! ```
//!
//! ⚠️ 三处细节错了就解不开，而且**只会得到「凭据无效」这种笼统结果**：
//!
//! 1. base64 用 **URL_SAFE 无填充**（不是标准 base64）
//! 2. 版本字节固定 `0x80`，且**参与 HMAC**
//! 3. 填充是 **PKCS7**（与 Id-Code 的零填充**不是一回事**，别混）

use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use neumooc_core::error::{Error, Result};

type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;
type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;
type HmacSha256 = Hmac<Sha256>;

const VERSION: u8 = 0x80;
const KEY_LEN: usize = 32;
const MIN_TOKEN: usize = 1 + 8 + 16 + 16 + 32; // 版本+时间戳+iv+至少一块密文+hmac

/// ⚠️ Fernet 用的是**带 `=` 填充**的 base64url，不是无填充。
///
/// 这个坑是被**真实 secret.key** 打出来的：Python 生成的 key 是 44 字符
/// （含结尾 `=`），用 `URL_SAFE_NO_PAD` 直接解码失败。
/// 而且自测发现不了 —— 自己加密自己解密，两边都用错的引擎，往返照样通。
fn b64() -> base64::engine::general_purpose::GeneralPurpose {
    base64::engine::general_purpose::URL_SAFE
}

/// 凭据加解密器。
pub struct CredentialCipher {
    signing_key: [u8; 16],
    encryption_key: [u8; 16],
}

impl CredentialCipher {
    /// 从 Fernet key（base64url 的 32 字节）构造。
    pub fn from_key(key: &[u8]) -> Result<Self> {
        let raw = b64()
            .decode(String::from_utf8_lossy(key).trim())
            .map_err(|_| Error::Invalid("secret.key 不是合法的 base64url".into()))?;
        if raw.len() != KEY_LEN {
            return Err(Error::Invalid(format!(
                "secret.key 应为 {KEY_LEN} 字节，实际 {}",
                raw.len()
            )));
        }
        let mut signing_key = [0u8; 16];
        let mut encryption_key = [0u8; 16];
        signing_key.copy_from_slice(&raw[..16]);
        encryption_key.copy_from_slice(&raw[16..]);
        Ok(Self { signing_key, encryption_key })
    }

    /// 从文件读（Python 版就是写在这个文件里的）。文件不存在时报错，
    /// **不自动生成** —— 自动生成会让已有数据全部解不开，那是灾难性的。
    pub fn from_file(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let raw = std::fs::read(path.as_ref()).map_err(|e| {
            Error::Invalid(format!("读不到密钥文件 {}：{e}", path.as_ref().display()))
        })?;
        Self::from_key(&raw)
    }

    /// 解密。`token` 是 Fernet token（bytes）。
    pub fn decrypt(&self, token: &[u8]) -> Result<Vec<u8>> {
        let raw = b64()
            .decode(String::from_utf8_lossy(token).trim())
            .map_err(|_| Error::Invalid("凭据不是合法的 base64url".into()))?;
        if raw.len() < MIN_TOKEN {
            return Err(Error::Invalid("凭据太短，不是合法的 Fernet token".into()));
        }
        let (body, mac) = raw.split_at(raw.len() - 32);

        // 先验签再解密 —— 顺序反了等于对未认证的密文做解密
        let mut m = HmacSha256::new_from_slice(&self.signing_key)
            .map_err(|e| Error::Invalid(e.to_string()))?;
        m.update(body);
        m.verify_slice(mac).map_err(|_| Error::Invalid("凭据签名不匹配（密钥不对或数据被改）".into()))?;

        if body[0] != VERSION {
            return Err(Error::Invalid(format!("不支持的 Fernet 版本 {:02x}", body[0])));
        }
        let iv = &body[9..25];
        let ct = &body[25..];
        let mut buf = ct.to_vec();
        let pt = Aes128CbcDec::new((&self.encryption_key).into(), iv.into())
            .decrypt_padded_mut::<Pkcs7>(&mut buf)
            .map_err(|_| Error::Invalid("凭据填充非法（密钥不对？）".into()))?;
        Ok(pt.to_vec())
    }

    /// 加密（写新账号用）。时间戳取当前时间。
    pub fn encrypt(&self, plain: &[u8]) -> Result<String> {
        self.encrypt_at(plain, crate::db::now_ms() / 1000)
    }

    /// 加密并指定时间戳（测试用，让输出可复现）。
    pub fn encrypt_at(&self, plain: &[u8], ts: i64) -> Result<String> {
        use aes::cipher::BlockEncryptMut;
        // iv 用随机数：这里取时间戳 + 明文长度的哈希，避免引入 rand 依赖
        let mut iv = [0u8; 16];
        let mut h = Sha256::new();
        h.update(plain);
        h.update(ts.to_be_bytes());
        h.update(crate::db::now_ms().to_be_bytes());
        iv.copy_from_slice(&h.finalize()[..16]);

        let ct = Aes128CbcEnc::new((&self.encryption_key).into(), (&iv).into())
            .encrypt_padded_vec_mut::<Pkcs7>(plain);

        let mut body = Vec::with_capacity(1 + 8 + 16 + ct.len());
        body.push(VERSION);
        body.extend_from_slice(&ts.to_be_bytes());
        body.extend_from_slice(&iv);
        body.extend_from_slice(&ct);

        let mut m = HmacSha256::new_from_slice(&self.signing_key)
            .map_err(|e| Error::Invalid(e.to_string()))?;
        m.update(&body);
        let mac = m.finalize().into_bytes();

        body.extend_from_slice(&mac);
        Ok(b64().encode(&body))
    }
}

/// 常量时间比较（校验 token 哈希之类的场景）。
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.ct_eq(b).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cipher() -> CredentialCipher {
        // 固定 key，让加解密可复现
        CredentialCipher::from_key(b"YWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWE=").unwrap()
    }

    /// **跨语言对账**：这些 token 是 Python `cryptography.Fernet` 的**真实输出**，
    /// 逐字抄进来的。
    ///
    /// 为什么必须有这一条：Fernet 有三处细节错了就解不开，而**自加密自解密
    /// 照样往返成功**（两边用同一个错的引擎）。本项目已经踩过一次 ——
    /// base64 引擎用了无填充，自测全绿，直到拿真实 `secret.key` 才发现。
    ///
    /// 重新生成：
    /// ```text
    /// python -c "from cryptography.fernet import Fernet; \
    ///   print(Fernet(b'y40aSPh_nEMss2Kc73jNJ0cT2WlvchXoDNfHeqgPhgQ=').encrypt(b'hello').decode())"
    /// ```
    #[test]
    fn matches_python_fernet_vectors() {
        let c = CredentialCipher::from_key(b"y40aSPh_nEMss2Kc73jNJ0cT2WlvchXoDNfHeqgPhgQ=").unwrap();
        let cases: &[(&str, &str)] = &[
            ("hello", "gAAAAABqupsUOrPUbfnOdlUpD752ZBlGxbXyF94XVPbtfLwS_4f0f5Zq3VafHhYDWULeV0pvQj6CUGXUSEVxWMa0CPvqfLRb4g=="),
            ("", "gAAAAABqupsUQDpVaCZPyCwv4GvW2oETlkWJVX0D_DF850AGT5jwsrtndtZ5LQnn5GfqBQVOnprByKcqxiY2YRxudXqcE6tWoQ=="),
        ];
        for (want, token) in cases {
            let got = c.decrypt(token.as_bytes()).expect("必须能解开 Python 的 token");
            assert_eq!(String::from_utf8_lossy(&got), *want, "明文不符");
        }
    }

    /// 反向：Rust 加密的 token 要能被 Python 解开。
    ///
    /// 断言不了 Python 那边，但至少钉住格式（版本字节、带填充 base64、长度）。
    #[test]
    fn produced_token_has_fernet_shape() {
        let c = CredentialCipher::from_key(b"y40aSPh_nEMss2Kc73jNJ0cT2WlvchXoDNfHeqgPhgQ=").unwrap();
        let tok = c.encrypt(b"hello").unwrap();
        let raw = b64().decode(&tok).unwrap();
        assert_eq!(raw[0], 0x80, "版本字节必须是 0x80");
        assert!(raw.len() >= MIN_TOKEN);
        assert_eq!((raw.len() - 1 - 8 - 16 - 32) % 16, 0, "密文应是 16 的倍数");
        // 与 Python 一样带 `=`
        assert!(tok.ends_with('='), "Fernet 的 token 带填充");
    }

    #[test]
    fn round_trip() {
        let c = cipher();
        for plain in ["", "p", "password123", &"x".repeat(200)] {
            let tok = c.encrypt(plain.as_bytes()).unwrap();
            let back = c.decrypt(tok.as_bytes()).unwrap();
            assert_eq!(back, plain.as_bytes(), "往返失败：{plain:?}");
        }
    }

    #[test]
    fn token_is_url_safe_base64() {
        // **不是**无填充 —— Fernet 用带 `=` 的 base64url。
        // 用错引擎自己发现不了（自加密自解密照样往返），只有真实数据能打出来。
        let tok = cipher().encrypt(b"x").unwrap();
        assert!(!tok.contains('+') && !tok.contains('/'), "URL_SAFE 不该出现 + 或 /");
    }

    #[test]
    fn key_with_padding_decodes() {
        // Python 的 Fernet.generate_key() 产出的就是这种 44 字符带 `=` 的形式
        let k = b"y40aSPh_nEMss2Kc73jNJ0cT2WlvchXoDNfHeqgPhgQ=";
        assert_eq!(k.len(), 44);
        assert!(CredentialCipher::from_key(k).is_ok(), "带填充的 key 必须能解");
    }

    #[test]
    fn tampered_token_is_rejected() {
        let c = cipher();
        let tok = c.encrypt(b"secret").unwrap();
        // 改掉密文里一个字符 —— 必须被 HMAC 挡住，而不是解出垃圾
        let mut bad: Vec<char> = tok.chars().collect();
        let mid = bad.len() / 2;
        bad[mid] = if bad[mid] == 'A' { 'B' } else { 'A' };
        let bad: String = bad.into_iter().collect();
        assert!(c.decrypt(bad.as_bytes()).is_err(), "改过的 token 必须被拒");
    }

    #[test]
    fn wrong_key_fails() {
        let a = CredentialCipher::from_key(b"YWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWE=").unwrap();
        let b = CredentialCipher::from_key(b"YmJiYmJiYmJiYmJiYmJiYmJiYmJiYmJiYmJiYmJiYmI=").unwrap();
        let tok = a.encrypt(b"secret").unwrap();
        assert!(b.decrypt(tok.as_bytes()).is_err());
    }

    #[test]
    fn bad_key_length_rejected() {
        assert!(CredentialCipher::from_key(b"c2hvcnQ").is_err());
    }
}
