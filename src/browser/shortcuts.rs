//! Central keyboard-shortcut definition table (Issue #38, see
//! docs/decisions.md D77).
//!
//! Every keyboard shortcut VeloX wires up used to be defined redundantly in
//! up to half a dozen places at once: a sentinel-string constant and a JS
//! `if`-branch in `ui::window` (the content-webview channel), a
//! `ToolbarCommand` variant and a JS `if`-branch in `ui::toolbar`/
//! `toolbar.html` (the toolbar channel), a dispatch arm in each of
//! `app::handle_content_shortcut`/`handle_toolbar_command`, and a display
//! row in `settings::shortcut_reference`. This module is the one table all
//! of those either read from directly ([`SHORTCUT_TABLE`]) or are generated
//! from ([`ui::window::tab_shortcut_script`]'s content-webview JS,
//! `settings::shortcut_reference`'s display rows): adding a new shortcut to
//! both the content-webview channel and the Settings screen now means
//! adding one row to [`SHORTCUT_TABLE`] below — see this module's tests and
//! docs/decisions.md D77 for exactly what still needs a second, manual edit
//! (the toolbar's own `ToolbarCommand` enum variant and structured-command
//! `keydown` branch, and the `app.rs` dispatch arms — all three inherent to
//! keeping the toolbar's trusted channel a real `enum`, not something a data
//! table can generate away).
//!
//! Everything here is plain, UI/engine-independent Rust data — no `wry`,
//! `tao`, or IPC handling — so it is exercised entirely by unit tests below.
//!
//! **Trust boundary (docs/decisions.md D18/D23)**: this module does not
//! change VeloX's dual-delivery security design at all. It only replaces
//! *duplicated hand-written tables* with *one hand-written table*; the
//! content webview still only ever receives a fixed, Rust-enumerated set of
//! sentinel strings ([`ShortcutId::sentinel`] over [`SHORTCUT_TABLE`], a
//! closed compile-time list), never anything resembling a structured command
//! it could forge. See [`parse_sentinel`]'s doc comment and
//! `ui::window::parse_content_shortcut`, which now delegates to it.

/// Which of the three shipped OS families a shortcut's modifier label should
/// use for display (Issue #38 acceptance criterion: "macOS/Windows/Linuxで
/// 適切なmodifierになる").
///
/// [`Platform::current`] is the only place this reads `cfg(target_os)` —
/// every other function here takes an explicit `Platform`, so the
/// label-formatting logic itself is unit-tested for all three platforms
/// regardless of which one actually runs the test suite (CI runs Linux
/// only — see CLAUDE.md's OS priority policy).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Windows,
    MacOs,
    Linux,
}

impl Platform {
    /// The platform this binary was actually compiled for.
    pub fn current() -> Self {
        #[cfg(target_os = "macos")]
        {
            Platform::MacOs
        }
        #[cfg(target_os = "windows")]
        {
            Platform::Windows
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            Platform::Linux
        }
    }

    /// Label for [`Modifiers::primary`] on this platform: "Cmd" on macOS,
    /// "Ctrl" everywhere else. VeloX's actual key handling always accepts
    /// *both* Ctrl and Cmd on every platform (docs/decisions.md D23) — this
    /// only controls what the Settings screen prints.
    fn primary_label(self) -> &'static str {
        match self {
            Platform::MacOs => "Cmd",
            Platform::Windows | Platform::Linux => "Ctrl",
        }
    }

    /// Label for [`Modifiers::alt`]: macOS spells it "Option" in its own UI
    /// conventions, Windows/Linux "Alt".
    fn alt_label(self) -> &'static str {
        match self {
            Platform::MacOs => "Option",
            Platform::Windows | Platform::Linux => "Alt",
        }
    }
}

/// One physical key, independent of modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    /// A letter key, stored lower-case; [`KeyChord`] handling (and the
    /// generated JS) accepts either case, matching every existing shortcut's
    /// `event.key === "t" || event.key === "T"` pattern.
    Char(char),
    /// A digit-row key, '1'..='9'.
    Digit(u8),
    Tab,
    F12,
}

