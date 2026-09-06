//! Opaque, stable identifier for a browser window (Issue #29).
//!
//! Mirrors [`super::tab::TabId`]'s shape exactly: a plain `u64` newtype,
//! assigned once by [`super::windows::Windows`] when a window is opened and
//! never reused, so a stale id (e.g. an event racing a window close) simply
//! refers to nothing rather than to the wrong window. See
//! docs/decisions.md D68 for why `WindowId` exists as its own type instead of
//! reusing `TabId`'s numeric space: unlike `TabId` (unique only within one
//! window's own `Tabs`), a `TabId` can legitimately repeat across two
//! different windows, so every per-tab event that crosses the window
//! boundary must carry a `WindowId` alongside it to disambiguate — see
//! `crate::app::UserEvent`.

/// Opaque, stable identifier for a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WindowId(u64);

impl WindowId {
    /// The underlying numeric id.
    pub fn get(self) -> u64 {
        self.0
    }
}

impl From<u64> for WindowId {
    fn from(id: u64) -> Self {
        WindowId(id)
    }
}

impl std::fmt::Display for WindowId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
