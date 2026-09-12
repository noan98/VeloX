# Platform, CI & Release Decisions

OS 固有実装、CI、配布、アプリ構成に関する Decision の入口です。

## 対象

主担当は **D50–D55, D68, D70, D73, D83, D91, D98, D100, D103, D107–D108** の 16 件 (網羅的な一覧は下の「詳細」節。ここは読み進める手がかりとしての要約)。

- **D50–D55** — CI / Windows release / icon / download-context fix / WebProcess sharing / auto-merge
- **D68** — multiple windows
- **D70** — release packaging
- **D73** — コード署名
- **D83, D91, D98, D100, D103, D107–D108** — auto-merge とレビューゲートの運用

## 補助ドキュメント

- [`../windows-code-signing.md`](../windows-code-signing.md)
- GitHub Actions workflow 群

## 詳細

**このカテゴリが主担当の Decision: D50–D55, D68, D70, D73, D83, D91, D98, D100, D103, D107–D108** (16 件)

`archive.md` の全 Decision は、いずれか 1 つのカテゴリが主担当として必ず
この一覧に載る。複数カテゴリにまたがるものは主担当だけに載せ、必要なら
本文から相互参照する (README の「重要なルール」3)。

⚠️ **この節は `generate_decision_index.py` が生成する。手で編集しない。**
`archive.md` との対応は `test_decision_index.py` が CI で検査するので、
追記漏れも、見出しを変えたことによるリンク切れも、そこで落ちる。

- [D50](./archive.md#d50-tabs_n-の-pssrss-サンプル不足-119--サンプリング間隔の自動短縮--サンプル数不足の明示的な警告) — `tabs_N` の PSS/RSS サンプル不足 (#119) — サンプリング間隔の自動短縮 + サンプル数不足の明示的な警告
- [D51](./archive.md#d51-windows-リリースビルドは-github-actions-release-windowsyml-で行う) — Windows リリースビルドは GitHub Actions (`release-windows.yml`) で行う
- [D52](./archive.md#d52-アプリアイコン--exe-リソースは-buildrs-で埋め込みウィンドウアイコンは実行時に設定) — アプリアイコン — `.exe` リソースは `build.rs` で埋め込み、ウィンドウアイコンは実行時に設定
- [D53](./archive.md#d53-ダウンロードハンドラは-webkitgtk-では-webcontext-単位--共有-context-d49-では-toolbar-webview-に-1-回だけ登録する) — ダウンロードハンドラは WebKitGTK では `WebContext` 単位 — 共有 context (D49) では toolbar webview に 1 回だけ登録する
- [D54](./archive.md#d54-タブ間で-webkitwebprocess-を共有-with_related_view--最大-4-タブプロセス読み込み中のプロセスには相乗りしない) — タブ間で `WebKitWebProcess` を共有 (`with_related_view`) — 最大 4 タブ/プロセス、読み込み中のプロセスには相乗りしない
- [D55](./archive.md#d55-pr-の自動マージは自前の-workflow-auto-mergeyml-で行う) — PR の自動マージは自前の workflow (`auto-merge.yml`) で行う
- [D68](./archive.md#d68-複数ウィンドウ対応-29--browserwindowidwindows-を新設tabid-はウィンドウ内でのみ一意という前提のまま-userevent-に-windowid-を明示的に付与する) — 複数ウィンドウ対応 (#29) — `browser::WindowId`/`Windows` を新設、`TabId` はウィンドウ内でのみ一意という前提のまま `UserEvent` に `WindowId` を明示的に付与する
- [D70](./archive.md#d70-リリースパッケージング-41--windows-は-tagversion-整合チェックを追加linux-は最小-tarball-を新設macos-は明示的に見送り) — リリースパッケージング (#41) — Windows は tag/version 整合チェックを追加、Linux は最小 tarball を新設、macOS は明示的に見送り
- [D73](./archive.md#d73-コード署名-42--証明書が無いため有効化可能な仕組みに留め実際の署名は見送り) — コード署名 (#42) — 証明書が無いため「有効化可能な仕組み」に留め、実際の署名は見送り
- [D83](./archive.md#d83-auto-merge-の-closes-キーワード自動クローズ-168--github_token-マージでは-github-標準の自動クローズが効かないためauto-mergeyml-がマージ成功後に自分で-gh-issue-close-する) — auto-merge の Closes キーワード自動クローズ (#168) — GITHUB_TOKEN マージでは GitHub 標準の自動クローズが効かないため、auto-merge.yml がマージ成功後に自分で `gh issue close` する
- [D91](./archive.md#d91-auto-merge-がレビューを見ずにマージしていた問題-188--未解決スレッド--changes_requested--codex-の-head-sha-レビューを必須要件化automerge-without-codex-で-codex-要件のみ免除) — auto-merge がレビューを見ずにマージしていた問題 (#188) — 未解決スレッド / CHANGES_REQUESTED / Codex の head SHA レビューを必須要件化、`automerge-without-codex` で Codex 要件のみ免除
- [D98](./archive.md#d98-claude-フォールバック-issue-194-の失敗原因を見えるようにする--エラー本文が-show_full_output-false-で伏せられていた) — `@claude` フォールバック (Issue #194) の失敗原因を「見えるようにする」 — エラー本文が `show_full_output: false` で伏せられていた
- [D100](./archive.md#d100-auto-merge-の待ち時間を縮める-188194216--claude-のコメントレビューを受理しレビュー返答で即再評価しcodex-の無駄な往復を省く) — auto-merge の待ち時間を縮める (#188/#194/#216) — Claude のコメントレビューを受理し、レビュー返答で即再評価し、Codex の無駄な往復を省く
- [D103](./archive.md#d103-猶予期間の満了はイベントを生まない-issue-219--待つのをやめるのではなくauto-merge-が自分で待ち直す) — 猶予期間の満了はイベントを生まない (Issue #219) — 待つのをやめるのではなく、auto-merge が自分で待ち直す
- [D107](./archive.md#d107-auto-merge-の-workflow_run-監視対象をpr-の-check-run-に現れうる-workflow-全部に揃え列挙漏れをテストで固定する-issue-225) — auto-merge の `workflow_run` 監視対象を「PR の check-run に現れうる workflow 全部」に揃え、列挙漏れをテストで固定する (Issue #225)
- [D108](./archive.md#d108-scripts-と-githubscripts-の-python-テストを-ciyml-で回す-issue-224--テスト-0-件で緑を明示的に失敗させる) — `scripts/` と `.github/scripts/` の Python テストを `ci.yml` で回す (Issue #224) — 「テスト 0 件で緑」を明示的に失敗させる

---

- [設計判断アーカイブ (全文)](./archive.md)
