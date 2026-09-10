# Tabs, Session, History & Omnibox Decisions

ユーザー状態を保持する機能と、タブ・履歴・ブックマーク・omnibox の設計判断をまとめたナビゲーションです。

## 対象

- **D8–D15** — tabs / suspension / persistence / history UI / private browsing / incognito
- **D20** — explicit tab lifecycle state machine
- **D22–D25** — tab strip / shortcuts / closed tabs / `window.open`
- **D26** — omnibox foundation
- **D27, D29–D31** — history data / grouping / search / persistence
- **D32–D35** — bookmark folders / editing / ordering / bookmark bar
- **D36–D40** — omnibox ranking / de-duplication / query history / private behavior / homepage

## 特に重要な関係

- D8–D9 が WebView lifecycle の土台
- D20 が `TabState` と WebView lifecycle の明示的な状態機械
- D26/D36–D40 が omnibox の入力分類 → 候補 → ranking → query history の流れ
- D10/D31 が persistence 方針の基礎

## 詳細

- [D8–D15](./archive.md#d8-multiple-tabs--one-content-webview-per-tab-kept-alive-while-open)
- [D20](./archive.md#d20-explicit-tab-lifecycle-state-machine-and-the-restoring-states-synchronous-collapse)
- [D22–D40](./archive.md#d22-tab-strip--titlefavicon-rendering-favicon-resolution-and-shrink-then-scroll)
- [設計判断アーカイブ](./archive.md)
