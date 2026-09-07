# VeloX ベンチマークスイート

> **競合ブラウザとの比較・性能目標は [docs/performance-targets.md](performance-targets.md)
> を参照。** この文書は VeloX 自身の内部計測 (`velox-bench`) を扱う。**メモリを
> ブラウザ間で比較するときは RSS 合計ではなく PSS を使うこと** — 理由と実測での
> 逆転例は D41 と performance-targets.md §3.1 にある。
>
> **このスイートやベンチマークで「遅い/重い」を検出した後、原因のコード箇所まで
> 掘り下げる手順は [docs/profiling.md](profiling.md) を参照。** ここでの
> 役割分担は: 本文書と `compare_browsers.py` が「どのくらい悪いか」を数値で
> 検出し、`docs/profiling.md` (`perf`/`heaptrack`/`scripts/profile/`) が
> 「どこのコードが原因か」を特定する。

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
| `src/bin/velox-bench.rs` | 実際に `velox` バイナリを起動してログを収集し、`benchmark.rs` に渡す薄い IO 層。`run` / `aggregate` / `compare` / `list-scenarios` サブコマンドを持つ CLI | **GUI (WebView) を起動できる環境が必要。この開発環境・CI はヘッドレスのため `Xvfb` (下記「実行環境要件」) を経由してのみ検証している** — Issue #106/#112 で `Xvfb` 上の全シナリオの `run` 実行に成功済み |
| `src/browser/automation.rs` | `VELOX_AUTOMATION_SCRIPT` の行区切りスクリプトのパース、`velox-bench run` 用スクリプト生成 (`generate_bench_script`)。UI/WebView/OS に非依存の純粋な Rust ロジック (Issue #112、docs/decisions.md D44) | `cargo test` で完全にカバー |
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
| `navigation` | navigation latency | 可 (`--url` 必須、下記「自動操作フック」参照) |
| `tab_create` | tab creation | 可 (`--url` 必須) |
| `tab_switch` | tab switching | 可 (`--url` 必須) |
| `tab_resume` | 休止タブの復帰コスト (Issue #63: `suspend` → `switch` を繰り返し、`tab_resume_ms` と復帰時の再読み込み `page_load_ms` を採る) | 可 (`--url` 必須) |
| `background_cpu` | バックグラウンドタブの CPU 消費 (Issue #64)。`busy.html` を開いてから `?idle=1` 版を新しいタブで開き、busy 側をバックグラウンドに送って測る。`--url` には `busy.html` を渡す | 可 (`--url` 必須) |
| `tab_create_1` / `_5` / `_10` / `_20` / `_50` | **N タブ開いた状態で**もう 1 つタブを作るコスト (Issue #60)。N タブまで開いてから `mark` し、以降「1 つ開いて閉じる」を 8 回繰り返すので、`tab_create_ms` のサンプルはすべてタブ数 N で採られる | 可 (`--url` 必須) |
| `tab_switch_1` / `_5` / `_10` / `_20` / `_50` | **N タブ開いた状態で**のタブ切替コスト (Issue #60)。同様に `mark` 後の 8 回の `switch` だけを測る | 可 (`--url` 必須) |
| `tabs_1` / `tabs_5` / `tabs_10` / `tabs_20` / `tabs_50` | 1/5/10/20/50 tabs でのメモリ/CPU使用量 | 可 (`--url` 必須) |

「自動実行」列の意味は `src/browser/benchmark.rs` の
`scenario::Scenario::is_unattended` を参照。**Issue #112 より前は、VeloX に
外部からタブ作成やナビゲーションを駆動する仕組みが存在しなかったため、`run`
サブコマンドが完全に無人で実行できるのは「起動して待つだけ」で済む起動系
シナリオだけだった。** #112 で追加された `VELOX_AUTOMATION_SCRIPT` フック
(下記「自動操作フック (`VELOX_AUTOMATION_SCRIPT`)」参照) により、`run` は
残り全シナリオについても自動操作スクリプトを生成して VeloX に渡すようになり、
現在は `is_unattended` が全シナリオで `true` を返す。起動系 3 シナリオ以外は
自動操作スクリプトを組み立てるために実際のページ URL が要る (「measure
whatever the default homepage is」のような無条件フォールバックは、比較可能な
数値を得るという目的に反する) ため、`--url` を省略すると `run` はエラーで
即座に失敗する (exit code 2)。手動でログを収集して `velox-bench aggregate`
に渡す経路は引き続き利用できる (シナリオを問わず)。

### 自動操作フック (`VELOX_AUTOMATION_SCRIPT`)

Issue #112 の成果物。VeloX は起動時に環境変数 `VELOX_AUTOMATION_SCRIPT=<path>`
が設定されていると、そのファイルを**一度だけ**読み込み、パースして、タブの
開閉・切替・ナビゲーション・待機・終了を自動的に行う。**これは常設の待ち受け
ソケットや RPC サーバではない** — 環境変数が無ければパーサ自体が一切動かず、
通常起動に追加のコストや攻撃面を持ち込まない。設計判断の詳細は
docs/decisions.md D44 (D18/D23 の IPC 信頼境界との関係) を参照。

パーサ本体は `src/browser/automation.rs`
(`browser::automation::parse_script`) — UI/WebView/OS に非依存の純粋な Rust
ロジックで、`cargo test` から完全にカバーされる。実際にタブを開いたり
切り替えたりする側 (`UserEvent::Automation` として既存のメインスレッド
ディスパッチに流し、`ToolbarCommand`/`ContentShortcut` と同じタブ操作関数を
呼ぶだけで、新しい状態変更経路は作っていない) は `src/app.rs` にある。

**スクリプト形式**: 1 行 1 コマンド、上から順に実行される。`#` で始まる行と
空行は無視される。

```
open <url>        # 新規タブを開いてアクティブにする
switch <index>    # tab strip 上の position <index> (0始まり) のタブをアクティブにする
close <index>     # position <index> のタブを閉じる
suspend <index>   # position <index> のタブを休止する (Issue #63。アクティブタブ・休止済みタブには無視される)
mark              # ここまでを準備 (warm-up) として集計から捨てる (Issue #60)
navigate <url>    # アクティブタブを <url> へ遷移させる
wait <ms>         # 次のコマンドまで <ms> ミリ秒待つ (上限 120000ms = automation::MAX_WAIT_MS)
wait_load [timeout_ms]  # アクティブタブの読み込み中のページロードが完了する
                  # まで待つ (Issue #169、docs/decisions.md D84)。既に読み込み
                  # 完了していれば即座に次へ進む。timeout_ms 省略時は
                  # automation::DEFAULT_WAIT_LOAD_TIMEOUT_MS (10000ms)、
                  # 指定時も上限は wait と同じ MAX_WAIT_MS。超過すると
                  # 何を待っていたかを stderr に出してタイムアウトし、次の
                  # コマンドへ進む (ハングしない)
wait_startup [timeout_ms]  # `startup` perf レコードが書き込まれるまで待つ
                  # (Issue #173、docs/decisions.md D85)。`wait_load` が
                  # 見るのは 1 タブの読み込み完了だけだが、`startup`
                  # レコードはそれに加えてツールバー Webview 独自の
                  # `ready` ハンドシェイクも揃わないと書かれない
                  # (`app::mark_startup`/`metrics::StartupTimestamps::
                  # report`) — このコマンドはその全体を待つ。既に書き込み
                  # 済みなら即座に次へ進む。引数・デフォルト・上限・
                  # タイムアウト時の挙動 (ハングしない) は `wait_load` と
                  # 同じ。パフォーマンス計測が無効 (`VELOX_PERF_METRICS`
                  # 未設定) だと `startup` レコード自体が書かれないため、
                  # 待っても何も起きず必ずタイムアウトする
quit              # アプリケーションを終了する
```

`open`/`navigate` の URL はアドレスバー入力と同じ
`browser::navigation::normalize_input` で正規化されるため、`javascript:`
などの危険なスキームはパース時点で拒否される。`switch`/`close` の
`<index>` は実行時点の tab strip 上の位置 (0 始まり) — スクリプトの各行が
実行される順にタブが増減していくので、`close 1` は「その時点で 2 番目に
あるタブ」を指す。範囲外の `index` は panic ではなく無視 (stderr に警告)
される。不正な行 (未知のコマンド・引数欠落・数値パース失敗・`wait`/
`wait_load` の上限超過) はパース時点で全体を拒否し、行番号付きのエラーを
stderr に出す — 一部だけ実行される、ということはない。

手で使う例:

```sh
cat > /tmp/script.txt <<'EOF'
open http://127.0.0.1:8731/minimal.html
wait 500
switch 0
wait 500
close 1
wait 300
quit
EOF

VELOX_AUTOMATION_SCRIPT=/tmp/script.txt \
VELOX_PERF_METRICS=1 VELOX_PERF_FORMAT=json VELOX_PERF_OUTPUT=/tmp/out.jsonl \
  cargo run --release
```

`velox-bench run` は `navigation`/`tab_create`/`tab_switch`/`tab_resume`/`tabs_N` それぞれ
に対して、上記コマンドを組み合わせたスクリプトを自動生成し (`--url` で
渡されたページを使う)、一時ファイルに書き出して子プロセスに
`VELOX_AUTOMATION_SCRIPT` として渡す。生成ロジックは
`browser::automation::generate_bench_script` (純粋関数、`cargo test` で
カバー) — 例えば `tabs_5` なら追加で 4 タブを `open` してから安定するまで
`wait` し、`tab_switch` ならタブを数個開いてから `switch` を繰り返す。生成
されるスクリプトは必ず `quit` で終わるため、`run` は各試行が warmup タイムア
ウトを待たずに自発的に終了するのを検出でき (`wait_for_exit_or_timeout`)、
シナリオごとの目安のタイムアウトは
`browser::automation::recommended_timeout_secs` が決める (`--warmup-secs`
で明示的に上書きできる)。

#### `mark` と warm-up の切り捨て (Issue #60)

`tab_create_20` のように「**N タブ開いた状態での**操作」を測るシナリオは、
測定に入る前に N 個のタブを開く準備フェーズを必ず持つ。この準備で発生する
`tab_create` / `page_load` は「1 タブ時」「2 タブ時」… の値であり、
本来測りたい「20 タブ時」の値と混ぜてしまうと中央値はどちらでもない数字に
なる。

`mark` はその境界を perf ログに `measure_start` イベントとして書き込み、
`velox-bench aggregate` (`benchmark::aggregate_trials`) が**最後の
`measure_start` より後のイベントだけ**を集計対象にする。マーカーが 1 つも
無い試行 (Issue #60 より前からある全シナリオ) は全イベントが対象のままなので、
既存シナリオの数値は変わらない。

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
| `page_load_engine_ms` | `page_load` | `engine_duration_ms` (`LoadStarted → LoadFinished`。black-box、Epic #57 ルール3。Issue #69) |
| `page_load_dispatch_ms` | `page_load` | `dispatch_duration_ms` (`NavigationStarted → LoadStarted`。**VeloX 自身のコストではない** — `docs/decisions.md` D87 参照。Issue #69) |
| `tab_create_ms` | `tab_create` | `duration_ms` |
| `tab_switch_ms` | `tab_switch` | `duration_ms` |
| `tab_resume_ms` | `tab_resume` | `duration_ms` (休止タブへの切替 = webview の再構築。Issue #63) |
| `cpu_percent` | `cpu` | `percent` (直近 2 回の `rss` サンプル間の、プロセスツリー全体の CPU 使用率。1 コアを 100 とする。Issue #64) |
| (集計対象の境界) | `measure_start` | フィールド無し。`mark` コマンドが書き込むマーカーで、これより前のイベントは集計から捨てられる (Issue #60) |
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
4 種類置いている (`file://` で開く)。`navigation` シナリオや将来の自動化で
使う想定。

- `minimal.html` — ほぼ空の最小ページ (ベースライン)
- `text.html` — 200 段落のテキスト中心ページ
- `dom_heavy.html` — 5000 個の `<div>` を持つ DOM 高負荷ページ
- `download.html` — 読み込み完了時に `download` 属性付きリンクを 1 回
  クリックし、data: URL から `velox-test.txt` (20 バイト) をダウンロード
  させるページ。ベンチマーク用ではなく、ダウンロード経路の統合テスト
  (`tests/integration.rs`) と手動再現 (docs/decisions.md D53) 用

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
  loopback 経由で指定すること。`navigation`/`tab_create`/`tab_switch`/
  `tabs_*` は自動操作スクリプトの生成にこの URL を使うため必須 (省略すると
  exit code 2 で即座に失敗する) — 上記「自動操作フック」参照。
- `--velox-bin <path>`: 起動する `velox` バイナリのパス。省略時は
  `velox-bench` 自身の実行ファイルと同じディレクトリの `velox` (Windows は
  `velox.exe`) を使う。
- `--warmup-secs <秒>`: 1 試行あたりプロセスを起動してから (強制) 終了させる
  までの上限。既定値はシナリオごとに異なる
  (`browser::automation::recommended_timeout_secs`) — 起動系 3 シナリオは
  従来どおり 5 秒、自動操作スクリプトを使うシナリオはそのスクリプトが
  余裕を持って完走できる秒数 (`tabs_N` はタブ数に応じて増える)。**自動操作
  スクリプトは必ず `quit` で終わるため、実際にはこの上限より先に子プロセス
  が自発的に終了することが多い** — `run` は `try_wait` でこれを検出し、
  待ち切らずに次の試行へ進む。それでも `--warmup-secs` を明示すれば既定値を
  上書きできる (遅い環境で余裕を持たせたい場合など)。
- `--rss-interval-ms <ms>`: `VELOX_PERF_RSS_INTERVAL_MS` を子プロセスに渡す。
- `--git-commit <sha>`: 保存する `environment.git_commit` を明示的に指定
  (省略時はカレントディレクトリで `git rev-parse HEAD` を試みる)。

### 3. 手動収集したログを集計する (`aggregate`)

`run` はすべてのシナリオを自動実行できるが (上記)、`aggregate` は今も
使える経路として残っている — `VELOX_AUTOMATION_SCRIPT` を使わずに手で VeloX
を操作したログや、`run` 以外の方法 (CI の別ジョブなど) で集めたログを集計
したい場合はこちらを使う。実際に VeloX を
`VELOX_PERF_METRICS=1 VELOX_PERF_FORMAT=json VELOX_PERF_OUTPUT=<path>` 付きで
起動し、想定の操作 (例: `tabs_5` ならタブを 5 個開いた状態を数秒維持する、
`navigation` なら `scripts/bench/pages/` のページ間を数回移動する) を手動で
行うか、上記の `VELOX_AUTOMATION_SCRIPT` を自分で書いて再現し、終了後に
そのログファイルを 1 試行分として渡す。試行回数分だけログファイルを
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

### 6. IPC トラフィックを集計する (`ipc-summary`, Issue #66)

**「WebView ↔ Rust の IPC は何回・何バイト・何ミリ秒か」を継続的に見える
化するサブコマンド。** `run`/`aggregate` のようにシナリオに紐付いた
`BenchmarkResult` は作らない — `ipc` イベント (`metrics::PerfRecord::Ipc`、
Issue #66) だけを `(direction, name)` ごとに集計した表を出す、独立した
診断コマンド。ロジックは `benchmark::summarize_ipc` (純粋 Rust、`cargo
test` で検証済み)。

```sh
# VELOX_PERF_METRICS=1 VELOX_PERF_FORMAT=json で採取したログなら何でもよい
# — `run`/`aggregate` の --output ではなく VELOX_PERF_OUTPUT の生ログ (jsonl)
# を渡す点に注意 (BenchmarkResult 化された JSON ではなく生イベント列が要る)。
VELOX_PERF_METRICS=1 VELOX_PERF_FORMAT=json VELOX_PERF_OUTPUT=/tmp/session.jsonl \
  VELOX_AUTOMATION_SCRIPT=/tmp/script.txt \
  ./target/release/velox

cargo run --release --bin velox-bench -- ipc-summary \
  --input /tmp/session.jsonl \
  --output results/ipc-summary.json
```

出力 (表は `total_bytes` 降順、`--output` を付けると同じ内容を JSON
(`Vec<benchmark::IpcSummary>`) でも保存する):

```text
dir  name                        count  total_bytes  median_ms     p95_ms
out  set_tabs                      120       257023      0.000      0.100
out  set_url                        89         4436      0.000      0.100
...
in   ready                           1           15      0.000      0.000

合計: 430 件 / 269277 bytes
```

- `--input <path>`: 集計対象の `VELOX_PERF_OUTPUT` ログ (jsonl)。**繰り返し
  指定できる** — 複数セッション/複数トライアルのログをまとめて 1 つの表に
  したいときに使う (per-trial の統計を出す `aggregate` とは異なり、ここでは
  トライアル間の warm-up カットは行わない — 実セッションの IPC トラフィック
  に「warm-up」という概念はない)。
- `--output <path>`: 表と同じ内容を `IpcSummary` の JSON 配列として保存。
- 終了コード: `0` = 集計できた、`1` = `ipc` イベントが 1 件も無かった
  (`VELOX_PERF_METRICS=1 VELOX_PERF_FORMAT=json` を付け忘れていないか確認)、
  `2` = 引数エラー等。
- **`direction=in`** (`window.ipc.postMessage` → `UserEvent::ToolbarMessage`)
  は `ToolbarCommand` の `cmd` タグ、**`direction=out`**
  (`ui::window::BrowserWindow::eval_toolbar`) は呼び出し元の `set_*` メソッド
  名がそのまま `name` になる。`duration_ms` は Rust 側のコスト
  (`parse_command`/`evaluate_script` の FFI 呼び出し) のみで、JS 実行や
  DOM 更新の時間は含まない — WebView をブラックボックスとして扱う Epic #57
  ルール 3 のとおり、VeloX 側から計測できるのはここまで。
- 自動操作スクリプト (`VELOX_AUTOMATION_SCRIPT`) が送るタブ操作
  (`open`/`switch`/`close`/`suspend`/`navigate`) は `AutomationCommand` 経由
  で `app.rs` のハンドラを直接呼ぶため、`direction=in` としては現れない
  (`docs/decisions.md` D81) — `in` 側の実測は「トースバーの `ready`/
  `script_started` ハンドシェイク」に限られる。実際のユーザ操作
  (クリック・キー入力) が送る `navigate`/`activate_tab` 等の `in` トラフィック
  はメッセージ本体が数十バイトの固定形状の JSON であることがコード上明らか
  (`ui::toolbar::ToolbarCommand`) なので、自動化スクリプトでは測れないと
  いう限界を D81 に明記した。実測データと結論は
  `docs/performance-targets.md` §18 を参照。

### 7. Windows で実行する (`.github/workflows/perf-windows.yml`, Issue #136)

**Windows (WebView2) 側の性能実測は、この Actions workflow を手動実行
(`workflow_dispatch`) することで行う。** 設計判断は `docs/decisions.md`
D88、実測結果は `docs/performance-targets.md` §21 を参照 — 初回実行
(run [`34127310212`](https://github.com/noan98/VeloX/actions/runs/34127310212)、
2026-09-07、Issue #180) が `windows-latest` ランナー上で success で完走し、
`cold_startup` の実測値と GUI 起動可否 (起動できた) が記録済み。

なおこの初回実行だけは `workflow_dispatch` ではなく、`perf-windows.yml` を
追加した PR #179 に対する `pull_request` トリガー (後述の「`perf-windows.yml`
自身を変更する PR でのみ検証目的で動く」経路) で走ったものである。`main` に
マージされた現在は、下記のとおり `workflow_dispatch` で手動実行できる。

これまでの節 (`run`/`aggregate`/`compare`/`gate`/`ipc-summary`) はすべて
`velox-bench` のサブコマンド自体は OS を問わず同じであり、Linux 向けに
書かれた例の `xvfb-run -a ... dbus-run-session -- ...` の部分だけが
Windows では不要になる (Windows には Xvfb/D-Bus セッションバスに相当する
仕組みが無く、`velox`/`velox-bench` を素のまま起動する)。**Windows
ローカルで手元実行する場合**は、`--velox-bin` を `velox.exe` に、`--output`
のパス区切りを適宜読み替えるだけで、上記 1〜6 の手順がそのまま使える。

**CI (`windows-latest` ランナー) 上で実行する場合**は、GitHub の Actions
タブから `perf-windows.yml` を選び「Run workflow」を押す (`workflow_dispatch`
はブランチを問わず実行できるが、main にマージされるまでタブに現れない —
`release-windows.yml` と同じ制約)。入力パラメータ:

| 入力 | 意味 | 既定 |
| --- | --- | --- |
| `scenario` | `velox-bench list-scenarios` の一覧から選ぶシナリオ ID | `cold_startup` |
| `trials` | 試行回数 | `10` |
| `url` | 計測対象ページの URL。空欄なら `scripts/bench/pages/` の固定ページ (`minimal.html`) を loopback 配信して使う (Linux の `perf-gate.yml` と同じ「ネットワーク非依存の固定ページで測る」方針) | (空、固定ページを使用) |

実行が終わると、結果 JSON (`velox-bench run --output` の出力そのもの、
`aggregate`/`compare`/`gate` にそのままかけられる形式) と実行環境の情報
(OS ビルド番号・CPU・メモリ・WebView2 Runtime バージョン) を記録した
テキストが Actions の Artifact として残る。

**⚠️ この workflow は `windows-latest` ランナー上で VeloX (WebView2)
のウィンドウが実際に起動できるかどうかが最大の未知数である。** Linux は
Xvfb で仮想ディスプレイを用意しているが、Windows ランナーには同種の仕組みが
無く、GitHub がホストする Windows ランナーが GUI プロセスを起動できる対話
セッションを提供しているかは D88 の時点では未検証だった。**「Windows で
動くはず」と決めつけず**、まずこの workflow を実際に走らせて結果を見ること
— 起動できなければ workflow の Job Summary / 診断ステップのログに失敗の
様子が残るので、そこからセルフホストランナー等の代替を検討する
(`docs/decisions.md` D88、Issue #59 が起動時間の調査で採った進め方と同じ)。

**PR ごとの自動実行や性能回帰ゲートとしては使わない** — Windows は
`workflow_dispatch` 限定 (加えて `perf-windows.yml` 自身を変更する PR でのみ
検証目的で動く)。性能回帰の CI ブロッキング判定は引き続き Linux の
`perf-gate.yml` (§10) が担う。

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
- **⚠️ D-Bus セッションバスが無いと、起動したまま無応答になる (Issue #72 で実測)。**
  GitHub Actions の `ubuntu-latest` で発生した。Xvfb があってもウィンドウ作成
  (`BrowserWindow::new`) から先へ進まず、**クラッシュもせず perf ログを 1 行も
  書かないまま**タイムアウトで kill される。`velox-bench` からは「0 件の
  レコードを取得」としか見えない。

  **対処: `dbus-run-session` でラップする。**

  ```sh
  sudo apt install -y dbus-x11
  xvfb-run -a --server-args="-screen 0 1280x900x24" \
    dbus-run-session -- \
      ./target/release/velox-bench run --scenario cold_startup ...
  ```

  WebKitGTK は web process を別プロセスとして起動し、UI プロセスとの IPC に
  D-Bus を使う。セッションバスが無いと子プロセスが起動できず、UI プロセスは
  それを待ち続ける。**プロセスツリーを見ると `WebKitWebProcess` /
  `WebKitNetworkProcess` が存在しない** (正常時は velox 系で 5 プロセスに
  なる) のが決定的な見分け方である。

  切り分けに使える観測点:

  | 観測 | 意味 |
  | --- | --- |
  | perf ログファイルが**一度も作られない** | `build_perf_log` (`app.rs`) に到達していない = `BrowserWindow::new` でブロック |
  | `VELOX_DEBUG=1` のトレースが空 | `UserEvent` が 1 件も発火していない = イベントループに入っていない |
  | `ps` に `WebKitWebProcess` が無い | WebKitGTK の子プロセスが起動できていない |

  以下は実測により原因では**ない**と確認済み。同じ症状が出たときに再度
  疑わなくてよい。

  - WebKitGTK のバージョン差 (ランナーも 2.52.6 でローカルと同一)
  - ソフトウェアレンダリングの強制 (`LIBGL_ALWAYS_SOFTWARE` /
    `WEBKIT_DISABLE_COMPOSITING_MODE` / `WEBKIT_DISABLE_DMABUF_RENDERER`) —
    付けても付けなくても同じように失敗した
  - WebKitGTK のサンドボックスと非特権ユーザ名前空間 —
    `kernel.apparmor_restrict_unprivileged_userns=0` にして `unshare -U` が
    成功する状態でも、`WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS=1` でも失敗した
  - 試行タイムアウトの不足 (全試行が一律で失敗する)
  - バイナリをリポジトリ外へコピーして実行すること

  **ローカルで再現しない点に注意。** 本プロジェクトの開発用コンテナでは
  `DBUS_SESSION_BUS_ADDRESS` が未設定でも動作するため、「ローカルで dbus 無しで
  動くから dbus は無関係」という推論は成り立たない。両環境の dbus の状態は
  同一ではない (ランナーではシステムバスが存在し AT-SPI の解決に失敗する)。

- **⚠️ `tabs_N` シナリオの既定の RSS/PSS サンプリング間隔 (5000ms) では、
  タブを開き終える前の状態しか記録できないことがある (Issue #119 で実測、
  D50 に記録)。**
  `spawn_rss_sampler` (`src/app.rs`) は起動直後に 1 回サンプルを取ってから
  `VELOX_PERF_RSS_INTERVAL_MS` (既定 5000ms, `config::DEFAULT_PERF_RSS_
  INTERVAL`) 間隔でループする一方、`tabs_N` の自動操作スクリプト
  (`browser::automation::generate_bench_script` の `TabCountMemory` 分岐)
  は `open` を待ち時間なしで連続実行し、全タブを開き終えてから
  `MEMORY_STABILIZE_MS` (3000ms) だけ待って `quit` する。**シナリオ全体の
  所要時間が 5000ms 未満で終わることが多く、この場合サンプラの「起動直後の
  1 回目」のサンプルしか記録に残らない** — `pss_total_bytes`/
  `rss_total_bytes` の中央値がタブ数に関係なくほぼ一定になり、見た目には
  それらしい数字が出るため気づきにくい。#61 (`docs/memory-analysis.md` §4.1)
  で最初に発見され、本 Issue で修正した。

  **対処: `velox-bench run` が `tabs_N` を検出したら自動でサンプリング
  間隔を短縮する。** `--rss-interval-ms` を明示しない限り、
  `browser::automation::recommended_rss_interval_ms` が
  `MEMORY_STABILIZE_MS` (タブ数に関わらず一定の待ち時間) から逆算した
  間隔 (既定の定数では 750ms) を `VELOX_PERF_RSS_INTERVAL_MS` として渡す
  — 起動系シナリオ (`cold_startup`/`warm_startup`/`first_page_load`) と
  `navigation`/`tab_create`/`tab_switch` は対象外で、挙動は変わらない。

  ```sh
  # --rss-interval-ms を省略すれば自動調整が効く
  xvfb-run -a --server-args="-screen 0 1280x900x24" dbus-run-session -- \
    ./target/release/velox-bench run --scenario tabs_20 --trials 3 \
      --url http://127.0.0.1:8771/minimal.html --output tabs20.json
  ```

  **それでもサンプル数が足りない場合は、値を静かに返さず警告する。**
  `browser::benchmark::memory_sample_confidence` が `tabs_N` の
  `pss_total_bytes`/`rss_total_bytes` の実際のサンプル数 (`Stats::count`)
  を `trials × MIN_RSS_SAMPLES_PER_TRIAL` (既定 2) と比較し、不足していれば
  `velox-bench run`/`aggregate` が `velox-bench: 警告: ... サンプル数が
  不足しています` を stderr に出し、終了コード `1` を返す。結果ファイル
  自体は採取できたサンプルをそのまま書き出す (D42 の「部分和は `None` では
  なく返す」方針に合わせ、値そのものは隠さない) — CI や手元での確認は
  終了コードと警告メッセージで気付く設計。`--rss-interval-ms` で意図的に
  大きな値を指定するなど、自動調整を上書きした場合には今でも再現しうる
  ので、この警告は自動調整の有無に関わらず常時効く安全網である。

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
- `navigation` / `tab_create` / `tab_switch` / `tabs_1..50`: **Issue #112 で
  `VELOX_AUTOMATION_SCRIPT` フックを実装し、`Xvfb` 上で実際に `run` から
  自動実行して実測に成功した** (4 コア・GPU 無しのコンテナ、ソフトウェア
  レンダリング、`scripts/bench/pages/minimal.html` を loopback 配信、各
  2 試行)。

  | シナリオ | metric | median | p95 | n |
  | --- | --- | ---: | ---: | ---: |
  | `tabs_5` | `page_load_ms` | 31.80 | 42.21 | 10 |
  | `tabs_5` | `tab_create_ms` | 25.75 | 36.86 | 8 |
  | `tabs_5` | `pss_total_bytes` | 140172288 | 142782259 | 2 |
  | `navigation` | `page_load_ms` | 7.45 | 23.68 | 10 |
  | `tab_create` | `tab_create_ms` | 20.65 | 50.22 | 10 |
  | `tab_switch` | `tab_switch_ms` | 0.60 | 0.85 | 16 |
  | `tab_resume` (#63) | `tab_resume_ms` | 2.7 | 3.9 | 64 |

  **⚠️ 上の `tabs_5` の `pss_total_bytes` (n=2) は、上記「実行環境要件」の
  D50 が記録したサンプル不足バグの実例そのものである** — この値が採取された
  時点 (#112) では `velox-bench run` は既定の 5000ms 間隔しか使えず、
  誰もこの数字がタブ数を反映していないことに気付けなかった。歴史的記録
  として残しているが、`tabs_N` の `pss_total_bytes`/`rss_total_bytes` の
  参考値としては使わないこと。

  **これも `cold_startup` の既存の実測結果同様、正式なベースラインでは
  ない** (a) ソフトウェアレンダリングのため RSS/PSS が実機より大きい、
  (b) 試行数が 2 と少ない、(c) 競合ブラウザとの比較条件・目標値は #58 で
  別途定める。ここでの目的は「自動実行の配線が実際に動き、意味のある値を
  返す」ことの確認であり、性能の当落判定ではない。`tab_switch_ms` が
  1ms 未満なのは、このコンテナではソフトウェアレンダリングの初回描画待ちが
  ボトルネックにならない (既にレンダリング済みの背景タブへの切り替えは
  ほぼ即時) ためで、実機の GPU レンダリングでも同程度かは未確認。
- **Windows (`perf-windows.yml`, Issue #136 / #180): `cold_startup` の初回
  実測結果は `docs/performance-targets.md` §21 に記録済み。**
  `windows-latest` ランナー上での GUI/WebView2 ウィンドウ起動 (Issue #136
  時点では未検証だった) と `velox-bench run` の完走はいずれも確認できた
  (run `34127310212`)。ただし `sample_process_tree_rss` の Windows 実装
  (`docs/decisions.md` D88) が返す RSS 値が Windows **実機**上でも同じ精度
  かは未検証のまま — 今回確認できたのは GitHub-hosted (共有・仮想化) ランナー
  上の値であり、Windows 実機の実力値ではない。他シナリオ (`tab_create_N` 等)
  の実測は #180 のスコープ外 — 上記「7. Windows で実行する」節を参照。

## 既知の制約・将来の拡張

- `browser::automation::generate_bench_script` が各シナリオに生成する
  ステップ数・待ち時間 (`NAVIGATION_STEPS` などの定数) は現時点では控えめな
  固定値であり、統計的に十分なサンプル数を保証する設計ではない。
  `tab_switch_ms`/`tab_create_ms` の `n` を増やしたい場合、今は `--trials`
  を増やすしかない (1 試行あたりのステップ数を増やす調整は将来の課題)。
- `browser::automation::recommended_timeout_secs` の見積もり定数
  (`PER_STEP_OVERHEAD_MS` など) は経験則であり、実機のログを継続的に見て
  調整する前提。極端に遅い/速い環境では `--warmup-secs` で明示的に上書き
  すること。
- `switch`/`close <index>` は tab strip 上の**位置** (0 始まり) でタブを
  指定する。並行して他の要因でタブの並び順が変わる状況 (今の VeloX には
  無いが、将来ドラッグ&ドロップでの並べ替え等が入った場合) には対応して
  いない — スクリプトの各行は「その時点の並び」を前提に書く。
- **`background_cpu` (Issue #64) のようなネットワーク版シナリオは無い。**
  `cpu_percent` は VeloX 自身の `/proc` サンプラが記録できるが、
  リクエスト単位のイベントは wry 0.56 経由では Windows 以外観測できない
  (`docs/decisions.md` D17/D59/D80)。バックグラウンドタブのネットワーク
  活動は `velox-bench` ではなく `scripts/profile/network_activity.py`
  (VeloX の外に立てたローカルサーバのアクセスログで数える) で測る —
  `docs/profiling.md` §3.6、結果は `docs/performance-targets.md` §15。
