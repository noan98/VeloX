# VeloX ベンチマークスイート

> **競合ブラウザとの比較・性能目標は [docs/performance-targets.md](performance-targets.md)
> を参照。** この文書は VeloX 自身の内部計測 (`velox-bench`) を扱う。**メモリを
> ブラウザ間で比較するときは RSS 合計ではなく PSS を使うこと** — 理由と実測での
> 逆転例は D41 と performance-targets.md §3.1 にある。

Issue #14 の成果物。VeloX 自身の最適化効果や、将来的な Chrome/Firefox 等との
比較を定量評価するための、ベンチマーク条件・実行方法・結果フォーマットを
記録する。Issue #13 が実装した計測基盤 (`src/browser/metrics.rs` /
`src/browser/perf_log.rs`、`docs/architecture.md` の「Performance extension
points」節) が生成する JSON Lines を、このスイートが回収・集計・比較する。

このドキュメントは #53 (Phase 2 Epic) の「高速化を感覚で判断しない」方針の
"Measure / Compare" 部分の仕様であり、#36 (CI へのパフォーマンス回帰検知の
導入) を Phase 3 の実運用レベルまで発展させた #72 (Performance Regression
Gate) が呼び出す前提のインターフェースでもある。CI が実際にブロッキング
判定へ使うのは「4. 過去の結果と比較する (`compare`)」ではなく「5. 回帰
ゲートを評価する (`gate`)」— 理由は当該節と `docs/decisions.md` D46 参照。

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
| `pss_total_bytes` | `rss` | `total_pss_bytes` |
| `pss_process_count` | `rss` | `pss_process_count` |

