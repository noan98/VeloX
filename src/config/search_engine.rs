//! Omnibox の検索エンジン定義と組み込みプリセット (docs/decisions.md D26)。

/// One selectable search engine: a display name plus the query template URL
/// the omnibox builds a search request from (see
/// `browser::navigation::build_search_url`). `query_template` must contain
/// the literal placeholder `{}`, replaced with the percent-encoded query
/// text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchEngine {
    pub name: String,
    pub query_template: String,
}

/// 組み込みプリセット 1 つを組み立てる関数 ([`SearchEngine::duckduckgo`] など)。
type PresetBuilder = fn() -> SearchEngine;

/// 組み込みプリセットの一覧: `settings.json` の `engine_preset` に保存される
/// キー名と、そのプリセットを組み立てる関数の対応表。名前からの引き当て
/// ([`SearchEngine::preset`]) と値からの逆引き ([`SearchEngine::preset_key`])
/// の両方がこの 1 つの表を見るので、プリセットを足すときに片方だけ
/// 更新し忘れることがない。
const PRESETS: [(&str, PresetBuilder); 5] = [
    ("duckduckgo", SearchEngine::duckduckgo),
    ("google", SearchEngine::google),
    ("bing", SearchEngine::bing),
    ("startpage", SearchEngine::startpage),
    ("ecosia", SearchEngine::ecosia),
];

impl SearchEngine {
    pub(super) fn new(name: &str, query_template: &str) -> Self {
        Self {
            name: name.to_owned(),
            query_template: query_template.to_owned(),
        }
    }

    /// VeloX's default — see docs/decisions.md D26 for why DuckDuckGo was
    /// chosen over Google/Bing/etc.
    pub fn duckduckgo() -> Self {
        Self::new("DuckDuckGo", "https://duckduckgo.com/?q={}")
    }

    pub fn google() -> Self {
        Self::new("Google", "https://www.google.com/search?q={}")
    }

    pub fn bing() -> Self {
        Self::new("Bing", "https://www.bing.com/search?q={}")
    }

    pub fn startpage() -> Self {
        Self::new("Startpage", "https://www.startpage.com/sp/search?query={}")
    }

    pub fn ecosia() -> Self {
        Self::new("Ecosia", "https://www.ecosia.org/search?q={}")
    }

    /// Look up one of the built-in presets by name (case-insensitive; a
    /// couple of common short aliases are accepted alongside the full
    /// name). `None` for anything unrecognized.
    pub(super) fn preset(name: &str) -> Option<Self> {
        let key = name.trim().to_ascii_lowercase();
        let key = match key.as_str() {
            "ddg" => "duckduckgo",
            other => other,
        };
        PRESETS
            .iter()
            .find(|(preset_key, _)| *preset_key == key)
            .map(|(_, build)| build())
    }

    /// `self` と完全一致する組み込みプリセットのキー名。カスタムエンジン
    /// (どのプリセットとも一致しないもの) は `None`。
    pub(super) fn preset_key(&self) -> Option<&'static str> {
        PRESETS
            .iter()
            .find(|(_, build)| build() == *self)
            .map(|(preset_key, _)| *preset_key)
    }
}

impl Default for SearchEngine {
    fn default() -> Self {
        Self::duckduckgo()
    }
}
