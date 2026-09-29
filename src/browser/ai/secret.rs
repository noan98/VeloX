//! 資格情報を包む型。`Debug` / `Display` は常に伏せ字を出す。
//!
//! 意図的に `Serialize` / `Deserialize` を実装しない: settings.json など
//! に平文で書き出す道を型で塞ぐ。中身は [`Secret::expose`] を明示的に
//! 呼んだ箇所 (プロバイダが HTTP ヘッダを組み立てる所) でだけ読める。

use std::fmt;

const REDACTED: &str = "[REDACTED]";

#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Secret(value.into())
    }

    /// 環境変数から読む。未設定・空白のみ・非 UTF-8 は `None`。
    pub fn from_env(name: &str) -> Option<Self> {
        std::env::var(name)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map(Secret)
    }

    /// 平文を取り出す。呼び出し側はログ・エラー文言に流さないこと。
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Secret({REDACTED})")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RAW: &str = "sk-super-secret-value-123";

    #[test]
    fn debug_and_display_never_contain_the_secret() {
        let s = Secret::new(RAW);
        for out in [
            format!("{s:?}"),
            format!("{s}"),
            format!("{s:#?}"),
            format!("{:?}", Some(&s)),
            format!("{s:>30}"),
        ] {
            assert!(!out.contains(RAW), "leaked: {out}");
            assert!(!out.contains("secret-value"), "leaked: {out}");
        }
        assert_eq!(s.expose(), RAW);
    }

    #[test]
    fn secret_inside_a_derived_debug_struct_is_redacted() {
        #[derive(Debug)]
        struct Cfg {
            #[allow(dead_code)]
            key: Secret,
        }
        let out = format!(
            "{:?}",
            Cfg {
                key: Secret::new(RAW)
            }
        );
        assert!(!out.contains(RAW));
        assert!(out.contains(REDACTED));
    }

    #[test]
    fn from_env_returns_none_when_unset() {
        assert!(Secret::from_env("VELOX_TEST_SURELY_UNSET_AI_KEY").is_none());
    }
}
