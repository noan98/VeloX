# Tabs, Session, History & Omnibox Decisions

ユーザー状態を保持する機能と、タブ・履歴・ブックマーク・omnibox の設計判断をまとめたナビゲーションです。

## 対象

主担当は **D8–D15, D20, D22–D40, D74, D119** の 30 件 (網羅的な一覧は下の「詳細」節。ここは読み進める手がかりとしての要約)。

- **D8–D15** — tabs / suspension / persistence / history UI / private browsing / incognito
- **D20** — explicit tab lifecycle state machine
- **D22–D25** — tab strip / shortcuts / closed tabs / `window.open`
- **D26** — omnibox foundation
- **D27, D29–D31** — history data / grouping / search / persistence
- **D32–D35** — bookmark folders / editing / ordering / bookmark bar
- **D36–D40** — omnibox ranking / de-duplication / query history / private behavior / homepage
- **D74** — private browsing の残りスコープ (Private Window)

## 特に重要な関係

- D8–D9 が WebView lifecycle の土台
- D20 が `TabState` と WebView lifecycle の明示的な状態機械
- D26/D36–D40 が omnibox の入力分類 → 候補 → ranking → query history の流れ
- D10/D31 が persistence 方針の基礎

## 詳細

**このカテゴリが主担当の Decision: D8–D15, D20, D22–D40, D74, D119** (30 件)

`archive.md` の全 Decision は、いずれか 1 つのカテゴリが主担当として必ず
この一覧に載る。複数カテゴリにまたがるものは主担当だけに載せ、必要なら
本文から相互参照する (README の「重要なルール」3)。

⚠️ **この節は `generate_decision_index.py` が生成する。手で編集しない。**
`archive.md` との対応は `test_decision_index.py` が CI で検査するので、
追記漏れも、見出しを変えたことによるリンク切れも、そこで落ちる。

