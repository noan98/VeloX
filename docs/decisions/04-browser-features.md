# Browser Feature Decisions

ブラウザ機能そのものに関する設計判断の入口です。

## 対象

主担当は **D17–D18, D59–D60, D64, D66, D69, D71–D72, D75–D78** の 13 件 (網羅的な一覧は下の「詳細」節。ここは読み進める手がかりとしての要約)。

- **D17–D18** — content blocking / DevTools / IPC boundary
- **D23–D28** — keyboard shortcuts / closed tabs / new-window handling / omnibox foundation / history favicon / downloads
- **D59–D60** — subresource blocking / site permissions
- **D64** — EasyList/EasyPrivacy support
- **D66** — site data management
- **D69** — in-page find
- **D71–D72, D75–D78** — ダークモード / View Source / 印刷・PDF / 名前を付けて保存 / ショートカット管理 / コンテキストメニュー

## 詳細

**このカテゴリが主担当の Decision: D17–D18, D59–D60, D64, D66, D69, D71–D72, D75–D78** (13 件)

`archive.md` の全 Decision は、いずれか 1 つのカテゴリが主担当として必ず
この一覧に載る。複数カテゴリにまたがるものは主担当だけに載せ、必要なら
本文から相互参照する (README の「重要なルール」3)。

⚠️ **この節は `generate_decision_index.py` が生成する。手で編集しない。**
`archive.md` との対応は `test_decision_index.py` が CI で検査するので、
追記漏れも、見出しを変えたことによるリンク切れも、そこで落ちる。

- [D17](./archive.md#d17-content-blocking--navigation-level-only-wry-056-exposes-no-subresource-hook) — Content blocking — navigation-level only, wry 0.56 exposes no subresource hook
- [D18](./archive.md#d18-devtools--feature-gating-shortcut-delivery-and-the-ipc-trust-boundary) — DevTools — feature gating, shortcut delivery, and the IPC trust boundary
- [D59](./archive.md#d59-サブリソースブロック--d17-の再検証windows-webview2-のみ実装できることが判明) — サブリソースブロック — D17 の再検証、Windows (WebView2) のみ実装できることが判明
- [D60](./archive.md#d60-サイト権限--wry-056-の-with_permission_handler-は存在するが-origin-もカスタム-ui-も渡せないorigin-単位ストア--安全側デフォルトの組み合わせで対応する) — サイト権限 — wry 0.56 の `with_permission_handler` は存在するが origin もカスタム UI も渡せない、origin 単位ストア + 安全側デフォルトの組み合わせで対応する
- [D64](./archive.md#d64-easylisteasyprivacy-対応-23--自前パーサを拡張実データは同梱もダウンロードもしない) — EasyList/EasyPrivacy 対応 (#23) — 自前パーサを拡張、実データは同梱もダウンロードもしない
- [D66](./archive.md#d66-サイトデータ管理-26--wrywebviewclear_all_browsing_data-で全消去origin-単位はエンジンごとに非対称で見送り) — サイトデータ管理 (#26) — `wry::WebView::clear_all_browsing_data()` で全消去、origin 単位はエンジンごとに非対称で見送り
- [D69](./archive.md#d69-ページ内検索-43--3-エンジンとも自前-js-実装ネイティブ-find-api-は-windows-を優先する限り使えないと判明) — ページ内検索 (#43) — 3 エンジンとも自前 JS 実装、ネイティブ find API は Windows を優先する限り使えないと判明
- [D71](./archive.md#d71-ダークモードとブラウザ-ui-テーマ-31--30-の資産の棚卸しを行い) — ダークモードとブラウザ UI テーマ (#31) — #30 の資産の棚卸しを行い、
- [D72](./archive.md#d72-view-source-45--documentdocumentelementouterhtml-を取得しhtml-エスケープ済みテキストとして新規タブに-data-url-で表示する) — View Source (#45) — `document.documentElement.outerHTML` を取得し、HTML エスケープ済みテキストとして新規タブに `data:` URL で表示する
- [D75](./archive.md#d75-印刷pdf保存-40--印刷は-wrywebviewprint-unsafe-不要) — 印刷・PDF保存 (#40) — 印刷は `wry::WebView::print()` (unsafe 不要)、
- [D76](./archive.md#d76-名前を付けて保存-46--windows-は-webview2-の-calldevtoolsprotocolmethod-で-mhtml-保存--ネイティブ-save-as-ダイアログmacoslinux-は-outerhtml-の素の保存に留める) — 名前を付けて保存 (#46) — Windows は WebView2 の `CallDevToolsProtocolMethod` で MHTML 保存 + ネイティブ Save-As ダイアログ、macOS/Linux は outerHTML の素の保存に留める
- [D77](./archive.md#d77-キーボードショートカット管理-38--既存ショートカットの棚卸しと) — キーボードショートカット管理 (#38) — 既存ショートカットの棚卸しと
- [D78](./archive.md#d78-コンテキストメニュー-39--ネイティブ-api-ではなく-js-描画を採用content-webview-からの入力は専用の境界付き第-3-チャネルとして扱う) — コンテキストメニュー (#39) — ネイティブ API ではなく JS 描画を採用、content webview からの入力は専用の境界付き第 3 チャネルとして扱う

---

- [設計判断アーカイブ (全文)](./archive.md)
