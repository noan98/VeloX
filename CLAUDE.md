# CLAUDE.md

このファイルは、本リポジトリのコードを扱う際に Claude Code (claude.ai/code) に
向けたガイダンスを提供します。

## 言語ポリシー

- **ユーザへの応答はすべて日本語で行ってください。** 説明・質問・確認プロンプト・
  ツール実行前の説明・進捗報告・エラー説明・最終サマリーなど、チャットに出力する
  すべての文章を日本語で記述します。これは Claude Code on the web (クラウド実行
  環境) を含む、本リポジトリで Claude Code が動作するすべての状況に適用される、
  例外のないルールです (コード・コマンド・識別子など本来英語で書くべきものは除く)。
- **プルリクエスト (PR) の作成は必ず日本語で行ってください。** PR のタイトル・
  本文・サマリー・テスト計画など、PR に含まれるすべての記述を日本語で記述します。
  これは Claude Code が本リポジトリで PR を作成するすべての状況に適用される、
  例外のないルールです。

## Issue のラベリングポリシー

- **新規 Issue を作成するときは、対応コストとメリットを必ずラベルで明示してください。**
  運用判断 (どれから着手するか / 後回しにするか) の材料になるため、両軸が揃って
  いない Issue は作成しないでください。既存 Issue を更新する際にも、これらの
  ラベルが付いていなければ合わせて付与します。
- **コスト (実装にかかる労力) は 3 段階**。実装規模・影響範囲・必要な検証を踏まえて
  判定します。
  - `cost:low` — 数時間〜半日程度。設定追加やツールバー UI の小改修など、影響範囲が
    限定的。
  - `cost:medium` — 1〜数日程度。新しいモジュール 1 つや既存パターンの拡張で済む規模。
  - `cost:high` — 1 週間以上。レンダリングエンジンの差し替え・マルチプロセス化・
    複数レイヤ (UI / browser / engine) を跨ぐ大規模変更など、設計検討と広範な検証が
    必要。
- **メリット (対応することによる価値) は 5 段階**。利用者への影響度・対象ユーザ数・
  事故防止や日常 DX への寄与を踏まえて判定します。
  - `benefit:1` — ごく一部のユーザのみが恩恵を受ける、または見た目の微調整レベル。
  - `benefit:2` — 一部ユーザの利便性が改善する程度。
  - `benefit:3` — 多くのユーザが日常的に恩恵を受ける QoL 改善や、特定ユースケース
    での価値が大きい機能。
  - `benefit:4` — 主要なワークフローを大きく改善する、または README ロードマップに
    明記された重要機能。
  - `benefit:5` — プロダクトの位置付けや安全性を一段引き上げる中核機能 (複数タブ・
    パフォーマンス計測基盤・コンテンツブロッキングなど)。
- ラベルは GitHub 上に存在しなければ自動作成されますが、命名は上記に厳密に従って
  ください (`cost:low|medium|high`、`benefit:1`〜`benefit:5`)。揺れがあると後段の
  集計・フィルタが壊れます。
- 判断に迷ったら Issue 本文の末尾に「コスト: medium (理由: ...) / メリット: 4
  (理由: ...)」のように短い根拠を残しておくと、後から見直しやすくなります。

## Issue と PR の紐付け

- **関連 Issue がある PR では、本文にクロージングキーワードを必ず含めてください。**
  GitHub は PR 本文 (またはマージ先ブランチに残るコミットメッセージ) に
  `Closes #123` / `Fixes #123` / `Resolves #123` などのキーワードが含まれている
  ときだけ、マージと同時に Issue を自動でクローズします。タイトルの `(#123)` や
  本文中の `#123` 単独はリンクされるだけで、close はされません。
- 複数 Issue を解消する PR では、それぞれにキーワードを付けてください。例:

  ```
  Closes #77
  Closes #73
  ```

  または 1 行で `Closes #77, closes #73` のように書けます。
- キーワード自体は英語のままで構いません (日本語本文との混在 OK)。PR 本文の
  冒頭または末尾の独立した行に置くのが確実です。コードブロックや引用 (`>`) の
  中に入れるとパースされません。
- 自動クローズの判定はマージ時点で行われます。マージ後に本文を編集しても
  Issue は閉じないため、その場合は手動で Issue をクローズしてください。
- **Epic (トラッキング Issue) の子をすべて解消する PR では、各子 Issue に加えて
  Epic 本体にも `Closes #<Epic番号>` を必ず入れてください。** Epic は子 Issue の
  クローズに連動して自動では閉じないため、最後の子をまとめて解消する PR で Epic
  も一緒に閉じます。例:

  ```
  Closes #115
  Closes #116
  Closes #154
  ```

  ただし子 Issue の一部だけを解消する (Epic がまだ完了しない) PR には Epic の
  `Closes` を入れないでください。早期クローズになります。その場合は子 Issue の
  キーワードのみ記載し、Epic は残った子が片付いた最後の PR で閉じます。

## コマンド

リポジトリのルートから実行します。ビルドシステムは plain `cargo` のみです。

