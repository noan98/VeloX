# VeloX ベンチマークスイート

Issue #14 の成果物。VeloX 自身の最適化効果や、将来的な Chrome/Firefox 等との
比較を定量評価するための、ベンチマーク条件・実行方法・結果フォーマットを
記録する。Issue #13 が実装した計測基盤 (`src/browser/metrics.rs` /
`src/browser/perf_log.rs`、`docs/architecture.md` の「Performance extension
points」節) が生成する JSON Lines を、このスイートが回収・集計・比較する。

このドキュメントは #53 (Phase 2 Epic) の「高速化を感覚で判断しない」方針の
"Measure / Compare" 部分の仕様であり、後続の #36 (CI へのパフォーマンス回帰
検知の導入) が呼び出す前提のインターフェースでもある。

## 構成

集計・比較ロジックとランナーは明確に分離している。

| 場所 | 役割 | 検証方法 |
|------|------|----------|
| `src/browser/benchmark.rs` | JSON Lines の解析、統計量算出 (中央値/p95 等)、複数試行の集約、2 つの保存結果の比較。UI/WebView/OS に非依存の純粋な Rust ロジック | `cargo test` で完全にカバー (ヘッドレス環境でも実行できる) |
| `src/bin/velox-bench.rs` | 実際に `velox` バイナリを起動してログを収集し、`benchmark.rs` に渡す薄い IO 層。`run` / `aggregate` / `compare` / `list-scenarios` サブコマンドを持つ CLI | **GUI (WebView) を起動できる環境でのみ動作を確認できる。この開発環境・CI はヘッドレスのため `run` サブコマンドは検証できていない** (下記「実行環境要件」参照) |
| `scripts/bench/pages/*.html` | 固定テストページ (ネットワーク非依存のローカル HTML) | — |

新規依存クレートは追加していない。既存の `serde` / `serde_json` のみを使用する
(CLI 引数パースも `Config::from_env_and_args` と同じ方針で、クレートを足さず
手書きしている。docs/decisions.md D6 参照)。

## 計測するシナリオ

`velox-bench list-scenarios` が一覧を表示する。Issue #14 の実装内容の各項目に
対応する:

| シナリオ ID | Issue #14 の項目 | 自動実行 (`run`) |
|---|---|---|
| `cold_startup` | cold startup | 可 |
| `warm_startup` | warm startup | 可 |
| `first_page_load` | first page load | 可 |
| `navigation` | navigation latency | 不可 (手動 / 将来の自動化) |
| `tab_create` | tab creation | 不可 (手動 / 将来の自動化) |
| `tab_switch` | tab switching | 不可 (手動 / 将来の自動化) |
| `tabs_1` / `tabs_5` / `tabs_10` / `tabs_20` / `tabs_50` | 1/5/10/20/50 tabs でのメモリ/CPU使用量 | 不可 (手動 / 将来の自動化) |

「自動実行」列の意味は `src/browser/benchmark.rs` の
`scenario::Scenario::is_unattended` を参照。VeloX には外部からタブ作成や
アドレスバー入力を駆動する CLI/IPC が (この Issue の時点では) 存在しないため、
`run` サブコマンドが完全に無人で実行できるのは「起動して待つだけ」で済む
起動系シナリオだけである。それ以外は手動 (または将来 Issue で追加される
自動操作) でログを収集し、`velox-bench aggregate` に渡す。

### 計測される生データとの対応

すべて `docs/architecture.md`「Output format and destination」の JSON Lines
スキーマに対応する (`velox` を `VELOX_PERF_METRICS=1 VELOX_PERF_FORMAT=json`
で起動して得られる行)。

| メトリクス名 (`BenchmarkResult.metrics` のキー) | 由来イベント | フィールド |
|---|---|---|
| `startup_window_created_ms` | `startup` | `window_created_ms` |
| `startup_toolbar_ready_ms` | `startup` | `toolbar_ready_ms` |
| `startup_first_load_ms` | `startup` | `first_load_ms` |
| `page_load_ms` | `page_load` | `duration_ms` |
| `tab_create_ms` | `tab_create` | `duration_ms` |
| `tab_switch_ms` | `tab_switch` | `duration_ms` |
| `rss_total_bytes` | `rss` | `total_rss_bytes` |
| `rss_process_count` | `rss` | `process_count` |

