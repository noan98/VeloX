# VeloX ドキュメント索引

`docs/` に置かれている文書の一覧です。どれを読めばよいかを最初に判断するための
索引で、内容そのものは各文書にあります。

## 設計

| 文書 | 内容 |
| --- | --- |
| [architecture.md](architecture.md) | 4 層 (`ui` / `app` / `browser` / `config`) の責務分担と、全状態変更をメインスレッドの `UserEvent` ディスパッチに集約する設計 |
| [decisions.md](decisions.md) | 設計判断の記録。「なぜそうしたか」「なぜそうしなかったか」を D 番号ごとに残す。実装方針を変えたときはここに追記する |

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

## この索引以外の入り口

- リポジトリ全体の紹介と機能一覧は [../README.md](../README.md)
- Claude Code 向けの作業ガイドラインは [../CLAUDE.md](../CLAUDE.md)