```sh
cargo build                                  # ビルド
cargo run                                    # ブラウザを起動
cargo test                                   # ユニットテスト (URL 正規化・タブ状態・IPC プロトコル)
cargo clippy --all-targets -- -D warnings    # lint (CI と同じ)
cargo fmt --check                            # 整形チェック (CI と同じ)
```

Linux では WebKitGTK の開発パッケージが必要です:

```sh
sudo apt install libwebkit2gtk-4.1-dev   # Debian/Ubuntu
```

macOS (WKWebView) / Windows (WebView2) は追加のシステム依存なしでビルドできます。

**Windows 固有コードや `cfg` 分岐を触ったら、Linux 上でも型チェックできます。**
CI を一往復させる前に手元で確認してください:

```sh
rustup target add x86_64-pc-windows-msvc          # 初回のみ
cargo check --target x86_64-pc-windows-msvc --all-targets
```

リンクを伴わない型チェックのみなので MSVC ツールチェーンは不要です
(`webview2-com` / `tao` の Windows 版まで検査されます)。ただしリンクと実行は
しないため、これが通っても Windows で `cargo build` / `cargo test` が通る
保証にはなりません (docs/decisions.md D61)。

CI は `.github/workflows/ci.yml` が PR と `main` push で
fmt → clippy → test → build を Linux 上で実行します。加えて Windows
(windows-latest) ジョブが build → test (`--lib` のみ、統合テストは対象外)
を実行します (Issue #33、docs/decisions.md D61)。macOS ジョブは方針上
追加していません。

`.github/workflows/auto-merge.yml` は、`main` 向けの open な PR のうち
「Auto Merge 自身を除くすべてのチェックが success / skipped になった」ものを
自動でマージします (プライベートリポジトリでは GitHub 標準の auto-merge が
使えないための代替。docs/decisions.md D55 を参照)。自動マージさせたくない
PR には `no-automerge` ラベルを付けるか、Draft のままにしてください。

## 対応 OS の優先度

VeloX が対象とする 3 つの OS は同列ではありません。**開発リソースを Windows に
集中させ、他 OS は品質が固まってから整備する**という方針です。

- **Windows (WebView2) を最優先とします。** 新機能・不具合対応・性能改善は、
  まず Windows で動作し、日常利用に耐える品質になっていることを目標にします。
  仕様や実装方針で OS 間のトレードオフが生じたときは、Windows を優先して
  判断してください。
- **macOS (WKWebView) / Linux (WebKitGTK) は当面「最低限の整備」に留めます。**
  ビルドが通り、既存機能を壊していない状態を維持できていれば十分とし、OS 固有の
  作り込みや検証コストの大きい対応は後回しにします。なお Linux は CI
  (`.github/workflows/ci.yml`) と性能計測 (`.github/workflows/perf-gate.yml`、
  `docs/performance-targets.md`) の実行環境として引き続き使います。
- **macOS / Linux の本格対応は、製品としての品質が担保できた段階で着手します。**
  その時点で 3 OS のリリースビルドを検証・配布できるよう整備します (Issue #33)。
  それまでは「3 OS 同時対応」を完了条件に据えないでください。
- OS 別の分岐を書くときは **Windows の実装を先に用意**し、macOS / Linux は
  「動作する」ことを優先した最小実装で構いません。この方針で意図的に見送った
  OS 固有の差異は、後から拾えるよう `docs/decisions.md` に記録してください。
- 性能の数値は OS ごとに分けて記録する原則 (Epic #57) を維持します。Linux 上の
  計測結果を Windows の実力値として扱わないでください。

## Rust コード品質

- stable Rust を基本とし、`unsafe` は原則使用しません (使用する場合は理由を
  コメントで明記)。
- 本体コードでの `unwrap()` / `expect()` の乱用を避け、エラーは `Result` で
  伝搬します。UI 系の失敗 (スクリプト評価など) はクラッシュさせず stderr に
  ログして継続します (`app.rs` の `log_failure` パターン)。
- コミット前に `cargo fmt` / `cargo clippy --all-targets -- -D warnings` /
  `cargo test` を通してください (compiler warning ゼロを維持)。
- 依存クレートは必要最小限に保ち、追加時は「なぜ必要か」を説明できる状態に
  します (docs/decisions.md の D6 を参照)。

## アーキテクチャ

VeloX は wry (システム WebView) + tao で構成される軽量デスクトップブラウザです。
責務は 4 層に分離されています:

- `src/ui/` — ウィンドウ・レイアウト・ツールバー (専用 WebView 内の自前 HTML)
- `src/app.rs` — イベントループ。全状態変更はメインスレッドの `UserEvent`
  ディスパッチに集約 (ロックなし)
- `src/browser/` — UI/エンジン非依存の純粋ロジック (URL 正規化・`Tab` 状態)。
  単体テストの主対象
- `src/config/` — 起動設定

詳細は [docs/architecture.md](docs/architecture.md) を、技術選定の判断理由
(wry vs Servo など) は [docs/decisions.md](docs/decisions.md) を参照してください。
設計判断を変更・追加した場合は docs/decisions.md にも記録します。
