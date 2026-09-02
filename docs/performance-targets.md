# VeloX 性能目標と競合ベンチマーク条件

Issue #58 / Epic #57。**Phase 3 のすべての最適化はこの文書の条件と目標値を基準に
評価する。** ここが決まっていない状態での最適化は、Epic #57 の絶対ルール
「ベンチマークなしの最適化をしない」に違反する。

---

## 1. 測定環境 (固定)

比較は**必ず同一マシン・同一セッション内**で行う。異なる日・異なるマシンで取った
数値を並べて比較しない。

| 項目 | 値 |
| --- | --- |
| OS | Ubuntu 24.04.4 LTS |
| カーネル | 6.18.44 |
| CPU | Intel Xeon @ 2.80GHz / 4 コア |
| メモリ | 15 GiB |
| ディスプレイ | Xvfb `-screen 0 1280x900x24` |
| GPU | **なし** (ソフトウェアレンダリング) |
| ネットワーク | 使わない (loopback 上の固定ページのみ) |
| WebKitGTK | 2.52.6 |
| rustc | 1.94.1 |
| VeloX ビルド | `cargo build --release` |

> **この環境の重大な制約**: GPU が無いため、両ブラウザともソフトウェア
> レンダリングにフォールバックする (`libEGL warning: DRI3 error`)。**メモリと
> 描画に関わる数値は実機と乖離する。** 同一環境での相対比較としてのみ読むこと。
> 実機での再測定は #70 (CPU / Memory Profiling Workflow) の課題とする。

## 2. 比較対象

| ブラウザ | バージョン | エンジン | 備考 |
| --- | --- | --- | --- |
| VeloX | `88c9356` | WebKitGTK 2.52.6 | システム WebView (wry 0.56) |
| Chromium | 141.0.7390.37 | Blink | Playwright 同梱版 |

**Firefox / Safari / Edge は未測定。** この環境に無く、追加もしていない。Safari は
Linux に存在せず、Edge は実質 Chromium と同一エンジンなので、**次に足す価値が
高いのは Firefox (Gecko)** である。

> **重要**: VeloX はシステム WebView を使うため、この比較は「VeloX 対 Chromium」で
> あると同時に「**WebKitGTK 対 Blink**」でもある。**VeloX 自身のオーバーヘッドと
> エンジン差は分離できていない。** Epic #57 の「WebView をブラックボックスとして
> 扱う」という原則どおり、VeloX が改善できるのは WebView の周囲だけである。

## 3. メトリクス定義

### 3.1 競合比較に使うもの (`scripts/bench/compare_browsers.py`)

ブラウザの内部 API に依存しない外形指標のみを使う。内部イベントは VeloX にしか
無く、比較に使えない。

| メトリクス | 定義 |
| --- | --- |
| `startup_to_load_ms` | プロセス spawn の瞬間から、**ページ自身の `load` イベント**が発火するまでの実時間。ページに注入した beacon が loopback の HTTP サーバを叩き、その到着時刻で測る |
| `pss_bytes` | load から `--settle-secs` 秒後の、プロセスツリー全体の **PSS** 合計 |
| `rss_bytes` | 同時点の RSS 合計 (**比較には使わない**。下記参照) |
| `process_count` | 同時点のプロセス数 |

> ### ⚠️ メモリ比較には PSS を使うこと
>
> **RSS 合計は共有メモリをプロセスごとに丸ごと数えるため、プロセス数の多い
> ブラウザほど大きく出る。** この環境では VeloX が 5 プロセス、Chromium が 9
> プロセスなので、RSS 合計で比較すると VeloX が不当に有利になる。実際、同じ
> 測定で結論が逆転する (§4)。
>
> **これは VeloX 自身の計測にも影響していた。** `browser::metrics::sample_process_tree_rss`
> (D16) はもともと RSS 合計のみを採っており、**VeloX のメモリ優位を過大評価
> していた。** #108 (D42) でこの関数自体に PSS 合計 (`total_pss_bytes`) と
> `pss_process_count` (読めたプロセス数) を追加済み。`velox-bench` の
> `MetricKey` にも `pss_total_bytes` / `pss_process_count` が追加されている
> ので、**#61 / #62 / #63 のメモリ最適化は `rss_total_bytes` ではなく
> `pss_total_bytes` を改善の指標に使うこと。** `smaps_rollup` が読めない環境
> (古いカーネル・権限不足・非 Linux) では `total_pss_bytes` は `null`
> (欠損として扱われ `velox-bench` の集計からは丸ごと省かれる) になり、RSS 側
> は従来どおり必ず取得できる。詳細は `docs/decisions.md` D42、フィールドの
> 意味は `docs/benchmarking.md` の対応表を参照。