`cold_startup` / `warm_startup` / `first_page_load` はいずれも同じ `startup`
イベントの 3 フィールドを見ている。「cold」と「warm」の違いはランナー側の
運用手順 (下記) であり、記録されるイベント自体は同じ形。

### 固定テストページ

`scripts/bench/pages/` に、ネットワークに依存しないローカル固定ページを
3 種類置いている (`file://` で開く)。`navigation` シナリオや将来の自動化で
使う想定。

- `minimal.html` — ほぼ空の最小ページ (ベースライン)
- `text.html` — 200 段落のテキスト中心ページ
- `dom_heavy.html` — 5000 個の `<div>` を持つ DOM 高負荷ページ

固定内容の静的 HTML なので、実行するたびに内容が変わらず、同一条件での
比較に使える。

## 試行回数・代表値・比較の方針

- **試行回数**: 既定 10 回 (`velox-bench run --trials 10`)。少なくとも 2〜3 回
  は必ず複数回実行し、1 回のノイズで判断しない。試行回数 1 でも動作する
  (中央値・p95 は 1 点のときはその値そのものになる。`compute_stats` の単体
  テスト `compute_stats_of_a_single_value` 参照)。
- **代表値**: `count` / `min` / `max` / `mean` / `median` / `p95` / `stddev`
  を保存する (`benchmark::Stats`)。比較 (`velox-bench compare`) は
  **中央値 (median)** を主指標として使う — p95 は共有 CI ランナーのような
  ノイズの多い環境ではテール側が揺れやすく、閾値判定には不向きなため
  (詳細は `docs/decisions.md` D21)。`p95` は `Stats` に含まれているので、
  レビュー時に手動で見比べることはできる。
- **cold / warm の区別**: 「cold」= プロセスやマシンを再起動した直後の 1 回目
  相当。「warm」= 直前に一度起動した後の 2 回目以降。この開発環境は OS の
  ページキャッシュを能動的にクリアする手段を持たないため、「完全なコールド
  キャッシュ」は保証できない — `cold_startup` は「このプロセスグループでの
  最初の起動」を近似値として使う運用とし、`warm_startup` はその後の
  繰り返し起動を使う。将来より厳密なコールド計測が必要になった場合は
  再検討する。
- **外れ値**: 自動では除外しない。`median`/`p95` は外れ値に強い代表値として
  そのまま使い、`mean`/`stddev` で外れ値の影響を確認できるようにしている
  (`compute_stats_handles_an_outlier_without_panicking` 参照)。

## 実行方法

### 1. ビルド

```sh
cargo build --release
```

`velox` と `velox-bench` の両方が `target/release/` にビルドされる
(`src/bin/velox-bench.rs` は `cargo` が `src/bin/*.rs` を自動的に個別バイナリ
として認識するため、`Cargo.toml` の編集は不要)。

### 2. 起動系シナリオを自動実行する (`run`)

```sh
cargo run --release --bin velox-bench -- run \
  --scenario cold_startup \
  --trials 10 \
  --output results/cold_startup-$(date +%Y%m%d).json
```

- `--velox-bin <path>`: 起動する `velox` バイナリのパス。省略時は
  `velox-bench` 自身の実行ファイルと同じディレクトリの `velox` (Windows は
  `velox.exe`) を使う。
- `--warmup-secs <秒>`: 1 試行あたりプロセスを起動してから終了させるまでの
  待ち時間。既定 5 秒 (`startup` イベントが記録されるのに十分な時間)。
- `--rss-interval-ms <ms>`: `VELOX_PERF_RSS_INTERVAL_MS` を子プロセスに渡す。
- `--git-commit <sha>`: 保存する `environment.git_commit` を明示的に指定
  (省略時はカレントディレクトリで `git rev-parse HEAD` を試みる)。

