//! BASE58(UUID v4) 字符串身份。
//!
//! 与 Atta 顶层 `Cloud/src/store/id.rs::Id` 兼容；合并。

use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Id(#[serde(with = "id_serde")] [u8; 16]);

#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
pub enum IdError {
    #[error("base58 decode: {0}")]
    Base58(#[from] bs58::decode::Error),
    #[error("expected 16 bytes, got {0}")]
    Length(usize),
}

impl Id {
    /// 随机生成一个 v4 UUID 包成 Id。生成入口仅此一处。
    pub fn new() -> Self {
        Self(*uuid::Uuid::new_v4().as_bytes())
    }

    /// The same sixteen bytes an id is made of, from somewhere other than the
    /// system's randomness — see
    /// [`Environment`](crate::interface::environment::Environment).
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// 从外部字符串解码并验证 16 字节。
    pub fn parse(s: &str) -> Result<Self, IdError> {
        let v = bs58::decode(s).into_vec()?;
        if v.len() != 16 {
            return Err(IdError::Length(v.len()));
        }
        let mut buf = [0u8; 16];
        buf.copy_from_slice(&v);
        Ok(Self(buf))
    }

    /// Raw 16-byte UUID payload (read-only view).
    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl Default for Id {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&bs58::encode(self.0).into_string())
    }
}

mod id_serde {
    use serde::{Deserialize, Deserializer, Serializer};
    /// serde adapter — encodes the 16-byte UUID as BASE58 text.
    pub fn serialize<S: Serializer>(bytes: &[u8; 16], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&bs58::encode(bytes).into_string())
    }
    /// serde adapter — decodes BASE58 text back to 16 bytes.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 16], D::Error> {
        let s = String::deserialize(d)?;
        let v = bs58::decode(&s)
            .into_vec()
            .map_err(serde::de::Error::custom)?;
        if v.len() != 16 {
            return Err(serde::de::Error::custom(format!(
                "expected 16 bytes, got {}",
                v.len()
            )));
        }
        let mut buf = [0u8; 16];
        buf.copy_from_slice(&v);
        Ok(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let a = Id::new();
        let s = a.to_string();
        let b = Id::parse(&s).unwrap();
        assert_eq!(a, b);
        // 16 字节的 BASE58 编码常见 22 字符；首字节为 0 时会输出 21（base58 不带 padding）
        assert!(
            (21..=22).contains(&s.len()),
            "unexpected length: {}",
            s.len()
        );
    }

    #[test]
    fn rejects_bad_length() {
        let bad = bs58::encode([0u8; 8]).into_string();
        assert!(Id::parse(&bad).is_err());
    }

    #[test]
    fn json_roundtrip() {
        let a = Id::new();
        let s = serde_json::to_string(&a).unwrap();
        let b: Id = serde_json::from_str(&s).unwrap();
        assert_eq!(a, b);
    }
}