### 3.2 VeloX 内部の計測 (`velox-bench`)

`docs/benchmarking.md` を参照。`startup_window_created_ms` / `startup_toolbar_ready_ms` /
`startup_first_load_ms` / `page_load_ms` / `rss_*`。これらは VeloX の内部を分解する
ためのもので、**競合比較には使えない**。

## 4. 初回 baseline (2026-09-02)

`xvfb-run -a --server-args="-screen 0 1280x900x24" python3 scripts/bench/compare_browsers.py`
を各ページで実行した中央値。

### minimal.html (7 試行)

| ブラウザ | load 到達 (ms) | PSS (MiB) | RSS 合計 (MiB) | プロセス数 |
| --- | ---: | ---: | ---: | ---: |
| **VeloX** | **435.6** | 423.6 | 775.2 | 5 |
| Chromium | 561.4 | **328.9** | 854.8 | 9 |

### dom_heavy.html (5 試行)

| ブラウザ | load 到達 (ms) | PSS (MiB) | RSS 合計 (MiB) | プロセス数 |
| --- | ---: | ---: | ---: | ---: |
| **VeloX** | **491.4** | 467.4 | 825.4 | 5 |
| Chromium | 723.9 | **364.6** | 895.5 | 9 |

### VeloX 内部の startup 内訳 (`velox-bench`, cold_startup, minimal.html)

| メトリクス | median (ms) |
| --- | ---: |
| `startup_window_created_ms` | 224.4 |
| `startup_toolbar_ready_ms` | 528.4 |
| `startup_first_load_ms` | 665.3 |
| `page_load_ms` | 77.5 |

## 5. 勝っている領域 / 負けている領域

### ✅ 勝っている: 起動から最初のページ表示まで

**VeloX は Chromium より 22〜32% 速い** (minimal 435.6ms vs 561.4ms、dom_heavy
491.4ms vs 723.9ms)。ページが重くなるほど差が広がっており、7 試行/5 試行で一貫
している。プロセス数が 5 対 9 と少ないことも起動コストに効いていると考えられる。

### ❌ 負けている: メモリ

**PSS で見ると VeloX は Chromium より 28〜29% 多い** (minimal 423.6 vs 328.9 MiB、
dom_heavy 467.4 vs 364.6 MiB)。

**これは VeloX の掲げる「低メモリ」という位置付けと矛盾する。** RSS 合計で見ると
逆に見えるが、それは §3.1 のとおり測り方の問題であり、実態ではない。