**`navigation` / `tab_create` / `tab_switch` / `tabs_*` を `run` に渡すと、
「手動で収集して `aggregate` に渡してください」というエラーで即座に失敗する
(exit code 2)。** これらのシナリオを外部から無人で駆動する仕組みはこの
Issue の範囲では実装していない。

### 3. 手動収集したログを集計する (`aggregate`)

`navigation` や `tabs_N` のように手動操作が要るシナリオでは、実際に VeloX を
`VELOX_PERF_METRICS=1 VELOX_PERF_FORMAT=json VELOX_PERF_OUTPUT=<path>` 付きで
起動し、想定の操作 (例: `tabs_5` ならタブを 5 個開いた状態を数秒維持する、
`navigation` なら `scripts/bench/pages/` のページ間を数回移動する) をした後に
終了し、そのログファイルを 1 試行分として渡す。試行回数分だけログファイルを
用意し、`--input` を繰り返し指定する:

```sh
VELOX_PERF_METRICS=1 VELOX_PERF_FORMAT=json \
  VELOX_PERF_OUTPUT=/tmp/velox-tabs5-trial1.jsonl \
  VELOX_PERF_RSS_INTERVAL_MS=1000 \
  cargo run --release   # ここでタブを5個開いた状態を維持してから終了する

cargo run --release --bin velox-bench -- aggregate \
  --scenario tabs_5 \
  --input /tmp/velox-tabs5-trial1.jsonl \
  --input /tmp/velox-tabs5-trial2.jsonl \
  --output results/tabs_5-$(date +%Y%m%d).json
```

`run` も内部的にはこの `aggregate` と同じ集計コードパス
(`benchmark::aggregate_trials`) を通る。

### 4. 過去の結果と比較する (`compare`)

```sh
cargo run --release --bin velox-bench -- compare \
  --baseline results/cold_startup-20260801.json \
  --candidate results/cold_startup-20260901.json \
  --threshold-pct 10 \
  --output results/cold_startup-diff.json
```

- `--threshold-pct`: 中央値がこの割合 (%) を超えて悪化していたら回帰と判定
  する。既定 10。
- 終了コード: 回帰なしなら `0`、いずれかの指標が閾値を超えて悪化していれば
  `1`。**この終了コードが Issue #36 の CI 回帰検知が使うフックになる。**
- `--baseline` と `--candidate` は同じ `scenario` かつ同じ OS
  (`environment.os`) の結果同士を比較すること。OS が異なる環境の結果を
  比較しても `compare` はエラーにはしない (意図的な比較を妨げないため) が、
  WebView 実装・メモリ管理が OS ごとに異なる (`docs/architecture.md`) ため
  数値の意味が異なり、比較として無意味になる。

## 結果ファイルのフォーマット

`velox-bench run` / `aggregate` が書き出す JSON (`BenchmarkResult`) の例:

```json
{
  "scenario": "cold_startup",
  "environment": {
    "os": "linux",
    "cpu_count": 4,
    "git_commit": "a60b710873b9d8074723276f73781d7d5c03fc",
    "generated_at": "2026-09-01T13:15:42Z",
    "trials": 10
  },
  "metrics": {
    "startup_first_load_ms": {
      "count": 10,
      "min": 108.2,
      "max": 134.5,
      "mean": 118.9,
      "median": 116.4,
      "p95": 131.0,
      "stddev": 7.8
    }
  }
}
```

`environment` が受け入れ条件の「実行環境情報 (OS、CPUコア数、VeloXのgit
commit、実行日時、試行回数)」に対応する。`metrics` はレコードが 1 件も無い
メトリクスは省略される (存在しない項目をゼロ扱いにしない — 詳細は
`compute_stats` のドキュメントコメント)。

`velox-bench compare --output` が書き出す JSON (`ComparisonReport`) の例:

