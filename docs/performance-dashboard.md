# VeloX Performance Dashboard

Issue #71 (Epic #57 傘下、#58/#70 依存)。**この文書は「これまでの計測結果を
時系列で並べて眺める」役目を扱う。** 個々の計測は
[docs/benchmarking.md](benchmarking.md) (`velox-bench`) と
[docs/performance-targets.md](performance-targets.md) (競合比較・目標値・
回帰ゲートの判定方式) が定義したものをそのまま使う — このダッシュボードは
**新しい計測手段ではなく、既存の計測結果を保存・比較・可視化する薄い層**
である。

```
計測する                             velox-bench run / compare_browsers.py
                                      (docs/benchmarking.md)
              │
              ▼
CI で回帰をブロックする              velox-bench gate
                                      (.github/workflows/perf-gate.yml、
                                       docs/performance-targets.md §10)
              │
              ▼
人が経時トレンドを眺める             この文書 / scripts/dashboard/
                                      (velox-bench gate と同じ判定ロジックを
                                       呼び出して使う — 別ロジックは持たない)
```

外部サービス (GitHub Pages、DB、SaaS 等) への依存は増やしていない。
リポジトリ内に結果を追記保存し (`results/history/`)、それを読んで静的な
HTML/Markdown レポートを生成するだけの Python スクリプト
(`scripts/dashboard/`) である。新規の Rust/Python 依存も追加していない
(標準ライブラリのみ)。

## 1. なぜ `velox-bench gate`/`compare` だけでは足りないのか

`velox-bench compare`/`gate` は「2 つの結果ファイルを比較する」機能は
持っているが、**結果を保存して並べる場所を持たない**。Issue #71 の受け入れ
条件 (過去結果と比較できる / OS ごとに分離できる / commit・PR 単位で確認
できる / 測定条件を保存する) は、保存の仕組みそのものが要る。

## 2. 最重要の設計判断: 「セッション」と「機種」を比較の単位にする

`docs/performance-targets.md` §10 (`docs/decisions.md` D46) は、この開発
コンテナが**無変更バイナリでもセッションを跨ぐと最大 +78.9% 動く**ほど
ノイズが大きいことを実測している。「過去結果と比較できる」機能を素朴に
実装すると、この原則と正面から矛盾する — 単に時系列で並べて線を引けば
「差が見える」ように見えてしまうが、その差の大半はノイズであり得る。

**採用した解決策**: 比較を「セッション」という明示的な単位でしか許可しない。

- 記録するたびに `session_id` を持つ。**省略すると呼び出しごとに一意な
  値が自動生成される** — 何もしなければ、2 回の記録は別セッション扱いに
  なり、自動的には比較されない (`scripts/dashboard/common.py` の
  `generate_session_id`)。
- 複数の計測を「同一マシン・同一セッションで採った、比較してよい系列」
  として扱いたい場合は、呼び出し側が **同じ `session_id` を明示的に**
  指定する必要がある。ちょうど `.github/workflows/perf-gate.yml` が
  baseline/candidate を同一ジョブ内で測っているのと同じ粒度感 —
  「セッションを跨がない」ことを保証する責任は呼び出し側 (人間、または
  CI ワークフロー) に残す。
- `report.py` は**同一 `session_id` の隣接エントリ同士だけ**を線でつなぎ、
  `velox-bench gate` (CI の回帰ゲートと同一ロジック) で差分の重大度を
  計算する。`session_id` が変わる境界では、グラフ上は線を引かず (点だけ
  プロット)、表では差分バッジを出さない。

この規則は**データ構造** (`session_id` フィールドが無ければ比較できない)
と **UI** (`session_id` が変わる箇所は視覚的に途切れる) の両方で強制されて
いる — どちらか一方だけでは、「セッションを跨いだ数値をうっかり比較して
しまう」事故を防げない。