impl Key {
    fn label(self) -> String {
        match self {
            Key::Char(c) => c.to_ascii_uppercase().to_string(),
            Key::Digit(d) => d.to_string(),
            Key::Tab => "Tab".to_owned(),
            Key::F12 => "F12".to_owned(),
        }
    }

    /// JS boolean expression (against a `keydown` `event`) that matches this
    /// key, accepting either case for a letter — used by
    /// `ui::window::tab_shortcut_script`'s codegen.
    pub fn js_condition(self) -> String {
        match self {
            Key::Char(c) => {
                let lower = c.to_ascii_lowercase();
                let upper = c.to_ascii_uppercase();
                format!("event.key === \"{lower}\" || event.key === \"{upper}\"")
            }
            Key::Digit(d) => format!("event.key === \"{d}\""),
            Key::Tab => "event.key === \"Tab\"".to_owned(),
            Key::F12 => "event.key === \"F12\"".to_owned(),
        }
    }
}

/// A modifier combination. `primary` means "Ctrl on Windows/Linux, Cmd on
/// macOS" for *display* purposes — the actual runtime key handling accepts
/// either modifier on every platform regardless (docs/decisions.md D23), so
/// this struct never branches behavior on `Platform`, only
/// [`KeyChord::label`]'s wording does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Modifiers {
    pub primary: bool,
    pub shift: bool,
    pub alt: bool,
}

impl Modifiers {
    pub const NONE: Self = Self {
        primary: false,
        shift: false,
        alt: false,
    };
    pub const PRIMARY: Self = Self {
        primary: true,
        shift: false,
        alt: false,
    };
    pub const PRIMARY_SHIFT: Self = Self {
        primary: true,
        shift: true,
        alt: false,
    };
    pub const PRIMARY_ALT: Self = Self {
        primary: true,
        shift: false,
        alt: true,
    };

    /// Label for just the modifier part of a chord (e.g. `"Ctrl"`,
    /// `"Cmd+Shift"`), empty when no modifier is held. [`KeyChord::label`]
    /// appends the key itself; `settings::shortcut_reference` also calls
    /// this directly to build the grouped "Ctrl+1〜8" row for
    /// `ActivateTabAt`, which has no single key to append.
    pub fn label(&self, platform: Platform) -> String {
        let mut parts = Vec::with_capacity(3);
        if self.primary {
            parts.push(platform.primary_label().to_owned());
        }
        if self.alt {
            parts.push(platform.alt_label().to_owned());
        }
        if self.shift {
            parts.push("Shift".to_owned());
        }
        parts.join("+")
    }
}

/// A key plus the modifiers held with it — one keyboard shortcut chord.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyChord {
    pub key: Key,
    pub modifiers: Modifiers,
}

impl KeyChord {
    pub const fn new(key: Key, modifiers: Modifiers) -> Self {
        Self { key, modifiers }
    }

    /// Human-readable label for `platform`, e.g. `"Ctrl+T"`, `"Cmd+Shift+B"`.
    /// This is the OS-aware wording the Settings screen's Shortcuts tab now
    /// renders (`settings::shortcut_reference`, computed against
    /// `Platform::current()`), satisfying this issue's "macOS/Windows/Linux
    /// で適切なmodifierになる" acceptance criterion.
    pub fn label(&self, platform: Platform) -> String {
        let modifiers = self.modifiers.label(platform);
        if modifiers.is_empty() {
            self.key.label()
        } else {
            format!("{modifiers}+{}", self.key.label())
        }
    }
}

