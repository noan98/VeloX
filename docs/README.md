# VeloX ドキュメント索引

`docs/` に置かれている文書の一覧です。どれを読めばよいかを最初に判断するための
索引で、内容そのものは各文書にあります。

## 設計

| 文書 | 内容 |
| --- | --- |
| [architecture.md](architecture.md) | UI (`src/ui/`) / Application (`src/app.rs`) / Browser logic (`src/browser/`) / Web engine (wry) という 4 層の責務分担と依存方向。UI 側は状態を直接触らず `UserEvent` を投げる、という設計もここ (`src/config/` は `src/browser/` と同じく純粋 Rust の補助モジュールで、層には数えない) |
| [extensions.md](extensions.md) | 拡張機能の設計 (Issue #82 / Epic #80): 信頼境界・脅威モデル・権限モデル・ライフサイクル・マニフェストスキーマ。悪意ある拡張機能を前提にした決定は D159。スキーマの実装は `src/browser/extension_manifest.rs`、API / ライフサイクル / ストレージのホスト側コアは `src/browser/extensions/` (#83 / #84、D163。いずれも未配線) |
| [sync.md](sync.md) | 複数端末同期の設計仕様 (同期対象・HLC・競合解決・オフラインキュー・プロトコルバージョン・脅威モデル)。実装は `src/browser/sync/` (アプリ未接続)、暗号化とサーバは #86 |
| [decisions/README.md](decisions/README.md) | 設計判断の入口。D 番号をテーマ別に整理し、詳細な一次記録は `decisions/archive.md` に保持 |

## 性能

Phase 3 (Epic #57) の成果物です。**計測結果は OS ごとに分けて記録する**という
原則があるため、数値を読むときは必ずどの OS のものかを確認してください。

| 文書 | 内容 |
| --- | --- |
| [performance-targets.md](performance-targets.md) | 測定環境の定義、性能目標 T1〜T4、これまでの計測結果 |
| [benchmarking.md](benchmarking.md) | `velox-bench` の使い方と、計測を無効にしてしまう落とし穴 |
| [profiling.md](profiling.md) | 性能問題を見つけたあとに原因を掘るための perf / heaptrack の手順 |
| [memory-analysis.md](memory-analysis.md) | メモリフットプリントがどこに消えているかの切り分け |
| [performance-dashboard.md](performance-dashboard.md) | 計測結果を時系列で追跡する仕組み |

## リリース

| 文書 | 内容 |
| --- | --- |
| [windows-code-signing.md](windows-code-signing.md) | コード署名と配布信頼性の調査結果、および現状の実装 |
| [releasing.md](releasing.md) | タグから GitHub Release を作る手順、リリースノートの生成、手作業・未対応の項目 |
| [user-guide.md](user-guide.md) | 利用者向け: インストール・更新・アンインストール・トラブルシューティング・問い合わせ窓口 |

## この索引以外の入り口

- リポジトリ全体の紹介と機能一覧は [../README.md](../README.md)
- Claude Code 向けの作業ガイドラインは [../CLAUDE.md](../CLAUDE.md)