**この設計が犠牲にしているもの**: 「main ブランチの性能が半年でどう推移
したか」を 1 本の連続した折れ線として自動で見せることはできない
(たとえ全ての PR が個別に記録されていても、それぞれ別セッションなら線は
つながらない)。これは意図的なトレードオフである — この環境のノイズの
大きさ (D46) を踏まえると、そういう連続した折れ線を描くこと自体が
「ノイズを回帰の証拠であるかのように見せる」誤りを埋め込むことになる。
本物の回帰検知は `.github/workflows/perf-gate.yml` (同一ジョブ内 baseline
比較) が担っており、このダッシュボードは**それを補完する人間向けの一覧
性**が役割である。実機 (Issue #136 の Windows 手動計測など) でノイズの
小さい環境が使えるようになった時点で、この制約は緩められる可能性がある
(§6 の Revisit condition 参照)。

### 2.1 「機種」という、もう一段の比較単位 (Issue #211 項目2 / D106)

`docs/decisions.md` D96 (Issue #208) は、`windows-latest` が **run ごとに
別スペックのマシンを割り当てる**ことを実測した — これまでに AMD EPYC
9V74 / Intel Xeon 8573C / Intel Xeon 6973P-C / AMD EPYC 7763 の 4 機種が
観測されており、`startup_window_created_ms` の絶対値は 573〜922ms まで
散らばる。

`session_id` は「呼び出し側が明示的に同一と指定した run」を表すだけで、
**その run がどの機種で計測されたかは別問題**である。将来、定期計測
(Issue #211 項目4) が「時系列として蓄積する」ために複数回の run へ同じ
`session_id` を使い回すようになった場合、機種が違う run を同一系列として
つないでしまう恐れがある。

そこで `session_id` に加えて **`machine_key`**
(`scripts/dashboard/common.py` の `derive_machine_key`) を比較のもう一段
の単位にした。`report.py` は**`session_id` と `machine_key` の両方が一致
する隣接エントリ同士だけ**を線でつなぎ、差分を計算する — どちらか一方
でも変われば、§2 と同じ扱い (点は表示するが線を引かず、差分も出さない)
になる。`machine_key` は `result.environment` の `os`/`cpu_model`/
`cpu_count`/`total_memory_bytes` (Issue #211 項目1 で追加されるフィールド、
すべて省略可能) から導出する純粋関数。

**「機種不明」の扱い**: `cpu_model` が無いエントリ (item1 以前に記録された
既存の `results/history/` の全エントリを含む) は、安全側に倒して**他の
どのエントリとも同一機種として扱わない** — 「不明」同士を同一機種とみなす
と、比較してはいけない組み合わせを比較することになるためである。**この
既存データはこれまで通り読み込める**(`schema_version` は変更していない)
が、`report.py` 上では常に孤立した点として表示される。

## 3. 保存形式

`velox-bench run`/`aggregate`/`gate` が使う `BenchmarkResult` JSON
(`docs/benchmarking.md` の「結果ファイルのフォーマット」) は**一切変換しない**
— そのまま 1 エントリの `"result"` フィールドに包む。これが perf-gate の
出力形式とダッシュボードの保存形式を分岐させない方法である: 変換ステップが
無ければ、2 つの形式が食い違って腐ることもない。

```jsonc
// results/history/<os>/<scenario>.jsonl — 1 行 1 エントリ (追記のみ)
{
  "schema_version": 1,
  "ingested_at": "2026-09-07T01:31:47Z",   // 取り込み時刻 (RFC3339)
  "session_id": "baseline-committed",       // §2 参照。比較の唯一の単位
  "source": "baseline-committed",           // 自由記述: manual / ci-perf-gate / baseline-committed 等
  "branch": "main",                         // 任意
  "pr_number": null,                        // 任意 (int)
  "note": "docs/performance-targets.md §4 の初回 baseline (#58, PR #109)",
  "result": { /* BenchmarkResult そのまま。environment.os/cpu_count/
                 git_commit/generated_at/trials が「測定条件」を運ぶ */ }
}
```

- **保存場所は OS ごとに分離** (`results/history/<os>/...`) — Epic #57
  ルール5「OS ごとに結果を分ける」を、ディレクトリ構造そのもので強制する。
  Linux (WebKitGTK) と将来の Windows (WebView2) / macOS (WKWebView) の数値が
  同じファイルに混在することは構造上あり得ない。
- **「測定条件を結果と一緒に保存」**: `BenchmarkResult.environment` が
  すでに OS・CPU コア数・git commit・実行日時・試行回数を持つ
  (`docs/benchmarking.md` の受け入れ条件どおり)。それに加えて、この
  ラッパーが `session_id`/`source`/`branch`/`pr_number` を足す — commit
  単体では「どの PR の変更か」「どの計測セッションの一部か」までは分から
  ないため。
- **`results/history/**/*.jsonl` はコミットする** (`results/baseline/` と
  同じ方針 — 生データであり、再生成できない)。**レポート出力
  (`report.html`/`report.md`) はコミットしない** (`.gitignore` 参照) —
  `results/compare-*.json` 等これまでの生成物と同じ扱いで、履歴データと
  `velox-bench` さえあればいつでも再生成できるビルド出力である。

## 4. commit / PR との紐付け

各エントリは `result.environment.git_commit` (`BenchmarkResult` が元々持つ
フィールド) に加えて、記録時に渡した `branch`/`pr_number` を保持する。
`report.py` はコミットを `<repo-url>/commit/<sha>`、PR を
`<repo-url>/pull/<pr_number>` へのリンクとして描画する (静的なテキストリンク
であり、GitHub API は一切呼ばない — 外部依存を増やさない方針どおり)。

**CI からの自動記録は本 Issue のスコープに含めていない。**
`.github/workflows/perf-gate.yml` が生成する baseline/candidate の
`BenchmarkResult` は、そのままの形式で `scripts/dashboard/record.py
--result baseline.json --result candidate-1.json --result candidate-2.json
--session-id <一意な値> --branch <PR のブランチ> --pr <PR 番号>` に渡せる
形になっている (perf-gate.yml 自身の `merge_base`/`github.event.pull_request`
コンテキストから機械的に埋められる) が、実際にワークフローへ組み込むと
「毎回の PR で `results/history/` に commit する」運用が発生し、
Issue #71 の受け入れ条件 (保存形式を固める) を超える意思決定
(誰がその commit を作るか、mainへの書き戻しをどうするか) が要る。
**§6 の Revisit condition に送る。**

## 5. threshold (閾値) の表示

差分の重大度 (OK/WARN/FAIL) は、可能な限り `velox-bench gate`
(`benchmark::evaluate_gate`) をそのまま呼び出して計算する —
`GateThresholds` の既定値 (`warn_pct=20.0` / `fail_pct=60.0`) に加えて、
メトリクスごとの最小絶対差 (`MetricKey::min_significant_delta`) も適用
される、CI の回帰ゲートと**全く同じ**判定ロジック。`velox-bench` バイナリ
が手元に無い場合のみ、絶対差フロアを持たない簡易フォールバック
(`common.classify_fallback`、既定 20%/60%) に切り替わり、その旨をレポート
上に明記する (実際の `velox-bench gate` より神経質に振れ得る)。

閾値の数値そのものを 2 重管理しないよう、可能な限り `velox-bench gate` の
出力 (`GateReport.thresholds`) を経由させている — Rust 側の
`GateThresholds::default()` が変わればダッシュボードの判定も自動的に
追随する。

## 6. 使い方

```sh
# 1. 計測する (docs/benchmarking.md の手順どおり)
xvfb-run -a --server-args="-screen 0 1280x900x24" dbus-run-session -- \
  ./target/release/velox-bench run --scenario cold_startup --trials 10 \
    --velox-bin ./target/release/velox \
    --url http://127.0.0.1:8731/minimal.html \
    --output /tmp/cold_startup.json

# 2. 履歴に記録する (session_id を省略すると自動生成される)
python3 scripts/dashboard/record.py \
  --result /tmp/cold_startup.json --source manual

# 同一セッションとして複数ファイルをまとめて記録する場合
# (perf-gate.yml の baseline/candidate 相当):
python3 scripts/dashboard/record.py \
  --result baseline.json --result candidate-1.json --result candidate-2.json \
  --source ci-perf-gate --session-id "pr123-run456" \
  --branch my-branch --pr 123

# 3. レポートを生成する (velox-bench バイナリがあれば自動検出する)
python3 scripts/dashboard/report.py \
  --output results/history/report.html \
  --markdown-output results/history/report.md
```

`scripts/dashboard/report.py --help` / `record.py --help` で全オプションを
確認できる。

## 7. グラフについて

Issue #71 の実装内容が挙げる「startup / memory / CPU / tab switching
graphs」に対応する代表メトリクスを、シナリオに存在するものだけ自動選択して
素の SVG (文字列組み立て、追加ライブラリなし) で描く
(`common.METRIC_GROUPS`):

| グループ | 優先して使うメトリクス (存在する最初のもの) |
| --- | --- |
| startup | `startup_first_load_ms` → `startup_window_created_ms` |
| memory | `pss_total_bytes` → `rss_total_bytes` (PSS を優先 —
  `docs/performance-targets.md` §3.1 のプロセス数二重計上問題) |
| cpu | `cpu_percent` |
| tab | `tab_switch_ms` → `tab_create_ms` → `tab_resume_ms` |

受け入れ条件 4 点 (過去比較 / OS 分離 / commit・PR 紐付け / 測定条件保存)
の方がグラフより優先度が高い、という Issue 本文の指針どおり、グラフは
上記のデータモデルの上に薄く載っているだけで、グラフ自体は数値の主たる
根拠ではない — 正確な数値と重大度判定は表 (`velox-bench gate` 経由) の
方にある。

## 8. 動作確認 (2026-09-07)

実データ (`results/baseline/cold_startup-linux-xvfb.json`、#58 で実際に
計測されたもの) を `results/history/linux/cold_startup.jsonl` に記録し、
`report.py` で HTML/Markdown レポートを生成できることを確認した。

追加で、この dev/agent コンテナ上で `cargo build` (debug ビルド) した
`velox`/`velox-bench` を使い、同一セッション内で `cold_startup` を 2 回・
`tab_switch` を 1 回実際に計測し (Xvfb + `dbus-run-session`、
`docs/benchmarking.md` の手順どおり)、それらを記録・レポート生成する一連の
流れが動作することも確認した (この確認用の計測データ自体はリポジトリには
コミットしていない — debug ビルドの数値であり、正式な baseline や履歴として
残す性質のものではないため)。同一セッション内の 2 点は `velox-bench gate`
経由で正しく差分 (重大度付き) が計算され、セッションが変わる箇所
(`baseline-committed` → 別セッション) では線が途切れ、差分も計算されない
ことを確認した。

## 9. 既知の制約・Revisit condition

- **CI (`perf-gate.yml`/`perf-windows.yml`) からの自動記録は未実装** (§4)。
  手動 (または将来のワークフロー変更) で `record.py` を呼ぶ運用を前提に
  している。自動化する場合は「誰が `results/history/` への commit を
  作るか」の運用設計が追加で必要 — 単純にワークフローに追記させると、
  PR の fork/権限によっては push できないケースがある。**Issue #211 項目4
  で `perf-windows.yml` にスケジュール実行を足した時点でもこれは未解決の
  まま** — スケジュール実行は結果 JSON を artifact として残すのみで、
  `results/history/windows/` への取り込みは依然として手動 (`record.py`)
  を要する。
- **「機種」を比較のもう一段の単位にした** (§2.1、Issue #211 項目2 /
  `docs/decisions.md` D106)。`session_id` が同じでも機種が違えば連結・
  差分計算をしない。機種不明の既存エントリは安全側に倒し孤立点として
  表示する。
- **セッションを跨いだトレンドを 1 本の折れ線として自動で見せる機能は
  意図的に持たせていない** (§2)。この環境のノイズの大きさが理由であり、
  実機 (Issue #136) でノイズの小さい継続計測ができるようになれば、
  「セッションを跨いでも許容誤差内なら緩やかにつなぐ」といった拡張は
  再検討の余地がある。
- **Windows/macOS のデータは現時点で 0 件**。`results/history/` は OS
  ディレクトリが無ければ単にそのセクションが空で表示される (エラーには
  ならないことを `--os macos` フィルタで確認済み)。Issue #136 (Windows
  手動計測ワークフロー) の成果が `record.py` に渡ればそのまま
  `results/history/windows/` に載る — ダッシュボード側の変更は不要。
- **セッション境界をまたぐ「見た目のグラフ上の連続性」の欠如は仕様**であり
  バグではない — §2 を参照。
- `report.py` の HTML はテーマ (ダーク/ライトモード) に追随する最小限の
  CSS のみ持つ。凝った UI ライブラリは意図的に使っていない
  (`グラフは「あれば良い」もの` という Issue の指針どおり)。
