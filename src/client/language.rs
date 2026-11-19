//! Local presentation language. Protocol keys and user/tool content stay unchanged.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum Language {
    #[default]
    #[serde(rename = "en")]
    English,
    #[serde(rename = "zh")]
    Chinese,
}

impl Language {
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "en" => Ok(Self::English),
            "zh" => Ok(Self::Chinese),
            _ => Err("language must be en or zh".into()),
        }
    }

    pub(crate) const fn code(self) -> &'static str {
        match self {
            Self::English => "en",
            Self::Chinese => "zh",
        }
    }

    pub(crate) const fn text<'a>(self, english: &'a str, chinese: &'a str) -> &'a str {
        match self {
            Self::English => english,
            Self::Chinese => chinese,
        }
    }
}

/// Both format strings are checked at compile time. Values are never translated.
macro_rules! localized {
    ($language:expr, $english:literal, $chinese:literal $(, $($args:tt)*)?) => {
        match $language {
            $crate::client::language::Language::English => format!($english $(, $($args)*)?),
            $crate::client::language::Language::Chinese => format!($chinese $(, $($args)*)?),
        }
    };
}
pub(crate) use localized;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_codes_are_stable_and_default_to_english() {
        assert_eq!(Language::default(), Language::English);
        for (code, language) in [("en", Language::English), ("zh", Language::Chinese)] {
            assert_eq!(Language::parse(code).unwrap(), language);
            assert_eq!(language.code(), code);
            assert_eq!(serde_json::to_value(language).unwrap(), code);
        }
        for invalid in ["", "fr", "EN", "zh-CN", "中文", " en"] {
            assert!(Language::parse(invalid).is_err());
        }
    }

    #[test]
    fn language_formatting_preserves_raw_values_and_unicode() {
        let raw = "English 中文 /tmp/Tasks/queue.json";
        assert_eq!(
            localized!(Language::English, "Saved {raw}", "已保存 {raw}"),
            format!("Saved {raw}")
        );
        assert_eq!(
            localized!(Language::Chinese, "Saved {raw}", "已保存 {raw}"),
            format!("已保存 {raw}")
        );
        assert_eq!(
            localized!(Language::Chinese, "{} items", "{} 项", 3),
            "3 项"
        );
        assert_eq!(
            localized!(Language::English, "{n} items", "{n} 项", n = 3),
            "3 items"
        );
    }
}
