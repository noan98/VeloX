//! 拡張機能のライフサイクル状態機械とイベント (Issue #84、docs/extensions.md §5)。
//!
//! 純粋ロジック (IO なし)。永続する状態は `Enabled` / `Disabled(理由)` の 2 系統だけ。
//! イベントページの休止 (`Suspended`) は実行時の状態で、背景実行を配線する
//! 段階で足す (未配線、D163)。`Uninstalled` は状態ではなくレジストリからの
//! 削除で表す。
//!
//! ```text
//!  install ─▶ Enabled ⇄ Disabled(User)
//!                │            ▲
//!    update で   │            │ enable
//!    権限が増加  ▼            │
//!        Disabled(NeedsReconsent) ──approve──▶ Enabled
//!    破損検出 ──▶ Disabled(Corrupt) ──(更新で修復)──▶ Disabled(User)
//! ```

use serde::{Deserialize, Serialize};

/// 無効の理由。理由によって「再有効化に何が必要か」が違う。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisabledReason {
    /// ユーザが無効化した。`enable` だけで戻せる。
    User,
    /// 更新で権限・ホストが承認済みを超えた (または承認記録を失った)。
    /// `approve` (再承認) が必要で、`enable` だけでは戻せない。
    NeedsReconsent,
    /// パッケージ・マニフェストの破損で隔離された。更新 (再インストール) で修復する。
    Corrupt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionState {
    Enabled,
    Disabled(DisabledReason),
}

impl ExtensionState {
    pub fn is_enabled(self) -> bool {
        self == ExtensionState::Enabled
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleError {
    /// 再承認が必要。`approve` を先に呼ぶ。
    ConsentRequired,
    /// 隔離中。更新 (再インストール) で修復するまで有効化できない。
    Quarantined,
}

impl std::fmt::Display for LifecycleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LifecycleError::ConsentRequired => write!(f, "権限の再承認が必要"),
            LifecycleError::Quarantined => {
                write!(f, "破損のため隔離されている (再インストールが必要)")
            }
        }
    }
}

impl std::error::Error for LifecycleError {}

/// 拡張機能ごとに発生するライフサイクルイベント。背景実行を配線する段階で
/// `runtime.onInstalled` 等として配送する (今は `drain_events` で取り出す)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleEvent {
    Installed,
    Updated {
        from: String,
        to: String,
    },
    Enabled,
    Disabled(DisabledReason),
    /// 更新で承認を超える権限が要求された (この後 `Disabled(NeedsReconsent)` になる)。
    ConsentRequired {
        added_permissions: Vec<String>,
        added_hosts: Vec<String>,
    },
    /// 破損を検出して隔離した。
    Quarantined {
        detail: String,
    },
    Uninstalled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleRecord {
    pub extension_id: String,
    pub event: LifecycleEvent,
}

pub fn enable(state: ExtensionState) -> Result<ExtensionState, LifecycleError> {
    match state {
        ExtensionState::Enabled | ExtensionState::Disabled(DisabledReason::User) => {
            Ok(ExtensionState::Enabled)
        }
        ExtensionState::Disabled(DisabledReason::NeedsReconsent) => {
            Err(LifecycleError::ConsentRequired)
        }
        ExtensionState::Disabled(DisabledReason::Corrupt) => Err(LifecycleError::Quarantined),
    }
}

/// 無効化。すでに別の理由で無効ならその理由を保つ (`NeedsReconsent` を
/// `User` に上書きして、`enable` だけで戻れるようにしてしまわない)。
pub fn disable(state: ExtensionState) -> ExtensionState {
    match state {
        ExtensionState::Enabled => ExtensionState::Disabled(DisabledReason::User),
        other => other,
    }
}

/// 再承認。`NeedsReconsent` だけが有効に戻る。ユーザ無効化中は無効のまま。
pub fn approve(state: ExtensionState) -> Result<ExtensionState, LifecycleError> {
    match state {
        ExtensionState::Disabled(DisabledReason::NeedsReconsent) => Ok(ExtensionState::Enabled),
        ExtensionState::Disabled(DisabledReason::Corrupt) => Err(LifecycleError::Quarantined),
        other => Ok(other),
    }
}

/// 更新適用後の状態。承認を超える更新は必ず `NeedsReconsent`。超えない更新は
/// 状態を保つが、隔離中の拡張機能は修復されたので `User` 無効に戻す
/// (自動では有効化しない)。
pub fn after_update(state: ExtensionState, exceeds_approval: bool) -> ExtensionState {
    if exceeds_approval {
        return ExtensionState::Disabled(DisabledReason::NeedsReconsent);
    }
    match state {
        ExtensionState::Disabled(DisabledReason::Corrupt) => {
            ExtensionState::Disabled(DisabledReason::User)
        }
        other => other,
    }
}

/// 状態が変わったときに発行するイベント。
pub fn state_change_event(prev: ExtensionState, next: ExtensionState) -> Option<LifecycleEvent> {
    if prev == next {
        return None;
    }
    Some(match next {
        ExtensionState::Enabled => LifecycleEvent::Enabled,
        ExtensionState::Disabled(r) => LifecycleEvent::Disabled(r),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use DisabledReason::*;
    use ExtensionState::*;

    #[test]
    fn enable_disable_cycle() {
        assert_eq!(disable(Enabled), Disabled(User));
        assert_eq!(enable(Disabled(User)), Ok(Enabled));
        assert_eq!(enable(Enabled), Ok(Enabled));
    }

    #[test]
    fn enable_cannot_bypass_reconsent_or_quarantine() {
        assert_eq!(
            enable(Disabled(NeedsReconsent)),
            Err(LifecycleError::ConsentRequired)
        );
        assert_eq!(enable(Disabled(Corrupt)), Err(LifecycleError::Quarantined));
    }

    #[test]
    fn disable_keeps_the_stronger_reason() {
        assert_eq!(disable(Disabled(NeedsReconsent)), Disabled(NeedsReconsent));
        assert_eq!(disable(Disabled(Corrupt)), Disabled(Corrupt));
    }

    #[test]
    fn approve_only_lifts_needs_reconsent() {
        assert_eq!(approve(Disabled(NeedsReconsent)), Ok(Enabled));
        assert_eq!(approve(Disabled(User)), Ok(Disabled(User)));
        assert_eq!(approve(Enabled), Ok(Enabled));
        assert_eq!(approve(Disabled(Corrupt)), Err(LifecycleError::Quarantined));
    }

    #[test]
    fn update_that_exceeds_approval_always_disables() {
        assert_eq!(after_update(Enabled, true), Disabled(NeedsReconsent));
        assert_eq!(after_update(Disabled(User), true), Disabled(NeedsReconsent));
        assert_eq!(after_update(Enabled, false), Enabled);
        assert_eq!(after_update(Disabled(User), false), Disabled(User));
        // 修復更新は自動では有効化しない。
        assert_eq!(after_update(Disabled(Corrupt), false), Disabled(User));
    }

    #[test]
    fn state_change_events() {
        assert_eq!(state_change_event(Enabled, Enabled), None);
        assert_eq!(
            state_change_event(Enabled, Disabled(User)),
            Some(LifecycleEvent::Disabled(User))
        );
        assert_eq!(
            state_change_event(Disabled(User), Enabled),
            Some(LifecycleEvent::Enabled)
        );
    }
}