- [D8](./archive.md#d8-multiple-tabs--one-content-webview-per-tab-kept-alive-while-open) — Multiple tabs — one content webview per tab, kept alive while open
- [D9](./archive.md#d9-tab-suspension--drop-the-webview-keep-the-url-no-engine-cache-tuning) — Tab suspension — drop the webview, keep the URL; no engine cache tuning
- [D10](./archive.md#d10-historybookmarks-persistence--json-files-no-new-dependency) — History/bookmarks persistence — JSON files, no new dependency
- [D11](./archive.md#d11-historybookmarks-ui-is-a-panel-in-the-toolbar-webview-not-a-content-webview-page) — History/bookmarks UI is a panel in the toolbar webview, not a content-webview page
- [D12](./archive.md#d12-page-titles-are-fetched-asynchronously-and-applied-best-effort) — Page titles are fetched asynchronously and applied best-effort
- [D13](./archive.md#d13-single-choke-point-for-disabling-history-recording) — Single choke point for disabling history recording
- [D14](./archive.md#d14-private-browsing-7--whole-app-mode-not-a-separate-private-window) — Private browsing (#7) — whole-app mode, not a separate private window
- [D15](./archive.md#d15-wrywebviewbuilderwith_incognito--availability-and-per-platform-backing) — `wry::WebViewBuilder::with_incognito` — availability and per-platform backing
- [D20](./archive.md#d20-explicit-tab-lifecycle-state-machine-and-the-restoring-states-synchronous-collapse) — Explicit tab lifecycle state machine, and the `Restoring` state's synchronous collapse
- [D22](./archive.md#d22-tab-strip--titlefavicon-rendering-favicon-resolution-and-shrink-then-scroll) — Tab strip — title/favicon rendering, favicon resolution, and shrink-then-scroll
- [D23](./archive.md#d23-tab-management-keyboard-shortcuts--same-delivery-pattern-as-d18-two-trust-boundaries) — Tab-management keyboard shortcuts — same delivery pattern as D18, two trust boundaries
- [D24](./archive.md#d24-closed-tab-stack-ctrlcmdshiftt--bounded-lifo-of-urls-in-browsertabs) — Closed-tab stack (Ctrl/Cmd+Shift+T) — bounded LIFO of URLs, in `browser::tabs`
- [D25](./archive.md#d25-target_blankwindowopen--wry-056s-with_new_window_req_handler-confirmed-from-source) — `target="_blank"`/`window.open()` — wry 0.56's `with_new_window_req_handler`, confirmed from source
- [D26](./archive.md#d26-omnibox-foundation-15--url-vs-search-split-default-search-engine-dangerous-input-dropdown-as-a-third-panel) — Omnibox foundation (#15) — URL-vs-search split, default search engine, dangerous input, dropdown as a third `Panel`
- [D27](./archive.md#d27-historyentry-grows-faviconvisit_count--additive-to-d4s-de-duplication-not-a-redesign-of-it) — `HistoryEntry` grows `favicon`/`visit_count` — additive to D4's de-duplication, not a redesign of it
- [D28](./archive.md#d28-downloads-16--wry-056s-startedcompleted-handlers-no-progress-or-mid-transfer-cancel) — Downloads (#16) — wry 0.56's started/completed handlers, no progress or mid-transfer cancel
- [D29](./archive.md#d29-history-date-grouping-今日昨日過去7日それ以前--utc-calendar-days-from-stdtime-no-chrono) — History date grouping (今日/昨日/過去7日/それ以前) — UTC calendar days from `std::time`, no `chrono`
- [D30](./archive.md#d30-history-search--case-insensitive-urltitle-substring-match-pure-function-over-the-full-store) — History search — case-insensitive URL/title substring match, pure function over the full store
- [D31](./archive.md#d31-persistence-stays-json-files--sqlite-considered-and-declined-for-this-issues-scope) — Persistence stays JSON files — SQLite considered and declined for this issue's scope
- [D32](./archive.md#d32-bookmark-folders-19--flat-folder_id-reference-one-layer-no-nesting) — Bookmark folders (#19) — flat `folder_id` reference, one layer, no nesting
- [D33](./archive.md#d33-bookmark-editing--reuses-navigationnormalize_input-rejects-duplicate-urls) — Bookmark editing — reuses `navigation::normalize_input`, rejects duplicate URLs
- [D34](./archive.md#d34-manual-bookmark-reordering--swap-the-vec-no-separate-position-field-favicon-keyed-by-url-captured-at-fetch-time) — Manual bookmark reordering — swap the `Vec`, no separate position field; favicon keyed by URL, captured at fetch time
- [D35](./archive.md#d35-bookmark-bar--a-third-additive-layout-component-not-a-panel-session-only-visibility) — Bookmark bar — a third, additive layout component, not a `Panel`; session-only visibility
- [D36](./archive.md#d36-omnibox-ranking-20--scoring-formula-match-tier--frecency--bookmark-bonus) — Omnibox ranking (#20) — scoring formula: match tier + frecency + bookmark bonus
- [D37](./archive.md#d37-historybookmark-de-duplication--one-entry-per-url-bookmark-wins-the-displayed-kind) — History/bookmark de-duplication — one entry per URL, bookmark wins the displayed kind
- [D38](./archive.md#d38-typed-search-query-history-入力履歴--search-queries-only-lru-capped-persisted-purged-with-clear-history) — Typed search-query history ("入力履歴") — search queries only, LRU-capped, persisted, purged with "clear history"
- [D39](./archive.md#d39-omnibox-candidates-in-private-mode--read-existing-data-record-nothing-new) — Omnibox candidates in private mode — read existing data, record nothing new
- [D40](./archive.md#d40-startup-url-is-overridable---homepage--velox_homepage-default-is-google) — Startup URL is overridable (`--homepage` / `VELOX_HOMEPAGE`), default is Google
- [D74](./archive.md#d74-プライベートブラウジング残りスコープ-27--private-window-を別ウィンドウとして開けるようにする分離は-d14d15-の既存メカニズムのままper-window-化だけを行う) — プライベートブラウジング、残りスコープ (#27) — Private Window を別ウィンドウとして開けるようにする。分離は D14/D15 の既存メカニズムのまま、per-window 化だけを行う
- [D119](./archive.md#d119-タブの優先度は既存の-tabstate-とは別の軸である--5-段階は状態機械ではなく再訪likelihood-の梯子として定義する-issue-176-stage-1) — タブの「優先度」は既存の `TabState` とは別の軸である — 5 段階は状態機械ではなく再訪likelihood の梯子として定義する (Issue #176 Stage 1)

---

- [設計判断アーカイブ (全文)](./archive.md)
