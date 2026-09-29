//! 何を同期に出すかの方針 (ゲート)。docs/sync.md §2。
//!
//! **プライベートモードは何があっても同期に出さない** — `accepts` の最初の
//! 分岐で `private_mode` を見て、種別の設定より優先する。

use serde::{Deserialize, Serialize};

use super::record::Kind;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncScope {
    pub bookmarks: bool,
    pub settings: bool,
    /// 閲覧履歴は既定でオフ (オプトイン)。URL の羅列はブックマークや設定
    /// より機微で、E2E 暗号化 (#86) が入るまでは出さない。
    pub history: bool,
}

impl Default for SyncScope {
    fn default() -> Self {
        Self {
            bookmarks: true,
            settings: true,
            history: false,
        }
    }
}

impl SyncScope {
    /// この種別のレコードを送受信してよいか (プライベートモード無関係)。
    pub fn kind_enabled(&self, kind: Kind) -> bool {
        match kind {
            Kind::Bookmark | Kind::BookmarkFolder => self.bookmarks,
            Kind::Setting => self.settings,
            Kind::History => self.history,
        }
    }

    /// ローカルの変更を同期キューに載せてよいか。
    pub fn accepts(&self, kind: Kind, private_mode: bool) -> bool {
        if private_mode {
            return false;
        }
        self.kind_enabled(kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_privacy_preserving() {
        let s = SyncScope::default();
        assert!(s.accepts(Kind::Bookmark, false));
        assert!(s.accepts(Kind::BookmarkFolder, false));
        assert!(s.accepts(Kind::Setting, false));
        assert!(!s.accepts(Kind::History, false));
    }

    #[test]
    fn private_mode_never_syncs_even_when_everything_is_enabled() {
        let all = SyncScope {
            bookmarks: true,
            settings: true,
            history: true,
        };
        for k in [
            Kind::Bookmark,
            Kind::BookmarkFolder,
            Kind::Setting,
            Kind::History,
        ] {
            assert!(all.accepts(k, false));
            assert!(!all.accepts(k, true));
        }
    }
}