**`pss_total_bytes` (Issue #108 / D42), not `rss_total_bytes`, is the metric
to use when comparing memory footprint across builds or against another
browser.** RSS sums each process's resident pages, so it double-counts
shared memory once per process — a build/browser with more helper processes
looks heavier by RSS even at equal real memory use (`docs/performance-
targets.md` §3.1 has a measured case where this flips which of two browsers
looks lighter). `pss_total_bytes` is absent from a trial's aggregated
metrics when PSS could not be read for any process (old kernel, permissions,
non-Linux) — `rss_total_bytes` still is present in that case, since RSS has
no such platform gap. `pss_process_count` says how many processes
contributed to the PSS sum, out of `rss_process_count` total; less than
`rss_process_count` (but present) means the sum is real but incomplete, not
wrong.

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
# ローカル固定ページを配信しておく (再現性のため)
(cd scripts/bench/pages && python3 -m http.server 8731 &)

cargo run --release --bin velox-bench -- run \
  --scenario cold_startup \
  --trials 10 \
  --url http://127.0.0.1:8731/minimal.html \
  --output results/cold_startup-$(date +%Y%m%d).json
```

- **`--url <URL>`**: 各試行で VeloX に読み込ませるページ。`VELOX_HOMEPAGE`
  として子プロセスへ渡す (Issue #106、D40)。**省略すると VeloX の既定
  ホームページ (`https://www.google.com/`) を計測することになり、外部
  ネットワークに依存して再現性が失われる**ため、`velox-bench` は未指定時に
  警告を出す。比較可能な数値を取るには `scripts/bench/pages/` の固定ページを
  loopback 経由で指定すること。
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
  `1`。
- `--baseline` と `--candidate` は同じ `scenario` かつ同じ OS
  (`environment.os`) の結果同士を比較すること。OS が異なる環境の結果を
  比較しても `compare` はエラーにはしない (意図的な比較を妨げないため) が、
  WebView 実装・メモリ管理が OS ごとに異なる (`docs/architecture.md`) ため
  数値の意味が異なり、比較として無意味になる。

**`compare` は単発の 2 ファイル diff であり、CI の合否判定には使わない。**
`compare` の単純な固定閾値は、この環境のセッション間ノイズ (同一バイナリで
中央値が最大 -19.0%〜+78.9% 動く。`docs/decisions.md` D46) の前では
`--threshold-pct` をいくつに設定しても誤検知を避けられない。CI が実際に
使うのは次の `gate` サブコマンドである。手元で 2 つの結果ファイルをさっと
見比べたいとき (ノイズを気にせず数値だけ見たいとき) には引き続き
`compare` が便利。

### 5. 回帰ゲートを評価する (`gate`, Issue #72)

**Issue #36/#72 の「性能回帰検知」を CI で実際にブロッキング判定できる
形にしたサブコマンド。** ロジックは `benchmark::evaluate_gate`
(`src/browser/benchmark.rs`、純粋 Rust、`cargo test` で境界値・同着・試行数
不足・baseline 欠損を含めて検証済み)。**なぜ `compare` の単純な固定閾値では
足りないか、どう解決したかは `docs/decisions.md` D46 と
`docs/performance-targets.md` §10 を参照。**

```sh
cargo run --release --bin velox-bench -- gate \
  --baseline results/baseline/cold_startup-linux-xvfb.json \
  --candidate results/cold_startup-candidate-1.json \
  --candidate results/cold_startup-candidate-2.json \
  --warn-pct 20 --fail-pct 60 \
  --output results/gate-report.json \
  --markdown-output results/gate-summary.md
```

- `--baseline <path>`: 比較の基準になる結果ファイル (1 つ)。
- `--candidate <path>`: 比較対象の結果ファイル。**繰り返し指定できる。**
  2 つ以上渡すと多数決 (過半数が `fail_pct` を超えたときのみ Fail) が働く
  — 1 回だけの悪化は Warn に留まる (D46)。CI では PR head を 2 回計測して
  渡す運用にしている (`.github/workflows/perf-gate.yml`)。
- `--warn-pct` / `--fail-pct`: 既定 20 / 60。**単なる固定閾値ではなく、**
  各メトリクスにはこれとは別にメトリクス種別ごとの最小絶対差
  (`MetricKey::min_significant_delta`) が併用される — 相対変化率だけでは
  拾えない「絶対値がほぼゼロなのに % だけ跳ねる」ケースを吸収する。
- `--output` / `--markdown-output`: それぞれ機械可読 JSON
  (`benchmark::GateReport`) と、GitHub Actions の Job Summary /
  PR コメントにそのまま貼れる Markdown テーブルを書き出す。
- **終了コード**: `0` = OK、`3` = WARN (非ブロッキング — 人間が確認する
  価値はあるが CI は失敗させない)、`1` = FAIL (CI をブロックすべき)、
  それ以外の `2` は引数エラー等 (既存サブコマンドと同じ規約)。
  **WARN に専用のコードを割ったのは `compare` の 0/1 の 2 値では
  「ノイズかもしれないので確認してほしい」と「確実に回帰している」を
  CI 上で区別できないため。**

**baseline に何を渡すべきか**: リポジトリに `results/baseline/` として
コミットされている結果ファイルを直接 CI のブロッキング判定に使っては
ならない (§10 参照 — 機械/セッションが変わる比較はこの環境では成立しない
ことが実測済み)。CI (`perf-gate.yml`) は baseline も **同じジョブ内で**
PR の merge-base コミットをその場でビルド・計測して作る。コミット済みの
`results/baseline/*.json` は経時トレンドを人が目視するための参考情報。

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
- **`Xvfb` があれば、ディスプレイのない環境でも実測できる** (Issue #106 で
  実際に確認済み)。

  ```sh
  sudo apt install -y xvfb            # Debian/Ubuntu
  xvfb-run -a --server-args="-screen 0 1280x900x24" \
    cargo run --release --bin velox-bench -- run --scenario cold_startup ...
  ```

  GPU が無い環境では WebKitGTK がソフトウェアレンダリングにフォールバック
  する (`libEGL warning: DRI3 error` が出る)。**この状態の RSS は実機より
  大きく出るため、メモリの絶対値を実機の基準値として扱わないこと。**
- 仮想ディスプレイすら無い場合は、子プロセス (`velox`) がウィンドウ作成に
  失敗して即座に終了する (GTK 初期化失敗の panic として観測)。`velox-bench`
  自身はクラッシュせず、「0 件のレコードを取得」「結果ファイルは書き出したが
  metrics は空」という警告を出し、終了コード `1` で終わる — つまり
  **ディスプレイのない環境で実行しても安全に失敗する** (壊れた結果ファイルを
  本物の計測結果として誤って保存することはない)。
- `aggregate` / `compare` / `list-scenarios` はディスプレイ不要で、この開発
  環境でも実際に動作確認済み (下記「この環境での検証状況」参照)。

## この環境での検証状況 (正直な記録)

- `src/browser/benchmark.rs` の集計・比較ロジック: `cargo test` で完全に
  検証済み (JSON Lines の正常系/壊れた行/空入力/1 試行のケースを含む)。
- `velox-bench aggregate` / `compare` / `list-scenarios`: 実際にビルドして
  合成した JSON Lines ログを与え、想定通りの集計結果・比較結果・終了コード
  (回帰時 1、非回帰時 0) になることを手動で確認した。
- `velox-bench run`: **Issue #106 で `Xvfb` を用いた実測に成功した。**
  `--url` で loopback 上の `minimal.html` を指定し、`cold_startup` を 3 試行
  実行した初回の結果 (4 コア・GPU 無しのコンテナ、ソフトウェアレンダリング):

  | metric | median | p95 |
  | --- | ---: | ---: |
  | `startup_window_created_ms` | 224.40 | 226.38 |
  | `startup_toolbar_ready_ms` | 528.40 | 643.51 |
  | `startup_first_load_ms` | 665.25 | 867.62 |
  | `page_load_ms` | 77.50 | 127.18 |
  | `rss_total_bytes` | 201273344 | 813502464 |

  **これは正式なベースラインではない**。(a) ソフトウェアレンダリングのため
  RSS が実機より大きい、(b) 3 試行のうち 1 試行は `--warmup-secs 6` では
  起動が間に合わず 1 レコードしか取れなかった (startup 系の `n` が 2 に
  なっている)、(c) 競合ブラウザとの比較条件が未確定 (#58)。正式な目標値と
  比較条件は #58 で定める。
- `navigation` / `tab_create` / `tab_switch` / `tabs_1..50`: シナリオ定義・
  データモデル・集計コードパスは実装・テスト済みだが、実測 (ログの収集)
  そのものは行っていない — タブ操作やナビゲーションを外部から無人で駆動
  する仕組みが存在しないため、この Issue の範囲では未実施。手動運用手順は
  上記「3. 手動収集したログを集計する」に記載した。

## 既知の制約・将来の拡張

- タブ作成・タブ切り替え・ナビゲーションを外部から自動的に駆動する仕組みが
  まだ無い。Issue #106 で**起動時の URL 指定 (`--homepage` / `VELOX_HOMEPAGE`)
  までは入った**ので `first_page_load` は自動化できるが、「起動時に N 個の
  タブを開く」「起動後に別ページへ遷移する」に相当するフックは未実装。
  `tabs_1..50` / `navigation` / `tab_create` / `tab_switch` の自動実行には
  引き続き追加のフックが必要で、`Scenario::is_unattended()` はこれらを
  `false` のままにしている。
- ~~`Xvfb` 等の仮想ディスプレイを使えば `run` の起動系シナリオを実行できる
  可能性がある~~ → **Issue #106 で実証済み。** 上記「実行環境の前提」の
  `xvfb-run` の項を参照。
