# 拡張機能アーキテクチャ / 権限モデル

Issue #82 (Epic #80 Stage 1) の設計文書です。**実装より先に、信頼境界・
権限モデル・マニフェストスキーマを確定する**ことが目的で、この文書の時点では
拡張機能を読み込むコードは存在しません。

- 判断の要約と Revisit condition: docs/decisions/archive.md の **D159**
- マニフェストスキーマの機械可読な実装 (未配線): `src/browser/extension_manifest.rs`
  (#83 / #84 が使う土台。**アプリの挙動は変えていない**)
- 後続: #83 (Extension API 最小サブセット) / #84 (Lifecycle / Storage) /
  #87 (カスタマイズ)

**前提の姿勢: 拡張機能は悪意があるものとして設計する。** 「善意の開発者が
うっかり」ではなく、「ストア審査をすり抜けた/侵害された更新で悪意を持った
拡張機能が、ユーザの同意した権限の範囲で最大限に悪用を試みる」ことを想定して
決めています (§9 に、その想定に基づく決定を一覧化しています)。

## 1. 目標と非目標

### 目標

1. 拡張機能を **VeloX のセキュリティ境界の外側** (信頼しないコード) として扱う。
   拡張機能が動いても、ツールバー・ブラウザ状態・他タブ・他拡張機能の
   データに、ユーザが同意した権限を超えて触れられない。
2. **既定は全拒否 (default-deny)**。マニフェストに書いていない能力は存在しない。
3. 最小権限。広い権限 (全サイト・全タブ URL) は「明示フラグ + 目立つプロンプト」
   の先にだけ置き、代替の狭い入口 (`active_tab`) を用意する。
4. API をバージョン付きにし、後から権限や API を**足せる** (縮められない
   ことを前提に、最初は小さく始める)。
5. 性能目標 (Epic #57) を壊さない。拡張機能が 0 個のときのコストは 0、
   休止中の拡張機能は常駐プロセスを持たない。

### 非目標 (少なくとも初期は扱わない)

- **Chrome Web Store / Chrome 拡張機能の互換性。** Epic #80 が明記している
  とおり完全互換は目指さない。理由:
  (a) Chrome の拡張機能モデルは分離ワールド・拡張機能専用プロセス・Service
  Worker・`webRequest` などブラウザエンジン内部の機構に依存しており、wry が
  公開する API (§3) では**同じ安全性で再現できない**。互換を名乗って
  安全でない近似を出すより、小さく安全な独自 API を選ぶ。
  (b) 既存の拡張機能の多くは `webRequest` / `cookies` / 全サイト content script
  など、最初から広い権限を要求する。互換を優先するとそれらを許すことになり、
  最小権限の原則と正面からぶつかる。
  (c) 互換レイヤは仕様追従コストが継続的に発生する。
  将来 `chrome.*` の薄いシムを**アプリ層の上に**足すことは妨げない
  (その場合も §4 の権限検査を必ず通す)。
- **WebView2 ネイティブの拡張機能サポートの利用。** wry は Windows で
  `with_browser_extensions_enabled` / `with_extensions_path` を公開している
  (wry 0.57 `WebViewBuilderExtWindows`) が、**使わない**。これは Edge 由来の
  拡張機能をエンジンが直接読み込む機構で、VeloX の権限モデルを完全に
  バイパスする (VeloX は権限を検査できない)。将来にも有効化しない。
  Windows 優先方針 (CLAUDE.md) と一見衝突するが、安全性を優先する。
- 拡張機能によるツールバー (信頼 UI) の描画・改変、独自のポップアップ UI
  (`action.default_popup`)、`webRequest` によるネットワーク横取り、
  `cookies` API、`history` / `bookmarks` の読み取り、ネイティブメッセージング、
  DevTools 拡張。いずれも理由付きで §4.3 に「提供しない」と記録した。
- 自動更新・ストア・署名インフラ。§5 で「形」だけ決め、実装は #84 以降。
- macOS / Linux 固有の作り込み。Windows を先に成立させ、他 OS は「動く」
  最小実装 (CLAUDE.md の OS 優先度)。§3 の差異は Decision に記録済み。

## 2. 脅威モデル

### 資産 (守るもの)

| 資産 | 例 |
|---|---|
| ユーザのブラウジング内容 | 閲覧中ページの DOM・入力中のフォーム・ログイン済みセッション |
| ブラウザの状態 | 履歴・ブックマーク・入力履歴・サイト権限・設定・ダウンロード |
| 信頼 UI | ツールバー (アドレスバー・タブ列)・権限プロンプト |
| ローカルのファイル・OS | ファイルシステム、他プロセス |
| 他の拡張機能のデータ | 各拡張機能のストレージ |

### 攻撃者

| # | 攻撃者 | 能力 |
|---|---|---|
| A1 | **悪意ある拡張機能** | 任意の JS (content script / background) を書ける。マニフェストを自由に書ける。ユーザが承認した権限は持つ。更新で挙動を変えられる |
| A2 | **侵害された/悪意あるページ** | 任意の JS をページ内で動かせる。拡張機能の存在を検出し、拡張機能が使う経路に話しかけられる |
| A3 | **拡張機能 ↔ 拡張機能** | ある拡張機能が別の拡張機能の権限・データを狙う |
| A4 | **拡張機能の配布経路の攻撃者** | 更新パッケージの差し替え、改ざんされたアーカイブ (パストラバーサル・巨大ファイル) |
| A5 | **ソーシャルエンジニアリング** | 権限プロンプトの文言・名前・アイコンでユーザを騙す |

### 想定する攻撃と対策の対応

| 攻撃 | 対策 (節) |
|---|---|
| A1: 信頼 UI (ツールバー) の `ToolbarCommand` を偽造して任意の操作をさせる | 拡張機能・ページ由来の入力は **ツールバーの IPC パーサに到達させない** (§3.2)。専用の別チャネルを使う |
| A1: 権限のない API を呼ぶ / 権限を勝手に増やす | 権限検査は **Rust 側のメインスレッドで、拡張機能 ID を Rust が決めた経路から得て**行う。メッセージ内の自己申告 ID は信用しない (§4.1) |
| A1: 承認済み権限より広いサイトにアクセスする | ホストパターンを検査 (§4.2)。`<all_urls>` は明示フラグ必須。`file:` / `velox:` / `data:` などは**パターンに書けない** |
| A1: 更新で権限を密かに増やす | 権限が増える更新は無効化して再承認 (§5) |
| A1: 大量データ・巨大 JSON・無限ループで DoS | 全入力にサイズ・件数上限 (`extension_manifest.rs` の定数、IPC は D62 と同様の上限)。background はイベント駆動で、CPU/メモリ予算超過時に停止 (§5) |
| A1: ツールバー/権限プロンプトのなりすまし (名前に制御文字・双方向文字) | `name` / `description` の文字種を制限 (`is_display_text`)。`velox.` 接頭辞の ID を予約 (§9) |
| A2: ページが content script の内部を覗く・メッセージを偽造する | content script は**ページと同じ JS ワールドで動く** (§3.1) ため秘匿性を仮定しない。特権 API は content script に置かず、権限は Rust 側の状態 (タブの現在 URL) から導出する (§6) |
| A2: ページが拡張機能 API を直接叩いて特権を得る | 拡張機能用チャネルからは「ページ自身ができること」以上の権限を与えない。特権操作は background 経由のみ (§4.1, §6) |
| A3: 別拡張機能のストレージ/権限を読む | 拡張機能ごとに別 `WebContext` / プロファイル、ストレージは ID 別ディレクトリ (§7) |
| A4: 改ざん・zip slip・巨大パッケージ | パッケージ内パスの検証 (`validate_resource_path`)、展開サイズ/ファイル数の上限、ハッシュ固定 (§5) |
| A5: 名前や説明でユーザを誤認させる | 名前・説明の制限、ID をプロンプトに併記、`is_sensitive()` な権限は警告を強調 (§4.4) |

### スコープ外 (守れないもの)

- **ユーザが承認した権限の範囲内での悪用**は防げない (例: 全サイトの content
  script を許可した拡張機能は、そのサイトの内容を読める)。だから広い権限は
  「明示フラグ + 強い警告」を必須にして、ユーザに判断材料を出す。
- WebView エンジン自体の脆弱性 (レンダラ脱出など) と、OS レベルのマルウェア。
- ユーザが自分で `manifest.json` を書き換えて広い権限を承認するケース。

## 3. 信頼境界 (既存アーキテクチャとの接続)

### 3.1 現在の境界

docs/architecture.md の「Why a webview toolbar?」が定める、既存の 2 つの境界:

```
 信頼する            ┌───────────────────────┐
 (自前 HTML)         │ toolbar webview        │  ToolbarCommand (構造化 JSON) を受ける
                     └──────────▲────────────┘
                                │ ← ページ/拡張機能の入力を絶対に通さない (D18)
 信頼しない          ┌──────────┴────────────┐
 (任意のページ)      │ content webview (タブごと)│  固定文字列センチネル / 上限付き第 3 チャネル
                     └───────────────────────┘
```

- toolbar webview と content webview は**別の WebView (Linux ではプロセスも別)**。
  content 側の `window.ipc.postMessage` は、toolbar の `ToolbarCommand` パーサとは
  **別のハンドラ**に届く (D18)。
- content 側ハンドラは、キーボードショートカットなど**固定文字列との完全一致**
  だけを受け付け、構造化データをデシリアライズしない (D18 / D23)。
  ページ由来のデータが避けられないコンテキストメニュー (D78) と入力復元
  (D142) だけが、**サイズ上限付き・接頭辞付き・サニタイズ後にのみ使う**
  専用の第 3 チャネルとして例外的に存在する。
  実装: `src/ui/window/content_scripts.rs` (`OPEN_DEVTOOLS_MESSAGE`,
  `CONTEXT_MENU_OPEN_PREFIX`, `parse_content_shortcut` など)。
- `browser::` 層は UI/エンジン非依存の純粋ロジックで、状態変更は
  メインスレッドの `UserEvent` に集約 (ロックなし)。

**新しい信頼境界は「拡張機能」を 1 つ足すことで作る。** 既存の 2 つの境界は
動かさない。

### 3.2 拡張機能を加えた境界

```
 信頼する      toolbar webview ── ToolbarCommand
 ──────────────────────────────────────────────────────────  (A) 信頼境界
 半信頼        Rust: extension host (browser::extension_* + app.rs の UserEvent)
               ・権限検査・ID の決定・レート/サイズ制限はここだけで行う
 ──────────────────────────────────────────────────────────  (B) 拡張機能の境界
 信頼しない    background webview (拡張機能ごと・イベントページ)
               ・専用 WebContext / プロファイル、専用の IPC ハンドラ
 信頼しない    content webview に注入された content script
               ・**ページと同じ JS ワールド** (§3.3)
 信頼しない    ページ本体 (A2)
```

決定事項:

1. **拡張機能由来の入力は、専用の IPC ハンドラでだけ受ける。** toolbar の
   `ToolbarCommand` パーサにも、D18 の固定センチネルのチャネルにも混ぜない
   (D18 の「2 つ目のコンテンツ由来アクションが必要になったら専用のセンチネル/
   バリアントを持たせ、既存ハンドラを構造化パーサに育てない」を踏襲)。
2. 拡張機能ハンドラはサイズ上限 (D62 と同様に**パース前**) を課し、
   `serde` の `deny_unknown_fields` 付き型にだけデシリアライズし、未知の
   メッセージ種別は捨てる。
3. **呼び出し元の拡張機能 ID は Rust が決める。** background webview は
   1 拡張機能に 1 つ作り、その webview の IPC ハンドラのクロージャが ID を
   束縛する (`with_ipc_handler(move |req| ... ext_id ...)`)。メッセージ本文中の
   `"extension_id"` 相当のフィールドは存在させない・読まない。
   (content script → background の経路は §6。)
4. 権限検査と状態変更は必ず `UserEvent` ディスパッチ (メインスレッド)
   で行い、`app.rs` 以外で行わない。
5. 拡張機能が作る**表示物** (通知・コンテキストメニュー項目) は、VeloX が
   拡張機能名を付けて装飾し、信頼 UI (ツールバー/権限プロンプト) と区別
   できるようにする。拡張機能はツールバー webview に何も描画・注入できない。

### 3.3 各エンジンで拡張機能のコードが動く場所 (wry 0.57 で実確認)

D25/D59/D60/D66/D69/D78 と同じく、推測ではなく**実ソース**
(`~/.cargo/registry/src/*/wry-0.57.0/`) で確認した。

| 原始機能 | WebView2 (Windows) | WKWebView (macOS) | WebKitGTK (Linux) |
|---|---|---|---|
| ドキュメント開始時の初期化スクリプト | `AddScriptToExecuteOnDocumentCreated` (`webview2/mod.rs`) | `WKUserScript` `AtDocumentStart` (`wkwebview/mod.rs` `init`) | `UserScript` `Start` (`webkitgtk/mod.rs` `init`) |
| サブフレームへの注入 | **常にサブフレームにも入る** (`for_main_frame_only` は無視。`lib.rs` の doc に明記) | `forMainFrameOnly` で制御可 | `TopFrame` / `AllFrames` で制御可 |
| 実行される JS ワールド | **ページと同じ (main world)** | **ページと同じ** (`WKContentWorld` は iOS 側の未使用バインディングにしか無い) | **ページと同じ** |
| IPC (`window.ipc.postMessage`) | `chrome.webview.postMessage`。要求 URI (フレーム URL) が付く | `webkit.messageHandlers.ipc` | `webkit.messageHandlers.ipc`。**iframe でもトップフレームの URL が付く** (`lib.rs` doc: "Linux / Android: The request URL is not supported on iframes") |
| カスタムプロトコル | `http://<scheme>.<path>` (`https` に切替可) | `<scheme>://<path>` | `<scheme>://<path>` (共有 `WebContext` では既登録の名前を再登録できない) |
| データ分離の単位 | WebView2 の user data folder / `with_profile_name` (Cookie・ストレージ・IndexedDB・キャッシュが分離) | `WKWebsiteDataStore` | `WebContext` (data directory) |
| ネットワーク横取り | `WebResourceRequested` (D59 で content blocking に使用済み) | 公開なし | 公開なし |

ここから導かれる**制約 (設計を縛る事実)**:

1. **分離ワールドは無い。** content script は必ずページと同じ JS ワールドで
   動く。ページはプロトタイプ汚染・関数の差し替え・`window` の観察で
   content script を覗き、改ざんできる。→ **content script に秘密を持たせない。
   content script が送るメッセージは、ページが偽造できるものと同等に扱う。**
2. **IPC の呼び出し元フレームを OS 間で信頼して特定できない**
   (Linux は iframe でも親フレームの URL)。→ 権限判断に「メッセージを送った
   フレームの URL」を使わない。判断に使うのは Rust が持つ**タブの現在の
   トップフレーム URL** (`browser::Tab`) だけ。
3. **Windows は `for_main_frame_only: false` 相当が強制**される。content script
   の `all_frames: false` (既定) は、Windows では初期化スクリプト内部で
   `window === window.top` を自己検査して実現する必要がある (ページに
   検査を外せるわけではない — 外されても「サブフレームでも動く」だけで、
   ホストパターンの一致はスクリプト注入前に Rust が確認済みのため権限は増えない)。
4. **`velox:` や `file:` などのカスタム/ローカルスキームのページには注入しない。**
   ホストパターンが `http` / `https` にしか書けない (§4.2) ことで保証する。
5. WebView2 のプロファイル分離 (`with_profile_name`) は 1 つの環境内で
   Cookie/ストレージを分けられる。**background webview を拡張機能ごとの
   プロファイル/`WebContext` で作る**ことでストレージ分離を OS 機能に任せる (§7)。
   Linux では拡張機能ごとの `WebContext`、macOS は非永続 or 専用ストア。
   *(プロトタイプ #84 で、WebView2 のプロファイル数に応じたメモリ増を計測して
   から確定する。)*

### 3.4 バックグラウンド実行の形

- background は**イベントページ**: 拡張機能ごとに、必要になったとき
  (イベント配送・ユーザ操作) だけ隠し WebView を作り、アイドルが続けば破棄する。
  常駐 (`persistent`) は提供しない。理由: (a) 性能目標 (休止中の拡張機能は
  プロセスを持たない)、(b) 悪意ある拡張機能が常時稼働するのを避ける、
  (c) 既存のタブ休止 (D 記録の suspension) と同じ思想。
- 拡張機能自身のページ (background) はカスタムプロトコルで**パッケージ内
  ファイルだけ**を配信して読み込む。ハンドラは (1) パスを正規化し
  パッケージルート外・隠しファイルを拒否、(2) マニフェストが宣言した
  リソースだけを返し、(3) 外部 URL への読み込みを `with_navigation_handler` で
  拒否する。

## 4. 権限モデル

### 4.1 原則

1. **default-deny。** マニフェストが宣言し、かつユーザが承認した権限だけが
   有効。宣言は「要求」であって「付与」ではない。
2. **権限は拡張機能 ID に紐づく Rust 側の状態。** チェックは `browser::`
   層の純粋関数 (`fn is_allowed(ext, permission, tab_url) -> bool`) として
   実装し、単体テスト可能にする (`site_permissions.rs` と同じ形)。
3. **未知のものは拒否。** 未知の権限名・未知のマニフェストキー・未知の
   メッセージ種別・未知の `manifest_version` はすべて拒否する
   (`site_permissions::PermissionKind::Other` が常に Block なのと同じ規律)。
4. **権限は TOCTOU を避けて使用時に検査する。** インストール時のチェックだけで
   済ませず、API 呼び出しごとに現在の承認状態を見る (無効化・失効が即時に効く)。

### 4.2 ホストパターン (origin access)

書式: `scheme://host[:port]/path` と特別トークン `<all_urls>`
(実装: `HostPattern`)。

| 要素 | 規則 |
|---|---|
| scheme | `http` / `https` / `*` (= http と https **のみ**)。`file` `data` `blob` `velox` `about` `javascript` `ftp` は**書けない** |
| host | 小文字 ASCII のドメイン名 (IDN は punycode)、`*.example.com` (自身とサブドメイン)、`*` (全ホスト)。`*.com` のような公開サフィックス相当のワイルドカードは不可 (ドット無しのドメイン部を拒否)。IP リテラルは v4 のみ (`127.0.0.1`)。IPv6 は不可 |
| port | 省略 = **そのスキームの既定ポートのみ** (`http://localhost/*` は `:3000` に一致しない)。指定は 1〜65535 の数字のみ |
| path | `/` で始まる。`*` は任意長 (空含む)。空白・制御文字・`#` 不可。**一致対象はパス + クエリ** (フラグメントは無視) |
| userinfo | パターンに書けない。`user@host` 形式の URL はどのパターンにも**一致しない** (`https://example.com@evil.test/` の誤認防止) |

- 一致判定は URL をパーサで正規化した後に行う (`..` の解決・ホスト小文字化)。
- **全ホストに効くもの (`<all_urls>`、`*://*/*`、`https://*/*` …) は
  `"allow_all_urls": true` を明示しないとマニフェストが受理されない。**
  プロンプトでも最上位の警告にする。
- `content_scripts.matches` は**ホストアクセスとして数える** (スクリプトが走る
  = そのページを読み書きできる)。`host_permissions` に書かなくても、
  インストール時のプロンプトには含める。`exclude_matches` は権限を増やさない。
- `optional_host_permissions` は実行時にユーザ操作を起点として要求する
  (§4.4)。

### 4.3 権限一覧 (閉じた集合)

**初期スキーマ (`manifest_version: 1`) で存在する権限は次の 8 つだけ**
(実装: `extension_manifest::Permission`)。

| 権限 | 与えるもの | リスク / 備考 |
|---|---|---|
| `storage` | 拡張機能ごとに分離された小さなキーバリュー (#84)。クォータあり | 低。他拡張機能・ページからは不可視 |
| `alarms` | 一定間隔の起床 | 低。最小間隔 (例 1 分) を実行時に強制。バックグラウンド常時稼働の抜け道にしない |
| `tabs` | **全タブの URL・タイトルの読み取り** | **高** (閲覧履歴に近い)。`active_tab` で足りるならそちらを使わせる |
| `active_tab` | ユーザが拡張機能を明示操作したタブ 1 枚への、一時的なホストアクセス (ナビゲーションで失効) | 中。常時ホスト権限の代替。推奨 |
| `scripting` | 実行時のスクリプト注入 | **高**。ホスト権限または `active_tab` と併用必須 (無ければ manifest 拒否) |
| `context_menus` | コンテキストメニュー (D78) への項目追加 | 低〜中。項目は VeloX が拡張機能名付きで表示 |
| `clipboard_write` | クリップボードへの書き込み**のみ** | 中。ユーザ操作 (user activation) 起点に限る。読み取りは提供しない |
| `notifications` | OS 通知 | 低〜中。表示は VeloX が装飾 |

**意図的に提供しない (マニフェストでは `ReservedPermission` として拒否):**

| 名前 | 提供しない理由 |
|---|---|
| `cookies` | セッション奪取に直結。ホスト権限があっても Cookie へ直接触らせない |
| `web_request` / `declarative_net_request` | ネットワーク横取り。wry では WebView2 でしか実現できず (§3.3)、OS 間で意味が揃わない。コンテンツブロックは VeloX 本体の機能 (D59) |
| `history` / `bookmarks` | 閲覧履歴・ブックマークの一括読み取りは、ホスト権限より遥かに大きな情報漏えい。必要になったら個別の狭い API として別 Decision |
| `native_messaging` | サンドボックス脱出 |
| `debugger` | エンジン内部への任意アクセス |
| `management` | 他拡張機能の操作 (A3) |
| `proxy` | 全通信の経路変更 |
| `downloads` | 任意ファイルの書き込み・実行への足がかり。VeloX のダウンロード管理は別途 |

**権限を足すときの手順:** 脅威モデル (§2) の更新 → この表への追記 →
`Permission` 列挙への追加 → Decision の追記。文字列を 1 つ足すだけでは
済ませない。

### 4.4 付与のタイミングと UI

| 種類 | いつ | 手段 |
|---|---|---|
| 必須 (`permissions` / `host_permissions` / `content_scripts.matches`) | インストール時 | 全項目を一覧表示する承認ダイアログ。承認しなければインストールしない。承認は「宣言の全部」であり、部分承認はしない (複雑さを避ける) |
| 任意 (`optional_*`) | 実行時。**ユーザ操作 (クリック等) の直後に限る** | 個別プロンプト。拒否は正常系として返す |
| `active_tab` | ユーザが拡張機能を起動した瞬間 | プロンプトなし。ただし対象は**そのタブ 1 枚・ナビゲーションまで** |

- プロンプトは**信頼 UI** (ツールバー webview 側) が描く。拡張機能名・ID・
  バージョンと、`is_sensitive()` な権限・全ホスト権限の強調を含む。
  拡張機能が渡す文字列 (`name` / `description`) は制御文字・双方向制御文字・
  ゼロ幅文字を拒否済み (なりすまし対策)。
- **永続する承認はインストール単位**で、Site permission (`site_permissions.rs`、
  カメラ等) とは別のストア。ユーザは拡張機能ごとに、いつでも
  権限を個別に取り消せる (取り消しは即時に効く)。
- **プライベートウィンドウ (D15) では既定で無効。** ユーザが拡張機能ごとに
  明示的に許可した場合だけ動く。プライベートブラウジングの前提 (痕跡を
  残さない) を、拡張機能が黙って破らないため。

### 4.5 Site permissions との関係

ページが要求するカメラ/位置情報などのサイト権限 (D60) は**そのまま独立**。
拡張機能に権限を与えても、サイトのカメラ許可の判定を変えない。逆に拡張機能は
`PermissionKind` を書き換えられない。

## 5. ライフサイクル

状態: `Installed(disabled)` → `Enabled` → (`Suspended`: イベントページ休止) …
`Disabled` / `Uninstalled`。詳細な実装は #84。ここでは**安全性に関わる規則**を確定する。

| 遷移 | 規則 |
|---|---|
| **install** | (1) パッケージのサイズ・ファイル数・展開後合計を上限で検査 (zip bomb 対策)。(2) 全エントリのパスを `validate_resource_path` 相当で検査し、絶対パス・`..`・`\`・隠しファイルを含む場合は**パッケージごと拒否** (zip slip)。(3) `manifest.json` を `parse_manifest` で検証。(4) 承認ダイアログ。(5) 承認後にのみディスクへ確定 (一時領域に展開 → 検証 → アトミックに移動)。失敗時は何も残さない。**ID は最初にインストールされた時点のものを永続に紐づけ**、同じ ID の別パッケージは更新経路以外では入らない |
| **enable / disable** | 無効化はイベントページを即時破棄し、content script の注入を止め、`alarms` を解除する。**無効化してもデータは残す** (再有効化で復元)。既定はインストール直後に有効化 (承認済みのため) だが、プライベートウィンドウは §4.4 のとおり別 |
| **update** | 新しい `version` は**単調増加のみ** (`ExtensionVersion` の順序。ダウングレードは拒否 = ロールバック攻撃対策)。**新マニフェストが要求する権限・ホストが承認済みの集合を 1 つでも超えたら、更新を適用したまま拡張機能を無効化し、再承認を求める。** 減る分には黙って適用してよい。`id` の変更は更新ではなく別拡張機能。更新はアトミックに置換し、失敗時は旧版を保つ |
| **uninstall** | パッケージ・ストレージ (専用プロファイル/`WebContext`)・承認記録・alarms をすべて削除する。削除失敗は「次回起動時に再試行するトゥームストーン」を残す (半端に残って ID が再利用されるのを避ける) |
| **crash / 破損** | マニフェストの再検証に失敗した拡張機能は**無効化して隔離**し、起動をブロックしない (D62 の「壊れたファイルで起動不能にしない」と同じ)。ストレージ破損は #84 で復旧手順を定義 |
| **実行予算** | イベントページは CPU 時間・メモリ・メッセージ量に上限を持ち、超過したら停止して警告を出す。上限値の具体化は #84/#83 |

**配布形態 (初期):** ストアは持たず、ユーザがローカルのパッケージを
インストールする開発者向けの形から始める。パッケージの完全性 (ハッシュ/
署名) と自動更新は、配布経路を決める段階で別 Decision (Revisit)。

## 6. content script の境界

- 注入は `wry` の初期化スクリプト (§3.3) で行う。**ホストパターンの一致は
  注入前に Rust が確認**し、一致するタブのトップフレーム URL に対してのみ
  スクリプトを渡す。一致しなければスクリプトはその WebView に**存在しない**
  (「実行時に自己判定して何もしない」方式にしない — ページから見えてしまう)。
- ナビゲーションのたびに (`with_on_page_load_handler` / ナビゲーション
  ハンドラ) 再評価する。SPA の `pushState` による URL 変更は新規注入を
  起こさない。**権限判断は常に Rust 側が持つ最新のタブ URL に対して**行う。
- `all_frames` の既定は `false`。`true` の場合でも、サブフレームの URL 一致は
  Linux では IPC から確認できない (§3.3 制約 2) ため、**サブフレーム注入は
  Windows でのみ最初に対応**し、他 OS は `all_frames: false` のみ (最小実装)。
- **content script は特権 API を持たない。** 提供するのは background への
  メッセージ送信 (`runtime.sendMessage` 相当) だけ。`tabs` / `storage` /
  `scripting` などの特権 API は background (専用 WebView・ID 束縛の IPC)
  からのみ呼べる。理由: content script はページと同じワールドで動く (§3.3
  制約 1) ため、そこに特権があるとページが乗っ取れる。
- content script → background のメッセージは、ページが偽造しうるもの
  として扱う: (a) サイズ上限、(b) スキーマ検証済みの型にだけデシリアライズ、
  (c) 送信元タブは Rust が (ID 束縛された webview から) 決める、
  (d) **content script のメッセージだけを根拠に権限を上げない**。
- content script は**ページの資源**に触れる。従って content script から
  得た情報は、ページ内容と同じ機密度であり、`storage` へ書くか background へ
  送るかはその拡張機能の責任範囲 (= ユーザが承認したホスト権限の範囲)。
- `world: "MAIN"` のような選択肢は存在しない (常に main world で、選べない)。
  将来 wry が分離ワールドを公開したら、**それを使う変更は互換的な強化**として
  行える (Revisit)。
- 注入対象外: `velox:` を含むすべての内部ページ、`file:` `data:` `blob:`
  `about:`、およびツールバー webview。

## 7. ストレージの分離 (#84 への引き継ぎ)

#84 が実装する。ここで**要件**だけ確定する:

1. 拡張機能ごとに**別のデータ領域** (§3.3 の WebContext/プロファイル、および
   `storage` API のバックエンドは ID 別ディレクトリ)。他拡張機能・ページ・
   VeloX 本体の永続ファイル (`history.json` 等) と混ざらない。
2. **クォータ**を持つ (総容量・キー数・値サイズ)。超過は書き込み失敗として返す。
3. ページの origin のストレージ (localStorage/Cookie) とは別物。拡張機能は
   ページのストレージへ `storage` 経由で触れない。
4. uninstall で確実に消える (§5)。
5. 書き込みは原子的 (一時ファイル → rename、`persistence` と同じ流儀)。
   壊れたデータは検出して隔離し、その拡張機能のみ影響を受ける。
6. 秘密情報を保存する API は提供しない (拡張機能のストレージは平文と
   みなす)。
7. Sync (#85/#86) には**拡張機能のストレージを含めない** (含めるなら別 Decision。
   サーバ側で読める情報を最小化する Epic の方針)。

## 8. API バージョニング

- `manifest_version` (整数): **マニフェストのスキーマ**の版。VeloX は自分が
  理解する版だけ受理し、それ以外 (古い・新しい) は拒否する。現在は `1`。
  `serde` の `deny_unknown_fields` と組み合わせて、**新しい版のマニフェストを
  古い VeloX が黙って部分解釈しない**ことを保証する。
- `min_velox_version` (任意): 拡張機能が要求する VeloX の最小版。満たさない
  場合はインストールを拒否 (互換性の宣言であって、権限ではない)。
- **API の版 (#83 で定義)** は `manifest_version` とは別に、`runtime`
  名前空間の版として持つ。互換を壊す変更は `manifest_version` を上げる。
- **権限の追加は互換的変更** (`Permission` に足す + 更新時の再承認 §5 が
  すでに存在)。**権限の意味を広げる変更は互換的変更ではない** (既存の
  承認が予期しない範囲に及ぶため)。広げたい場合は新しい権限名を作る。
- 非推奨: 権限や API を廃止する場合は 1 つ以上の `manifest_version` の間
  警告付きで残す。ただし**安全性の理由**による撤去は猶予なしで行える。

## 9. マニフェストスキーマ

`manifest.json` (UTF-8、最大 64 KiB)。実装と網羅テスト:
`src/browser/extension_manifest.rs`。**下の表がスキーマの正**であり、
コードとずれたらどちらも直す。

```json
{
  "manifest_version": 1,
  "id": "com.example.hello",
  "name": "Hello",
  "version": "1.2.3",
  "description": "短い説明",
  "min_velox_version": "0.5.0",
  "permissions": ["storage", "active_tab"],
  "optional_permissions": ["tabs"],
  "host_permissions": ["https://*.example.com/*"],
  "optional_host_permissions": ["https://example.org/docs/*"],
  "content_scripts": [
    {
      "matches": ["https://example.com/*"],
      "exclude_matches": ["https://example.com/private/*"],
      "js": ["content/main.js"],
      "run_at": "document_idle",
      "all_frames": false
    }
  ],
  "background": { "script": "bg.js" },
  "allow_all_urls": false
}
```

| フィールド | 必須 | 型 / 規則 |
|---|---|---|
| `manifest_version` | 必須 | 整数。**`1` のみ** |
| `id` | 必須 | 3〜64 バイト。`[a-z0-9._-]`、先頭は英小文字、末尾は英数字、`..` 不可。**`velox.` 接頭辞は予約 (拒否)** |
| `name` | 必須 | 1〜45 文字。前後空白・制御文字・双方向制御文字・ゼロ幅文字不可 |
| `version` | 必須 | `MAJOR.MINOR.PATCH[-pre]`。各数値は先頭ゼロ無し・9 桁以内。順序あり (更新は単調増加) |
| `description` | 任意 | 132 文字以内。`name` と同じ文字種規則 |
| `min_velox_version` | 任意 | `version` と同形式 |
| `permissions` | 任意 | §4.3 の閉じた集合の名前。重複不可。未知は `UnknownPermission`、提供しない名前は `ReservedPermission` |
| `optional_permissions` | 任意 | 同上。`permissions` と重複不可 |
| `host_permissions` | 任意 | §4.2 のパターン。50 件まで。重複不可 |
| `optional_host_permissions` | 任意 | 同上。`host_permissions` と重複不可 |
| `content_scripts` | 任意 | 20 件まで。各要素は下記 |
| `content_scripts[].matches` | 必須 | 1〜20 件のパターン。**ホストアクセスとして数える** |
| `content_scripts[].exclude_matches` | 任意 | 20 件まで |
| `content_scripts[].js` | 必須 | 1〜20 件のパッケージ内相対パス |
| `content_scripts[].run_at` | 任意 | `document_start` / `document_end` / `document_idle` (既定 `document_idle`) |
| `content_scripts[].all_frames` | 任意 | 既定 `false` |
| `background` | 任意 | `{ "script": <相対パス> }` のみ。イベントページ。`persistent` は存在しない |
| `allow_all_urls` | 任意 | 既定 `false`。全ホスト対象のパターンがあるとき `true` 必須 |

**リソースパス** (`js` / `background.script`): `.js` で終わり、128 バイト以内、
`[A-Za-z0-9/._-]` のみ、絶対パス・`..`・`.` セグメント・空セグメント・
`.` で始まるセグメント・`\`・`:` を含まない。実在確認は行わない (呼び出し側の責務)。

**横断規則:**

- 未知のフィールドは**トップレベルも入れ子も拒否** (`web_accessible_resources`
  など Chrome 由来のキーを黙って無視しない)。
- `scripting` を要求するには、`host_permissions` / `optional_host_permissions` /
  `active_tab` のいずれかが必要。
- 全ホスト対象パターン (`<all_urls>` / `*://*/*` / `https://*/*` …) は
  `host_permissions` / `optional_host_permissions` / `content_scripts.matches`
  のどこにあっても `allow_all_urls: true` が必須 (`exclude_matches` は除く)。
- 既知の制約: `serde_json` は**同じキーの重複を後勝ちで受理**する。検証は
  解釈後の値に対してのみ行う。パッケージ全体の整合 (ハッシュ) は §5 で扱う。

## 10. 悪意ある拡張機能を想定して決めたこと (一覧)

D159 の要約。各項目は上の節に根拠がある。

1. 拡張機能を信頼境界の**外側**に置く。ツールバー webview に拡張機能由来の
   入力を通さない。専用の IPC ハンドラ (D18 の分離を踏襲)。(§3.2)
2. 呼び出し元 ID は **Rust が経路から決める**。メッセージ内の自己申告 ID は
   存在させない。(§3.2, §4.1)
3. 権限は **default-deny**、使用時に検査、取り消しは即時。(§4.1)
4. content script は**ページと同じワールド**という前提で、特権 API を
   持たせず、そのメッセージを偽造可能として扱う。権限判断はタブの
   現在 URL (Rust 側) から。(§3.3, §6)
5. **注入前に Rust がホストパターンを検査**し、不一致ならスクリプトを存在
   させない。(§6)
6. 権限の集合は**閉じた列挙**。未知は拒否、提供しない権限は明示的に拒否。
   `cookies` / `web_request` / `history` / `bookmarks` / `native_messaging` /
   `debugger` / `downloads` は提供しない。(§4.3)
7. ホストパターンは `http` / `https` のみ。`file` / `data` / `velox` などは
   書けない。ポート省略は既定ポートのみ。userinfo 付き URL は一致しない。
   `*.com` 相当は不可。全ホストは `allow_all_urls` 明示必須。(§4.2)
8. **更新で権限が増えたら無効化して再承認**。ダウングレード拒否。(§5)
9. 表示文字列は制御・双方向・ゼロ幅文字を拒否。`velox.` ID を予約して
   ファーストパーティのなりすましを防ぐ。(§4.4, §9)
10. マニフェスト・パッケージ・メッセージの**すべてにサイズ/件数上限**、
    未知フィールド拒否、パスの正規化 (zip slip)。(§5, §9)
11. background は**イベントページのみ** (常駐しない)、拡張機能ごとに別
    プロファイル/`WebContext`、実行予算あり。(§3.4, §5, §7)
12. **WebView2 ネイティブ拡張機能サポートは使わない** (権限モデルの
    バイパスになる)。(§1)
13. **プライベートウィンドウでは既定で無効**。(§4.4)
14. Chrome 互換は目指さない。権限の意味を暗黙に広げる変更を禁止。(§1, §8)

## 11. 実装の段階と未決事項

| Issue | 範囲 |
|---|---|
| #82 (本文書) | 設計・スキーマ (`extension_manifest.rs`、未配線)・Decision D159 |
| #83 | API 仕様 (versioned schema)・最小 API (`runtime` / `storage` / `tabs` / `scripting`)・権限検査・統合テスト |
| #84 | インストール/更新/削除・有効化/無効化・ストレージ (分離・クォータ・復旧) |
| #87 | 拡張機能設定 (`extension preferences`) |

未決 (後続で実測・決定する):

- background 用に拡張機能ごとの WebView2 プロファイル/Linux の `WebContext`
  を作るコスト (メモリ・起動時間)。#84 のプロトタイプで計測してから確定する。
  高すぎる場合は「拡張機能ごとに 1 プロセスを諦め、ID 束縛 IPC + 別 origin
  のみで分離する」代替を Decision として比較する。
- カスタムプロトコルの origin が Windows (`http://<scheme>.<path>`) で
  拡張機能ごとに分離されるか。ID をホスト部に持つ形で実機確認する。
- サブフレーム content script の Linux/macOS 対応。
- 拡張機能パッケージの完全性 (署名/ハッシュ) と自動更新の配布経路。
- 実行予算 (CPU・メモリ・メッセージ量) の具体値。
