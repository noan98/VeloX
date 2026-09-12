# Security & Maintenance Decisions

セキュリティ、入力値、依存監査、セッション復元、設定など、横断的な保守性に関する Decision の入口です。

## 対象

主担当は **D61–D63, D65, D67, D102, D116** の 7 件 (網羅的な一覧は下の「詳細」節。ここは読み進める手がかりとしての要約)。

- **D61–D63** — CI 実行環境 / input robustness / dependency & security audit
- **D65** — tab session restore
- **D67** — settings architecture
- **D102** — public 化で失効した前提の洗い出し
- **D116** — 設計判断の索引を生成し、CI で検査する

## 詳細

**このカテゴリが主担当の Decision: D61–D63, D65, D67, D102, D116** (7 件)

`archive.md` の全 Decision は、いずれか 1 つのカテゴリが主担当として必ず
この一覧に載る。複数カテゴリにまたがるものは主担当だけに載せ、必要なら
本文から相互参照する (README の「重要なルール」3)。

⚠️ **この節は `generate_decision_index.py` が生成する。手で編集しない。**
`archive.md` との対応は `test_decision_index.py` が CI で検査するので、
追記漏れも、見出しを変えたことによるリンク切れも、そこで落ちる。

- [D61](./archive.md#d61-ci-に-windows-ジョブを追加する--macos-は対象外統合テストは実行しない) — CI に Windows ジョブを追加する — macOS は対象外、統合テストは実行しない
- [D62](./archive.md#d62-セキュリティ入力値堅牢性-35--スキーム許可リストは維持ipc-に) — セキュリティ・入力値堅牢性 (#35) — スキーム許可リストは維持、IPC に
- [D63](./archive.md#d63-依存関係セキュリティ監査を-ci-化-37--cargo-deny-単体を採用しpr-は依存グラフを触った時だけブロッカーにする) — 依存関係・セキュリティ監査を CI 化 (#37) — cargo-deny 単体を採用し、PR は依存グラフを触った時だけブロッカーにする
- [D65](./archive.md#d65-タブセッション復元-25--保存は-sync_tab_strip-に相乗り復元は休止復帰機構をそのまま再利用クラッシュ検知フックは-wry-056-に存在しない) — タブセッション復元 (#25) — 保存は `sync_tab_strip` に相乗り、復元は休止/復帰機構をそのまま再利用、クラッシュ検知フックは wry 0.56 に存在しない
- [D67](./archive.md#d67-設定画面と永続設定基盤-30--browsersettings-に一元化し) — 設定画面と永続設定基盤 (#30) — `browser::settings` に一元化し、
- [D102](./archive.md#d102-リポジトリの-public-化-2026-09-09-で失効した前提を洗い出しコメント起動-workflow-に投稿者の絞り込みを入れる) — リポジトリの public 化 (2026-09-09) で失効した前提を洗い出し、コメント起動 workflow に投稿者の絞り込みを入れる
- [D116](./archive.md#d116-設計判断の索引は手で書かずarchivemd-から生成して-ci-で検査する-issue-227) — 設計判断の索引は手で書かず、`archive.md` から生成して CI で検査する (Issue #227)

---

- [設計判断アーカイブ (全文)](./archive.md)