Phase 3 のメモリ最適化 (#61 / #62 / #63) は「Chromium より軽い」を出発点にできない。
**まず「なぜ WebKitGTK ベースの VeloX が Blink より PSS で重いのか」を切り分ける
必要がある** — VeloX 側のオーバーヘッドなのか、WebKitGTK と Blink の差なのか。
後者なら Epic #57 の原則上 VeloX には手が出せない領域になる。

### 未測定

タブ生成/切替、複数タブ時のメモリ、バックグラウンド CPU、ページロードの内訳
(DNS/TLS/レンダリング)、バッテリー/アイドル消費。これらは自動駆動の仕組みが
まだ無い (`Scenario::is_unattended()` が `false`)。#106 で起動 URL の指定までは
入ったが、「N タブ開く」「起動後に遷移する」フックは未実装。

## 6. VeloX の性能目標

上記 baseline を踏まえた Phase 3 の目標。**すべて同一環境・同一ページでの中央値**で
評価する。

| # | 目標 | 現在値 | 目標値 | 根拠 |
| --- | --- | ---: | ---: | --- |
| T1 | 起動〜load の優位を**維持**する | Chromium 比 -22〜-32% | **Chromium より速い状態を維持** (目安 -20%) | 既に勝っている領域を最適化で失わないことが最優先。回帰ゲート (#72) の対象 — **判定方式は §10 で確定**。単発の測定値で -20% を割ったことを回帰と判定してはならない (§10 参照) |
| T2 | メモリ (PSS) で Chromium と**同等**まで詰める | Chromium 比 +28〜29% | **Chromium 比 +10% 以内** | 「低メモリ」を名乗る最低条件。まず #61/#62 で VeloX 側の寄与を特定する |
| T3 | `startup_toolbar_ready_ms` を短縮する | 528.4ms | **300ms 以下** | ⚠️ **保留**。当初「この区間は VeloX 自身のコードでエンジン差ではないから確実に手が出せる」と設定したが、**#59 の実測でこの前提は誤りと判明した** (支配的なのは tao/GTK の初期化と WebKitGTK の webview 生成)。目標値は据え置くが、達成手段は現時点で不明。§9 参照 |
| T4 | 20 タブ時に操作不能な遅延を出さない | 未測定 | タブ切替 median **100ms 以下** | #60 の受け入れ条件。まず計測手段が必要 |

> **⚠️ この節の当初の記述は #59 の実測で覆っている。**
>
> #58 の時点では「T3 が Phase 3 で最初に着手すべき項目である。エンジン差の影響を
> 受けず、VeloX のコードだけで改善でき、かつ内訳上いちばん大きい」と書いていた。
> **これは内訳を細分化する前の推測であり、誤りだった。** #59 が
> `window_created → toolbar_ready` を 3 区間に分解して実測した結果、この区間は
> `tao`/GTK のイベントループ初期化と WebKitGTK の最初の webview 生成が支配的で、
> **VeloX の Rust 側起動処理 (history/bookmarks の読み込み、`AppState` 構築) は
> 約 0.1ms、ツールバー自身の JS 実行はほぼ 0ms** だった。`toolbar.html` を 49KB
> から `<script>` 2 行に差し替えても変化しない。詳細は §9 と D43。
>
> **したがって Phase 3 で次に着手すべきは T2 (メモリ) である。** T3 は目標として
> 残すが、エンジン側のコストである以上、Epic #57 の「WebView をブラックボックス
> として扱う」原則の下では VeloX 側から短縮する手段が現時点で無い。

## 7. baseline の保存形式

- 競合比較: `scripts/bench/compare_browsers.py --output <path>` が出力する JSON
- VeloX 内部: `velox-bench` の `BenchmarkResult` JSON (`docs/benchmarking.md` §出力形式)

いずれも OS・CPU コア数・試行回数・実行日時を含む。**git commit と対応付けて
保存すること** — どのコードの数値かが分からない baseline は比較に使えない。

## 8. 再現手順

```sh
# 依存 (Debian/Ubuntu)
sudo apt install -y libwebkit2gtk-4.1-dev xvfb

cargo build --release

# 競合比較
xvfb-run -a --server-args="-screen 0 1280x900x24" \
  python3 scripts/bench/compare_browsers.py \
    --velox ./target/release/velox \
    --chromium /path/to/chromium \
    --page minimal.html --trials 7 \
    --output results/compare-minimal.json

# VeloX 内部の内訳
(cd scripts/bench/pages && python3 -m http.server 8731 &)
xvfb-run -a --server-args="-screen 0 1280x900x24" \
  ./target/release/velox-bench run \
    --scenario cold_startup --trials 10 \
    --url http://127.0.0.1:8731/minimal.html \
    --output results/cold_startup.json
```

## 9. T3 の調査結果 (Issue #59, 2026-09-02)

**結論を先に**: `window_created → toolbar_ready` を細分化して実測した結果、
このギャップは `tao`/GTK のイベントループ初期化と、WebKitGTK が最初の
webview を生成する際のエンジン側コストが支配的で、**`toolbar.html` の内容量
にも VeloX の Rust 側起動処理にもほとんど依存しないことが実測で確認できた**。
49KB のフル `toolbar.html` を `<script>` 2 行だけの最小版に一時的に差し替えて
再計測しても、このギャップはほとんど変化しなかった（詳細・生データは
`docs/decisions.md` D43 を参照）。そのため **本 Issue では有効な最適化を
適用していない** — 効果がゼロと分かっている変更を数値のために入れることは
Epic #57 の「ベンチマークなしの最適化をしない」に反するため。

### 追加した計測

`browser::metrics::StartupTimestamps` に 2 つの中間チェックポイントを追加し
（`rust_setup_done`／`toolbar_script_started`）、`startup` イベントの JSON に
`rust_setup_done_ms`／`toolbar_script_started_ms` フィールドが増えた。
`velox-bench` の `MetricKey` にも対応する
`startup_rust_setup_done_ms`／`startup_toolbar_script_started_ms` を追加した。
計測オフ時のオーバーヘッドは増えていない（既存の `Option` パターンを維持、
D19）。

### VeloX 内部の同一セッション before/after (`velox-bench`, cold_startup, minimal.html, 各 10 試行)

計測を追加する前 (`e8f1250`, before) と、計測を追加した後 (after, 機能的な
実行パスの変更なし) を同一セッション内で比較。

| メトリクス | before median (ms) | after median (ms) | 変化率 |
| --- | ---: | ---: | ---: |
| `startup_window_created_ms` | 209.70 | 191.50 | -8.7% |
| `startup_rust_setup_done_ms` | (未計測) | 191.65 | — |
| `startup_toolbar_script_started_ms` | (未計測) | 378.60 | — |
| `startup_toolbar_ready_ms` | 397.50 | 380.20 | -4.4% |
| `startup_first_load_ms` | 426.35 | 404.70 | -5.1% |

`velox-bench compare --baseline before.json --candidate after.json` は
**回帰なし**（すべての既存メトリクスが閾値 10% 以内、`rss_*` も含む）。
`toolbar_ready` の -4.4% はセッション内変動の範囲内であり、計測追加による
実質的な改善ではない（実行パスを一切変えていないので当然の結果）。

新しく分かったのは内訳: `rust_setup_done`(191.65ms) は `window_created`
(191.50ms) とほぼ同時刻 — history/bookmarks/input_history の読み込みと
`AppState` 構築は無視できるコスト（約 0.1ms）。一方
`toolbar_script_started`(378.60ms) は `toolbar_ready`(380.20ms) とほぼ同時刻
— ツールバー自身の JS 実行はほぼ 0ms。つまり `window_created` →
`toolbar_ready` の約 190ms は、ほぼ全て `rust_setup_done →
toolbar_script_started` の区間（エンジンがツールバーのドキュメントをパース
し終えるまで）に集中している。この区間が `toolbar.html` のサイズに依存しな
いことは `docs/decisions.md` D43 に記載した実験（フル版 vs 最小版の比較）で
確認済み。

### 競合比較 (`compare_browsers.py`) の after — T1 が保たれているか

| ブラウザ | load 到達 median (ms, 7 試行) | PSS median (MiB) |
| --- | ---: | ---: |
| **VeloX** | **453.9** | 423.9 |
| Chromium | 560.6 | 330.3 |

VeloX は Chromium より **-19.0%** 速い（§4 の初回 baseline は -22.4%
〔minimal〕）。

**この測定値は当初 T1 に書いていた -20% の線をわずかに下回っている。** 本 Issue は
実行パスを一切変更していない（計測点を増やしただけ）ので、コード変更による回帰では
なく、セッション間のノイズと考えるのが自然である — このマシンは他セッションと共有
されており、並行するビルドの有無で ±10〜20% 程度変動する。

ただし「回帰ではない」と「目標を達成した」は別である。**-19.0% は -20% の線を
満たしていない**ので、ここでは達成とは書かない。むしろこの結果は、**単発の中央値と
固定閾値で T1 を判定する運用自体が、この環境では成立しないことを示している。**
#72 (Performance Regression Gate) は、複数回測定・統計処理・許容分散を前提に
設計する必要がある（#72 の本文にも同じ注意書きがある）。T1 の閾値はその設計が
決まった時点で見直すこと。優位そのもの（VeloX の方が明確に速い）は保たれている。

### T3 (`startup_toolbar_ready_ms` ≤ 300ms) は未達

**達成できなかった。** 実測により、このメトリクスを支配しているのは
VeloX 自身のコードではなく `tao`/GTK の初期化と WebKitGTK の webview 生成
コストであることが分かったため、本 Issue のスコープ（新規依存を避ける、
`unsafe` 原則禁止、correctness を壊さない）の中では安全に短縮する手段が
見つからなかった。参考実験として `LIBGL_ALWAYS_SOFTWARE=1
WEBKIT_DISABLE_COMPOSITING_MODE=1` を設定すると `rust_setup_done →
toolbar_script_started` がおよそ半分になったが、これは本評価環境が GPU を
持たないために発生する `DRI3`/`EGL` ネゴシエーション失敗のリトライコストを
スキップしているだけであり、実 GPU を持つ利用者の環境には当てはまらない
（`docs/decisions.md` D43 参照）。したがって本 Issue のコードには含めていない。

## 10. 回帰ゲートの判定方式 (Issue #72, 2026-09-02)

§9 で「単発の中央値と固定閾値で T1 を判定する運用自体が、この環境では成立
しない」と書いた問題への回答。**T1 の判定方式はここで確定する。**

### 追加で実測したノイズ

`cold_startup` を 10 試行 × 6 セット、**同一バイナリ・同一コミット
(`b81b1fc`)・コード変更なし**で連続実行し、セット間のばらつきを 2 通りの
見方で計測した（生データ・コマンドは `docs/decisions.md` D46 参照）。

- **隣接セット同士の比較**（CI の同一ジョブ内比較が近似する条件）:
  `startup_first_load_ms` 最大 19.0%、`startup_toolbar_ready_ms` 最大
  16.0%、`pss_total_bytes` 最大 28.8%。
- **セット 1 とセット 6 の比較**（約 9 分離れた測定。古い baseline
  ファイルとの比較が近似する条件）: `page_load_ms` +77.0%/+78.9%、
  `startup_toolbar_ready_ms` +52.2%/+53.4%、`startup_first_load_ms`
  +49.2%/+52.4%。**同一バイナリでここまで動く。**

この 2 つ目の数字が、**固定 baseline ファイルとの比較を CI のブロッキング
判定に使ってはならない**という結論の直接の根拠である。

### 採用した判定方式

`src/browser/benchmark.rs` の `evaluate_gate`（純粋ロジック、`cargo test`
で境界値・同着・試行数不足・baseline 欠損を含めて検証済み）が実装する:

1. **2 段階の重大度**: `warn_pct`(既定 20%) と `fail_pct`(既定 60%)。
   `fail_pct` は隣接セットの最大ノイズ (28.8%) に約 31pt、セット 1/6 間の
   最悪ノイズ (53.4%、絶対値フロアで吸収できないメトリクスに限る) にも
   約 7pt のマージンを残す。
2. **メトリクスごとの最小絶対差**: 相対閾値だけでは吸収できない
   (`page_load_ms` の +78.9% は絶対では 16.2ms の変化でしかなかった)。
3. **複数候補測定の多数決**: CI では候補 (PR head) を 2 回測定し、**両方**
   が `fail_pct` を超えたときのみ Fail とする — 1 回だけの悪化は Warn に
   留める。
4. **試行数不足の検出**: baseline/candidate のいずれかが 5 試行未満なら
   `low_confidence` を立て、Fail への昇格を禁止する。

固定閾値 1 本 + 単発比較という素朴な方式を採らなかった理由は、上記の実測
そのもの — セット 1/6 間で無変更のバイナリが 50〜79% 動く環境では、固定
閾値だけでは「ノイズか回帰か」を区別できない。

### CI での運用: baseline は都度その場で作る

**`results/baseline/cold_startup-linux-xvfb.json`（この dev/agent コンテナ
で採取、§7 の形式で commit 紐付け済み）は、CI のブロッキング判定には
使わない。** 上記のとおり、機械が変わる/セッションが変わる比較はこの
環境で意味をなさないと実測で分かっているため。**代わりに
`.github/workflows/perf-gate.yml` は、PR の merge-base コミットと PR
head コミットを同一ジョブ内で連続してビルド・計測し、その場で作った
baseline と candidate を `velox-bench gate` に渡す。** これにより機械差・
セッション差そのものを比較から除去する。コミット済みの baseline
ファイルは、経時トレンドを人が目視で追うための参考情報として残す
(`docs/benchmarking.md` 参照)。

### 結果として T1 はどう判定されるか

- **CI (ブロッキング)**: 上記の `evaluate_gate` — Chromium との比較ではなく
  **VeloX 自身の PR head vs merge-base** の `startup_*`/`page_load_ms` を
  対象にする。Chromium 比 -20% という数値自体は目安として §6 に残すが、
  Chromium バイナリを CI に持ち込む仕組みはまだ無い (#58 の競合比較は手動
  実行のまま) ため、**CI が自動でブロックするのは「Chromium との差」では
  なく「直前の VeloX からの回帰」である。**
- Chromium との相対優位 (§4〜§5) は、引き続き手動での `compare_browsers.py`
  実行で確認する。自動化は将来 Issue の対象。

### CI で GUI ベンチを回すかどうか

**回す。** Xvfb でこの環境の `velox-bench run` が実測できることは #106 /
本 Issue で確認済みで、GUI を回さない代替 (純粋ロジックのマイクロベンチ等)
では `startup_*`/`page_load_ms` という T1 が問題にしている指標そのものを
測れない。コストは同一ジョブ内で 2 回ビルド + 3 回計測 (baseline 1 回 +
candidate 2 回) が掛かるが、`ci.yml` の本体 CI とは別ワークフローに分離
してあるため、通常の fmt/clippy/test/build のフィードバック速度には影響
しない。
