# Performance Decisions

性能計測・ベンチマーク・メモリ削減・プロファイリング・回帰検知に関する Decision の入口です。

## 対象

- **D16** — `/proc` ベースの metrics
- **D19** — tab latency / structured perf output
- **D21** — benchmark suite
- **D41–D46** — competitive benchmarking / PSS / startup 分解 / automation / profiling / regression gate
- **D47–D49** — integration test / memory root cause / WebContext sharing
- **D56–D58** — adaptive suspension / WebProcess sharing 上限 / background CPU

## 補助ドキュメント

- [`../benchmarking.md`](../benchmarking.md)
- [`../performance-targets.md`](../performance-targets.md)
- [`../memory-analysis.md`](../memory-analysis.md)
- [`../profiling.md`](../profiling.md)
- [`../performance-dashboard.md`](../performance-dashboard.md)

## 詳細

- [D16](./archive.md#d16-performance-metrics--proc-directly-no-new-dependency)
- [D41–D49](./archive.md#d41-competitive-benchmarking-measures-an-external-load-beacon-and-compares-memory-by-pss)
- [D56–D58](./archive.md#d56-adaptive-tab-suspension--3-signals)