/// Stable identifier for one bindable action — the "verb" a shortcut
/// triggers, independent of which physical key currently triggers it.
///
/// `Serialize`/`Deserialize` with a fixed `snake_case` wire form: this is
/// the seam a future per-user remapping store (`Settings`, see
/// docs/decisions.md D67) would key a `HashMap<ShortcutId, KeyChord>`
/// override table by, without needing a schema change here — the "将来の
/// ユーザーカスタマイズを考慮した定義形式" acceptance criterion. No such
/// override store exists yet; see docs/decisions.md D77 for why this PR
/// stops at the assignment/collision-detection layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShortcutId {
    NewTab,
    CloseTab,
    ReopenClosedTab,
    NextTab,
    PrevTab,
    /// 1-based display position, 1..=8 — matches
    /// `ui::window::ContentShortcut::ActivateTabAt`.
    ActivateTabAt(u8),
    ActivateLastTab,
    FocusAddressBar,
    ToggleBookmark,
    ToggleBookmarkBar,
    NewWindow,
    OpenFindBar,
    ViewSource,
    OpenDevtools,
}

impl ShortcutId {
    /// The fixed sentinel string the content-webview channel sends for this
    /// action (docs/decisions.md D18/D23) — the single place this format is
    /// defined; `ui::window::parse_content_shortcut` and
    /// `ui::window::tab_shortcut_script`'s codegen both derive from it
    /// instead of each hand-writing their own copy.
    ///
    /// `OpenDevtools` is listed here for completeness ([`SHORTCUT_TABLE`]/
    /// the Settings screen document it alongside every other shortcut, and
    /// [`parse_sentinel`] recognizes its sentinel exactly like any other
    /// row's), but `ui::window::parse_content_shortcut` deliberately maps it
    /// to `None` rather than a `ContentShortcut`-equivalent action:
    /// devtools keeps its own separate, pre-existing delivery mechanism
    /// (`ui::window::OPEN_DEVTOOLS_MESSAGE`/`devtools_shortcut_script`,
    /// gated by the `devtools` feature/`debug_assertions` per D18) rather
    /// than being folded into the tab-management shortcuts' shared codegen —
    /// see that function's own doc comment.
    pub fn sentinel(&self) -> String {
        match self {
            ShortcutId::NewTab => "velox:new-tab".to_owned(),
            ShortcutId::CloseTab => "velox:close-tab".to_owned(),
            ShortcutId::ReopenClosedTab => "velox:reopen-closed-tab".to_owned(),
            ShortcutId::NextTab => "velox:next-tab".to_owned(),
            ShortcutId::PrevTab => "velox:prev-tab".to_owned(),
            ShortcutId::ActivateTabAt(n) => format!("velox:activate-tab-{n}"),
            ShortcutId::ActivateLastTab => "velox:activate-tab-last".to_owned(),
            ShortcutId::FocusAddressBar => "velox:focus-address-bar".to_owned(),
            ShortcutId::ToggleBookmark => "velox:toggle-bookmark".to_owned(),
            ShortcutId::ToggleBookmarkBar => "velox:toggle-bookmark-bar".to_owned(),
            ShortcutId::NewWindow => "velox:new-window".to_owned(),
            ShortcutId::OpenFindBar => "velox:open-find-bar".to_owned(),
            ShortcutId::ViewSource => "velox:view-source".to_owned(),
            ShortcutId::OpenDevtools => "velox:open-devtools".to_owned(),
        }
    }
}

/// One row of [`SHORTCUT_TABLE`]: an action, its Settings-screen label, and
/// its default chord(s). Most actions bind exactly one chord;
/// [`ShortcutId::OpenDevtools`] binds two (F12, and macOS's Cmd+Option+I —
/// see its own doc comment on why that second chord is deliberately *not*
/// modeled as [`Modifiers::primary`]-accepts-either-modifier the way every
/// other row is: `ui::window::devtools_shortcut_script` has only ever
/// checked `event.metaKey`, never `event.ctrlKey`, for that combination, and
/// this table does not change that existing, already-shipped behavior).
#[derive(Debug, Clone, Copy)]
pub struct ShortcutDef {
    pub id: ShortcutId,
    /// Japanese display label for the Settings screen's Shortcuts tab.
    pub label: &'static str,
    pub chords: &'static [KeyChord],
}