```json
{
  "baseline_scenario": "cold_startup",
  "candidate_scenario": "cold_startup",
  "threshold_pct": 10.0,
  "diffs": {
    "startup_first_load_ms": {
      "baseline_median": 115.0,
      "candidate_median": 145.0,
      "delta": 30.0,
      "pct_change": 26.08695652173913,
      "regressed": true
    }
  },
  "only_in_baseline": [],
  "only_in_candidate": [],
  "any_regressed": true
}
```

## 実行環境要件

- `run` サブコマンドの起動系シナリオ (`cold_startup` / `warm_startup` /
  `first_page_load`) は、実際に VeloX のウィンドウを作成できる環境が必要
  (Linux は `DISPLAY` が使える X11/Wayland セッション、WebKitGTK が動作する
  環境。macOS/Windows はそれぞれ通常のデスクトップセッション)。
- **この開発環境・CI コンテナはヘッドレスで、GUI をまったく起動できない。**
  `Xvfb` 等の仮想ディスプレイがあれば動く可能性はあるが、この環境にはなく、
  導入もしていない。前提条件としては書かない (Issue #14 のタスキング指示
  通り)。
- ヘッドレスで `velox-bench run` を実行すると、子プロセス (`velox`) が
  ウィンドウ作成に失敗して即座に終了する (この環境では GTK 初期化失敗の
  panic として観測した)。`velox-bench` 自身はクラッシュせず、
  「0 件のレコードを取得」「結果ファイルは書き出したが metrics は空」という
  明確な警告を出し、終了コード `1` で終わることを確認済み — つまり
  **ヘッドレス環境で `cargo run --bin velox-bench -- run ...` を実行しても
  安全に失敗する** (壊れた結果ファイルを本物の計測結果として誤って保存する
  ことはない)。
- `aggregate` / `compare` / `list-scenarios` はディスプレイ不要で、この開発
  環境でも実際に動作確認済み (下記「この環境での検証状況」参照)。

## この環境での検証状況 (正直な記録)

- `src/browser/benchmark.rs` の集計・比較ロジック: `cargo test` で完全に
  検証済み (JSON Lines の正常系/壊れた行/空入力/1 試行のケースを含む)。
- `velox-bench aggregate` / `compare` / `list-scenarios`: 実際にビルドして
  合成した JSON Lines ログを与え、想定通りの集計結果・比較結果・終了コード
  (回帰時 1、非回帰時 0) になることを手動で確認した。
- `velox-bench run`: ヘッドレス環境のため、実際に VeloX を起動しての
  cold/warm startup 計測そのものは検証できていない。上記の通り「起動失敗を
  クラッシュせず処理できること」は確認したが、**実マシンでの起動時間の
  実測値は未取得**。GUI が使える環境 (開発者のローカルマシン、または将来
  Xvfb 等を導入した CI) で改めて実測する必要がある。
- `navigation` / `tab_create` / `tab_switch` / `tabs_1..50`: シナリオ定義・
  データモデル・集計コードパスは実装・テスト済みだが、実測 (ログの収集)
  そのものは行っていない — タブ操作やナビゲーションを外部から無人で駆動
  する仕組みが存在しないため、この Issue の範囲では未実施。手動運用手順は
  上記「3. 手動収集したログを集計する」に記載した。

## 既知の制約・将来の拡張

- タブ作成・タブ切り替え・ナビゲーションを外部から自動的に駆動する仕組みが
  ない。実現するには `app.rs`/toolbar IPC に何らかの自動操作用フック
  (例: 起動時に開くタブ数や URL 一覧を指定する CLI フラグ) を追加する必要が
  あり、今回のタスキングでは `src/app.rs` 等への変更が禁止されている
  (Issue #12 の並行作業との衝突回避) ため見送った。将来の Issue で検討する。
- `Xvfb` 等の仮想ディスプレイを使えば、この開発環境でも `run` の起動系
  シナリオを実行できる可能性がある。ただし本 Issue の指示に従い、前提条件
  としては明記していない。