/// The one table every keyboard shortcut VeloX wires up is defined in. See
/// the module doc comment for what reads this table directly vs. what still
/// needs its own, separate edit (a `ToolbarCommand` variant and dispatch
/// arm) when a *new* shortcut is added — reducing that second category to
/// as little as possible is this issue's explicit goal (see the PR), since
/// three other in-flight issues (#27/#40/#46) are each about to add one.
///
/// Order matches the Settings screen's Shortcuts tab (unchanged from before
/// this issue) with two additions this table's tests caught missing from
/// the old hand-maintained `shortcut_reference` list: `NewWindow` (Issue
/// #29) and `OpenFindBar` (Issue #43) were both wired up in `ui::window`/
/// `ui::toolbar` at the time but never added to the Settings screen's
/// reference table — a small, real instance of exactly the drift this
/// consolidation is meant to prevent.
pub const SHORTCUT_TABLE: &[ShortcutDef] = &[
    ShortcutDef {
        id: ShortcutId::NewTab,
        label: "新しいタブ",
        chords: &[KeyChord::new(Key::Char('t'), Modifiers::PRIMARY)],
    },
    ShortcutDef {
        id: ShortcutId::CloseTab,
        label: "タブを閉じる",
        chords: &[KeyChord::new(Key::Char('w'), Modifiers::PRIMARY)],
    },
    ShortcutDef {
        id: ShortcutId::ReopenClosedTab,
        label: "閉じたタブを再度開く",
        chords: &[KeyChord::new(Key::Char('t'), Modifiers::PRIMARY_SHIFT)],
    },
    ShortcutDef {
        id: ShortcutId::NextTab,
        label: "次のタブ",
        chords: &[KeyChord::new(Key::Tab, Modifiers::PRIMARY)],
    },
    ShortcutDef {
        id: ShortcutId::PrevTab,
        label: "前のタブ",
        chords: &[KeyChord::new(Key::Tab, Modifiers::PRIMARY_SHIFT)],
    },
    ShortcutDef {
        id: ShortcutId::ActivateTabAt(1),
        label: "1番目のタブに切り替え",
        chords: &[KeyChord::new(Key::Digit(1), Modifiers::PRIMARY)],
    },
    ShortcutDef {
        id: ShortcutId::ActivateTabAt(2),
        label: "2番目のタブに切り替え",
        chords: &[KeyChord::new(Key::Digit(2), Modifiers::PRIMARY)],
    },
    ShortcutDef {
        id: ShortcutId::ActivateTabAt(3),
        label: "3番目のタブに切り替え",
        chords: &[KeyChord::new(Key::Digit(3), Modifiers::PRIMARY)],
    },
    ShortcutDef {
        id: ShortcutId::ActivateTabAt(4),
        label: "4番目のタブに切り替え",
        chords: &[KeyChord::new(Key::Digit(4), Modifiers::PRIMARY)],
    },
    ShortcutDef {
        id: ShortcutId::ActivateTabAt(5),
        label: "5番目のタブに切り替え",
        chords: &[KeyChord::new(Key::Digit(5), Modifiers::PRIMARY)],
    },
    ShortcutDef {
        id: ShortcutId::ActivateTabAt(6),
        label: "6番目のタブに切り替え",
        chords: &[KeyChord::new(Key::Digit(6), Modifiers::PRIMARY)],
    },
    ShortcutDef {
        id: ShortcutId::ActivateTabAt(7),
        label: "7番目のタブに切り替え",
        chords: &[KeyChord::new(Key::Digit(7), Modifiers::PRIMARY)],
    },
    ShortcutDef {
        id: ShortcutId::ActivateTabAt(8),
        label: "8番目のタブに切り替え",
        chords: &[KeyChord::new(Key::Digit(8), Modifiers::PRIMARY)],
    },
    ShortcutDef {
        id: ShortcutId::ActivateLastTab,
        label: "最後のタブに切り替え",
        chords: &[KeyChord::new(Key::Digit(9), Modifiers::PRIMARY)],
    },
    ShortcutDef {
        id: ShortcutId::FocusAddressBar,
        label: "アドレスバーにフォーカス",
        chords: &[KeyChord::new(Key::Char('l'), Modifiers::PRIMARY)],
    },
    ShortcutDef {
        id: ShortcutId::ToggleBookmark,
        label: "ブックマークの追加/削除",
        chords: &[KeyChord::new(Key::Char('d'), Modifiers::PRIMARY)],
    },
    ShortcutDef {
        id: ShortcutId::ToggleBookmarkBar,
        label: "ブックマークバーの表示切替",
        chords: &[KeyChord::new(Key::Char('b'), Modifiers::PRIMARY_SHIFT)],
    },
    ShortcutDef {
        id: ShortcutId::NewWindow,
        label: "新しいウィンドウ",
        chords: &[KeyChord::new(Key::Char('n'), Modifiers::PRIMARY)],
    },
    ShortcutDef {
        id: ShortcutId::OpenFindBar,
        label: "ページ内検索を開く",
        chords: &[KeyChord::new(Key::Char('f'), Modifiers::PRIMARY)],
    },
    ShortcutDef {
        id: ShortcutId::ViewSource,
        label: "ページのソースを表示",
        chords: &[KeyChord::new(Key::Char('u'), Modifiers::PRIMARY)],
    },
    ShortcutDef {
        id: ShortcutId::OpenDevtools,
        label: "DevTools を開く",
        chords: &[
            KeyChord::new(Key::F12, Modifiers::NONE),
            KeyChord::new(Key::Char('i'), Modifiers::PRIMARY_ALT),
        ],
    },
];

/// Parse a shortcut sentinel string into the [`ShortcutId`] it names —
/// `None` for anything that is not an exact match against
/// [`SHORTCUT_TABLE`]'s fixed, compile-time-enumerated sentinel set (via
/// [`ShortcutId::sentinel`]). Every table entry's sentinel round-trips here,
/// `OpenDevtools`'s included — this function does not itself decide which
/// channel a given action is allowed to arrive over, only whether a string
/// names *some* known action at all.
///
/// **This is the trust-boundary function content-webview delivery is built
/// on** (docs/decisions.md D18/D23): `ui::window::parse_content_shortcut`
/// calls this rather than re-implementing its own copy of the sentinel
/// table, against `body` — raw text posted by the content webview's
/// `window.ipc`, which renders arbitrary, potentially hostile web pages.
/// Nothing here ever deserializes `body` as JSON or otherwise treats it as
/// structured data, only ever compares it for exact equality against one of
/// the finitely many strings [`SHORTCUT_TABLE`] enumerates at compile time.
/// A page that posts a command name not in this closed set (including a
/// well-formed `ToolbarCommand`-shaped JSON payload) gets `None`, exactly
/// like every other unrecognized string — see this module's
/// `rejects_unknown_content_shortcut_strings` test. `ui::window::
/// parse_content_shortcut` then additionally maps `Some(ShortcutId::
/// OpenDevtools)` to `None` too, since that action is never delivered
/// through the content-webview channel this function backs — see
/// [`ShortcutId::sentinel`]'s doc comment.
pub fn parse_sentinel(body: &str) -> Option<ShortcutId> {
    SHORTCUT_TABLE
        .iter()
        .map(|def| def.id)
        .find(|id| id.sentinel() == body)
}

/// One conflict found by [`find_conflicts`]: two or more actions bound to
/// the exact same [`KeyChord`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShortcutConflict {
    pub chord: KeyChord,
    pub actions: Vec<ShortcutId>,
}

/// Detect keyboard-shortcut collisions: any [`KeyChord`] bound by two or
/// more distinct [`ShortcutId`]s among `defs`'s chords, expanding every
/// row's `chords` slice first (so `OpenDevtools`'s two chords are each
/// checked independently). Returns one [`ShortcutConflict`] per colliding
/// chord, each listing every action that claims it — empty when every
/// shortcut in `defs` is bound to a unique chord.
///
/// Pure and order-independent (grouping, not scanning position), so it
/// works the same whether `defs` is [`SHORTCUT_TABLE`] itself (this issue's
/// "キー衝突を検出できる" acceptance criterion, exercised by
/// `default_table_has_no_conflicts` below as a standing regression guard)
/// or a hypothetical future user-customized table (once a remapping UI
/// exists — see the module doc comment).
pub fn find_conflicts(defs: &[ShortcutDef]) -> Vec<ShortcutConflict> {
    let mut by_chord: Vec<(KeyChord, Vec<ShortcutId>)> = Vec::new();
    for def in defs {
        for &chord in def.chords {
            match by_chord.iter_mut().find(|(c, _)| *c == chord) {
                Some((_, actions)) => {
                    if !actions.contains(&def.id) {
                        actions.push(def.id);
                    }
                }
                None => by_chord.push((chord, vec![def.id])),
            }
        }
    }
    by_chord
        .into_iter()
        .filter(|(_, actions)| actions.len() > 1)
        .map(|(chord, actions)| ShortcutConflict { chord, actions })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Platform-aware labels (acceptance criterion: macOS/Windows/Linux
    // で適切なmodifierになる) ---

    #[test]
    fn primary_modifier_label_differs_only_on_macos() {
        let chord = KeyChord::new(Key::Char('t'), Modifiers::PRIMARY);
        assert_eq!(chord.label(Platform::Windows), "Ctrl+T");
        assert_eq!(chord.label(Platform::Linux), "Ctrl+T");
        assert_eq!(chord.label(Platform::MacOs), "Cmd+T");
    }

    #[test]
    fn shift_and_alt_modifiers_are_labeled_per_platform() {
        let shift = KeyChord::new(Key::Char('b'), Modifiers::PRIMARY_SHIFT);
        assert_eq!(shift.label(Platform::Windows), "Ctrl+Shift+B");
        assert_eq!(shift.label(Platform::MacOs), "Cmd+Shift+B");

        let alt = KeyChord::new(Key::Char('i'), Modifiers::PRIMARY_ALT);
        assert_eq!(alt.label(Platform::Windows), "Ctrl+Alt+I");
        assert_eq!(alt.label(Platform::MacOs), "Cmd+Option+I");
    }

    #[test]
    fn digit_and_named_keys_label_without_case_changes() {
        let digit = KeyChord::new(Key::Digit(3), Modifiers::PRIMARY);
        assert_eq!(digit.label(Platform::Linux), "Ctrl+3");

        let tab = KeyChord::new(Key::Tab, Modifiers::PRIMARY_SHIFT);
        assert_eq!(tab.label(Platform::Linux), "Ctrl+Shift+Tab");

        let f12 = KeyChord::new(Key::F12, Modifiers::NONE);
        assert_eq!(f12.label(Platform::Linux), "F12");
    }

    #[test]
    fn platform_current_returns_the_platform_this_binary_was_built_for() {
        // Loose: only asserts it returns *something* consistent with the
        // `cfg` this test itself was compiled under, without hardcoding
        // Linux — mirrors how VeloX only actually runs CI on Linux
        // (CLAUDE.md's OS priority policy) but this logic is not
        // Linux-specific.
        let current = Platform::current();
        #[cfg(target_os = "macos")]
        assert_eq!(current, Platform::MacOs);
        #[cfg(target_os = "windows")]
        assert_eq!(current, Platform::Windows);
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        assert_eq!(current, Platform::Linux);
    }

    // --- Sentinel round-trip (docs/decisions.md D18/D23) ---

    #[test]
    fn every_table_sentinel_parses_back_to_its_own_id() {
        // `parse_sentinel` itself makes no exception for `OpenDevtools` —
        // rejecting it is `ui::window::parse_content_shortcut`'s job, one
        // layer up (see that function's doc comment), not this general
        // string-to-id resolver's.
        for def in SHORTCUT_TABLE {
            let sentinel = def.id.sentinel();
            assert_eq!(parse_sentinel(&sentinel), Some(def.id));
        }
    }

    #[test]
    fn activate_tab_sentinels_are_distinct_from_activate_last_tab() {
        assert_eq!(
            parse_sentinel("velox:activate-tab-9"),
            None,
            "9 is reserved for activate-tab-last, not a literal position"
        );
        assert_eq!(
            parse_sentinel("velox:activate-tab-last"),
            Some(ShortcutId::ActivateLastTab)
        );
        for n in 1u8..=8 {
            assert_eq!(
                parse_sentinel(&format!("velox:activate-tab-{n}")),
                Some(ShortcutId::ActivateTabAt(n))
            );
        }
    }

    /// The exact scenario CLAUDE.md's task called out by name: a malicious
    /// or merely buggy content webview posting a string that is *not* one
    /// of the enumerated sentinels — including something that looks like a
    /// plausible new command, JSON, or a near-miss of a real sentinel —
    /// must always be rejected, never partially matched or coerced into an
    /// action. This is what keeps content webview shortcut delivery from
    /// ever growing into a second `ToolbarCommand`-style structured-command
    /// channel (docs/decisions.md D18/D23). `"velox:open-devtools"` is
    /// intentionally *not* in this list — `parse_sentinel` correctly
    /// recognizes it as `Some(ShortcutId::OpenDevtools)` (it is a real,
    /// enumerated sentinel); `ui::window::parse_content_shortcut`'s own
    /// test (`parse_content_shortcut_matches_every_sentinel_exactly`) is
    /// where that string's actual rejection — one layer up, action-specific
    /// rather than string-specific — is verified.
    #[test]
    fn rejects_unknown_content_shortcut_strings() {
        for body in [
            "",
            "velox:self-destruct",
            "velox:navigate",
            r#"{"cmd":"new_tab"}"#,
            r#"{"cmd":"update_settings","settings":{}}"#,
            "velox:new-tab ",
            "VELOX:NEW-TAB",
            "velox:new-tab\0",
            "velox:activate-tab-0",
            "velox:activate-tab-9",
            "velox:activate-tab-99",
            "velox:activate-tab-",
            "not a sentinel at all",
        ] {
            assert_eq!(parse_sentinel(body), None, "body was {body:?}");
        }
    }

    #[test]
    fn does_not_panic_on_pathological_content_shortcut_input() {
        let huge = "a".repeat(5_000_000);
        assert_eq!(parse_sentinel(&huge), None);
        for hostile in ["\0\0\0", "🚀日本語", "\u{202e}velox:new-tab"] {
            assert_eq!(parse_sentinel(hostile), None, "{hostile:?}");
        }
    }

    // --- Collision detection (acceptance criterion: キー衝突を検出できる) ---

    #[test]
    fn default_table_has_no_conflicts() {
        // Regression guard: every shortcut VeloX ships today must remain on
        // a unique chord. A future PR (including #27/#40/#46's Ctrl/Cmd+
        // Shift+N / +P / +S additions) that accidentally reuses an existing
        // chord fails this test rather than silently shadowing a shortcut.
        assert_eq!(find_conflicts(SHORTCUT_TABLE), Vec::new());
    }

    #[test]
    fn detects_two_actions_bound_to_the_same_chord() {
        const CHORD_P: [KeyChord; 1] = [KeyChord::new(Key::Char('p'), Modifiers::PRIMARY)];
        let chord = KeyChord::new(Key::Char('p'), Modifiers::PRIMARY);
        let defs = [
            ShortcutDef {
                id: ShortcutId::NewWindow,
                label: "新しいウィンドウ",
                chords: &CHORD_P,
            },
            ShortcutDef {
                id: ShortcutId::ViewSource,
                label: "ページのソースを表示",
                chords: &CHORD_P,
            },
        ];
        let conflicts = find_conflicts(&defs);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].chord, chord);
        let mut actions = conflicts[0].actions.clone();
        actions.sort_by_key(|id| format!("{id:?}"));
        let mut expected = vec![ShortcutId::NewWindow, ShortcutId::ViewSource];
        expected.sort_by_key(|id| format!("{id:?}"));
        assert_eq!(actions, expected);
    }

    #[test]
    fn no_conflict_when_every_chord_is_unique() {
        const CHORD_T: [KeyChord; 1] = [KeyChord::new(Key::Char('t'), Modifiers::PRIMARY)];
        const CHORD_W: [KeyChord; 1] = [KeyChord::new(Key::Char('w'), Modifiers::PRIMARY)];
        let defs = [
            ShortcutDef {
                id: ShortcutId::NewTab,
                label: "新しいタブ",
                chords: &CHORD_T,
            },
            ShortcutDef {
                id: ShortcutId::CloseTab,
                label: "タブを閉じる",
                chords: &CHORD_W,
            },
        ];
        assert_eq!(find_conflicts(&defs), Vec::new());
    }

    #[test]
    fn a_multi_chord_action_does_not_conflict_with_itself() {
        // `OpenDevtools` legitimately binds two chords (F12 and Cmd+Option+I)
        // for the *same* action — that must never be reported as a
        // self-conflict.
        const DEVTOOLS_CHORDS: [KeyChord; 2] = [
            KeyChord::new(Key::F12, Modifiers::NONE),
            KeyChord::new(Key::Char('i'), Modifiers::PRIMARY_ALT),
        ];
        let conflicts = find_conflicts(&[ShortcutDef {
            id: ShortcutId::OpenDevtools,
            label: "DevTools を開く",
            chords: &DEVTOOLS_CHORDS,
        }]);
        assert_eq!(conflicts, Vec::new());
    }

    // --- Table integrity ---

    #[test]
    fn table_ids_are_unique() {
        let mut ids: Vec<String> = SHORTCUT_TABLE
            .iter()
            .map(|d| format!("{:?}", d.id))
            .collect();
        let before = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), before, "duplicate ShortcutId in SHORTCUT_TABLE");
    }

    #[test]
    fn table_labels_are_unique_and_non_empty() {
        let mut labels: Vec<&str> = SHORTCUT_TABLE.iter().map(|d| d.label).collect();
        for label in &labels {
            assert!(!label.trim().is_empty());
        }
        let before = labels.len();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), before, "duplicate label in SHORTCUT_TABLE");
    }

    #[test]
    fn table_covers_every_documented_activate_tab_position() {
        for n in 1u8..=8 {
            assert!(
                SHORTCUT_TABLE
                    .iter()
                    .any(|d| d.id == ShortcutId::ActivateTabAt(n)),
                "missing ActivateTabAt({n})"
            );
        }
    }

    // --- Serde wire form (future customization seam) ---

    #[test]
    fn shortcut_id_serializes_to_stable_snake_case_tags() {
        assert_eq!(
            serde_json::to_string(&ShortcutId::NewTab).unwrap(),
            "\"new_tab\""
        );
        assert_eq!(
            serde_json::to_string(&ShortcutId::ToggleBookmarkBar).unwrap(),
            "\"toggle_bookmark_bar\""
        );
        assert_eq!(
            serde_json::to_string(&ShortcutId::ActivateTabAt(3)).unwrap(),
            r#"{"activate_tab_at":3}"#
        );
    }

    #[test]
    fn shortcut_id_round_trips_through_json() {
        for def in SHORTCUT_TABLE {
            let json = serde_json::to_string(&def.id).unwrap();
            let back: ShortcutId = serde_json::from_str(&json).unwrap();
            assert_eq!(back, def.id);
        }
    }
}
