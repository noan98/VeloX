# Performance Decisions

性能計測・ベンチマーク・メモリ削減・プロファイリング・回帰検知に関する Decision の入口です。

## 対象

主担当は **D16, D19, D21, D41–D49, D56–D58, D79–D82, D84–D90, D92–D97, D99, D101, D104–D106, D109–D115** の 44 件 (網羅的な一覧は下の「詳細」節。ここは読み進める手がかりとしての要約)。

- **D16** — `/proc` ベースの metrics
- **D19** — tab latency / structured perf output
- **D21** — benchmark suite
- **D41–D46** — competitive benchmarking / PSS / startup 分解 / automation / profiling / regression gate
- **D47–D49** — integration test / memory root cause / WebContext sharing
- **D56–D58** — adaptive suspension / WebProcess sharing 上限 / background CPU
- **D79–D82** — メモリ/リソース監査 / background network / IPC 計測 / dashboard
- **D84–D90** — 統合テストの固定 wait 根治 / state dispatch / page load / Windows 実測 / serialization / 自動休止のデフォルト ON
- **D92–D97** — 起動チェックポイント / Memory Budget Manager / gate の入力検証 / Windows の起動「回帰」/ メモリ計測シナリオ
- **D99, D101** — Windows での T2 評価と T2-W の定義
- **D104–D106** — 結果 JSON の機種情報 / 休止タブ数 / dashboard の機種単位
- **D109–D115** — #176 Stage 1 の計測群、RAM 相対のメモリ予算、複数ページ計測

## 補助ドキュメント

- [`../benchmarking.md`](../benchmarking.md)
- [`../performance-targets.md`](../performance-targets.md)
- [`../memory-analysis.md`](../memory-analysis.md)
- [`../profiling.md`](../profiling.md)
- [`../performance-dashboard.md`](../performance-dashboard.md)

## 詳細

**このカテゴリが主担当の Decision: D16, D19, D21, D41–D49, D56–D58, D79–D82, D84–D90, D92–D97, D99, D101, D104–D106, D109–D115** (44 件)

`archive.md` の全 Decision は、いずれか 1 つのカテゴリが主担当として必ず
この一覧に載る。複数カテゴリにまたがるものは主担当だけに載せ、必要なら
本文から相互参照する (README の「重要なルール」3)。

⚠️ **この節は `generate_decision_index.py` が生成する。手で編集しない。**
`archive.md` との対応は `test_decision_index.py` が CI で検査するので、
追記漏れも、見出しを変えたことによるリンク切れも、そこで落ちる。

- [D16](./archive.md#d16-performance-metrics--proc-directly-no-new-dependency) — Performance metrics — `/proc` directly, no new dependency
- [D19](./archive.md#d19-performance-metrics-part-2--tab-latency-structured-output-one-record-type) — Performance metrics, part 2 — tab latency, structured output, one record type
- [D21](./archive.md#d21-benchmark-suite-14--pure-aggregation-module--separate-unverifiable-headless-runner-binary) — Benchmark suite (#14) — pure aggregation module + separate, unverifiable-headless runner binary
- [D41](./archive.md#d41-competitive-benchmarking-measures-an-external-load-beacon-and-compares-memory-by-pss) — Competitive benchmarking measures an external load beacon, and compares memory by PSS
- [D42](./archive.md#d42-browsermetrics-gains-pss-alongside-rss-rather-than-instead-of-it) — `browser::metrics` gains PSS, alongside RSS rather than instead of it
- [D43](./archive.md#d43-window_created--toolbar_ready-を細分化して実測した結果有効な最適化は見つからなかった) — `window_created` → `toolbar_ready` を細分化して実測した結果、有効な最適化は見つからなかった
- [D44](./archive.md#d44-benchmark-automation-hook--a-read-once-opt-in-script-file-not-a-socketrpc-server) — Benchmark automation hook — a read-once opt-in script file, not a socket/RPC server
- [D45](./archive.md#d45-プロファイリングは既存の外部ツール--browsermetrics-の組み合わせとし常設の計測コードは足さない) — プロファイリングは既存の外部ツール + `browser::metrics` の組み合わせとし、常設の計測コードは足さない
- [D46](./archive.md#d46-performance-regression-gate-72--2-段階閾値--絶対差フロア--多数決baseline-は同一ジョブ内でその場作成) — Performance Regression Gate (#72) — 2 段階閾値 + 絶対差フロア + 多数決、baseline は同一ジョブ内でその場作成
- [D47](./archive.md#d47-integration-test-基盤-34--velox_automation_script-を駆動機構に再利用しgui-不可環境は実行時判定でスキップ) — Integration Test 基盤 (#34) — `VELOX_AUTOMATION_SCRIPT` を駆動機構に再利用し、GUI 不可環境は実行時判定でスキップ
- [D48](./archive.md#d48-メモリ超過の主因は-webkitgtkblink-のエンジン差ではなくvelox-自身の-webviewwebcontext-の使い方だった) — メモリ超過の主因は WebKitGTK/Blink のエンジン差ではなく、VeloX 自身の webview/`WebContext` の使い方だった
- [D49](./archive.md#d49-toolbarタブ間で-webcontext-を共有--効果は部分的-networkprocess-は統合できたが-webprocess-は残った実装は残す) — toolbar/タブ間で `WebContext` を共有 — 効果は部分的 (`NetworkProcess` は統合できたが `WebProcess` は残った)、実装は残す
- [D56](./archive.md#d56-adaptive-tab-suspension--3-シグナルの適応ポリシー休止の単位はタブではなくプロセスグループ) — Adaptive Tab Suspension — 3 シグナルの適応ポリシー、休止の単位はタブではなくプロセスグループ
- [D57](./archive.md#d57-タブ生成切替の律速は-velox-側ではなく-web-プロセスの起動--既定の上限-4-は据え置きノブだけ公開する) — タブ生成・切替の律速は VeloX 側ではなく web プロセスの起動 — 既定の上限 4 は据え置き、ノブだけ公開する
- [D58](./archive.md#d58-バックグラウンドタブの-cpu-は-webkitgtk-が既に抑えている--velox-側の実装は足さず計測手段だけ用意する) — バックグラウンドタブの CPU は WebKitGTK が既に抑えている — VeloX 側の実装は足さず、計測手段だけ用意する
- [D79](./archive.md#d79-メモリリソースライフタイム監査-62--page_load_timers-の無制限成長を修正共有-webprocess-の-retention-は有界と確認して見送り) — メモリ/リソースライフタイム監査 (#62) — `page_load_timers` の無制限成長を修正、共有 WebProcess の retention は「有界」と確認して見送り
- [D80](./archive.md#d80-バックグラウンドタブのネットワーク活動--webkitgtk-は-1-秒未満のタイマーだけをクランプするネットワーク要求そのものは止まらず完全な抑制は既存の-adaptive-tab-suspension-63-頼み) — バックグラウンドタブのネットワーク活動 — WebKitGTK は 1 秒未満のタイマーだけをクランプする。ネットワーク要求そのものは止まらず、完全な抑制は既存の Adaptive Tab Suspension (#63) 頼み
- [D81](./archive.md#d81-ipc-計測基盤-66--perfrecordipc-で-js--rust-を両方向計測し実測に基づいてタブストリップ全件再送信は意図的履歴パネルの無条件再送信は不要と切り分けた) — IPC 計測基盤 (#66) — `PerfRecord::Ipc` で JS ↔ Rust を両方向計測し、実測に基づいて「タブストリップ全件再送信は意図的」「履歴パネルの無条件再送信は不要」と切り分けた
- [D82](./archive.md#d82-performance-dashboard-71--velox-benchperf-gate-の出力形式をそのまま保存し比較は明示的に同一とマークしたセッションの中でしか許可しない) — Performance Dashboard (#71) — `velox-bench`/perf-gate の出力形式をそのまま保存し、比較は「明示的に同一とマークしたセッション」の中でしか許可しない
- [D84](./archive.md#d84-統合テストの固定-wait-を根治する--automationcommandwaitload-をmain-スレッドの状態機械--通知チャネルで実装しd44-の枠内-新規制御チャネルなし-に収める) — 統合テストの固定 wait を根治する — `AutomationCommand::WaitLoad` を「main スレッドの状態機械 + 通知チャネル」で実装し、D44 の枠内 (新規制御チャネルなし) に収める
- [D85](./archive.md#d85-統合テストに残った固定-wait-を無くす-173--wait_startup-を追加して-d84-の-revisit-condition-1-を解消し2-は実測の上でwait_load-に置き換えない結論を確定させた) — 統合テストに残った固定 wait を無くす (#173) — `wait_startup` を追加して D84 の Revisit condition (1) を解消し、(2) は実測の上で「wait_load に置き換えない」結論を確定させた
- [D86](./archive.md#d86-browser-state--event-dispatch-最適化-67--persist_session-の無条件ディスク書き込みを唯一の削減対象として特定し直前スナップショットとの比較でスキップする形にしたtabwindow-lookup-と-lock-contention-は実測前の設計調査だけで対象外と判断した) — Browser State / Event Dispatch 最適化 (#67) — `persist_session` の無条件ディスク書き込みを唯一の削減対象として特定し、直前スナップショットとの比較でスキップする形にした。tab/window lookup と lock contention は実測前の設計調査だけで「対象外」と判断した
- [D87](./archive.md#d87-ページロードの段階計測-69--navigationstarted--loadstarted--loadfinished-に分解dns接続tls-timing-は-wry-に無いvelox-側の追加最適化も見送り) — ページロードの段階計測 (#69) — `NavigationStarted → LoadStarted → LoadFinished` に分解。DNS/接続/TLS timing は wry に無い、VeloX 側の追加最適化も見送り
- [D88](./archive.md#d88-windows-で性能を実測できるようにする-136--sample_process_tree_rss-に-toolhelp32psapi-実装を追加しpss-相当は実装しないと結論perf-windowsyml-workflow_dispatch-限定-を追加) — Windows で性能を実測できるようにする (#136) — `sample_process_tree_rss` に Toolhelp32/PSAPI 実装を追加し、PSS 相当は「実装しない」と結論。`perf-windows.yml` (`workflow_dispatch` 限定) を追加
- [D89](./archive.md#d89-serialization--allocation-最適化-68--escape_js_line_terminators-の常時フルコピーを削除write_json-の内訳は実測の結果-fswrite-が支配的でシリアライズは対象外と判明) — Serialization / Allocation 最適化 (#68) — `escape_js_line_terminators` の常時フルコピーを削除。`write_json` の内訳は実測の結果 `fs::write` が支配的でシリアライズは対象外と判明
- [D90](./archive.md#d90-自動タブ休止をデフォルト-on-にする-issue-184--メモリ予算シグナルのみ700-mibd9-の-opt-in-方針と-d56-revisit-condition-3-の決着) — 自動タブ休止をデフォルト ON にする (Issue #184) — メモリ予算シグナルのみ、700 MiB。D9 の opt-in 方針と D56 Revisit condition (3) の決着
- [D92](./archive.md#d92-起動の-process_start--window_created-を-4-つの中間チェックポイントで分解する-182--計測の追加のみで最適化はまだ行わない) — 起動の `process_start` → `window_created` を 4 つの中間チェックポイントで分解する (#182) — 計測の追加のみで、最適化はまだ行わない
- [D93](./archive.md#d93-memory-budget-manager-issue-176-stage-1--計測の結果本体コードは変更しないram-相対の予算は裸の比率では成立しないことが分かった) — Memory Budget Manager (Issue #176) Stage 1 — 計測の結果、本体コードは変更しない。RAM 相対の予算は「裸の比率」では成立しないことが分かった
- [D94](./archive.md#d94-回帰ゲートの入力前提を-gate-自身が検証する-issue-196--シナリオos-不一致空-candidate-をokと言わせない) — 回帰ゲートの入力前提を `gate` 自身が検証する (Issue #196) — シナリオ/OS 不一致・空 candidate を「OK」と言わせない
- [D95](./archive.md#d95-issue-195-metrics-off-で-toolbar-ipc-の-json-二重パース-は現行コードには存在しなかった--型で防がれている) — Issue #195 (metrics OFF で Toolbar IPC の JSON 二重パース) は現行コードには存在しなかった — 型で防がれている
- [D96](./archive.md#d96-windows-の起動回帰issue-208-は回帰ではなかった--同一-run-内-ab-で-185-を否定しwindows-latest-が-run-ごとに別スペックのマシンを割り当てることを確認した) — Windows の起動「回帰」(Issue #208) は回帰ではなかった — 同一 run 内 A/B で #185 を否定し、`windows-latest` が run ごとに別スペックのマシンを割り当てることを確認した
- [D97](./archive.md#d97-windows-のメモリ計測は回収が終わるまで待つシナリオで行う-issue-197--tabs_n-の値で製品の挙動を論じてはいけない) — Windows のメモリ計測は「回収が終わるまで待つ」シナリオで行う (Issue #197) — `tabs_N` の値で製品の挙動を論じてはいけない
- [D99](./archive.md#d99-windows-で-t2-chromium-比-を評価できるようにする-issue-197--pss-が無い-os-では近似値を作らず真の値を区間で挟む) — Windows で T2 (Chromium 比) を評価できるようにする (Issue #197) — PSS が無い OS では「近似値を作らず、真の値を区間で挟む」
- [D101](./archive.md#d101-windows-用のメモリ目標-t2-w-を定義する-issue-197--区間比較で-met-を出すprivate-working-set-への置き換えはしない) — Windows 用のメモリ目標 T2-W を定義する (Issue #197) — 「区間比較で met を出す」。Private Working Set への置き換えは**しない**
- [D104](./archive.md#d104-velox-bench-の結果-json-に機種情報を持たせる-issue-211-項目1--environment-infomd-では機械的に突き合わせられない) — `velox-bench` の結果 JSON に機種情報を持たせる (Issue #211 項目1) — `environment-info.md` では機械的に突き合わせられない
- [D105](./archive.md#d105-休止タブ数を-rss-の隣に出せるようにする-issue-197-revisit-condition-4--0-件は欠損ではない) — 休止タブ数を RSS の隣に出せるようにする (Issue #197 Revisit condition (4)) — 「0 件」は欠損ではない
- [D106](./archive.md#d106-performance-dashboard-に機種という比較単位を足しperf-windowsyml-を週次スケジュール実行にする-issue-211-項目2項目4--機種不明のエントリは安全側に倒して連結しないゲート化はまだしない) — Performance Dashboard に「機種」という比較単位を足し、`perf-windows.yml` を週次スケジュール実行にする (Issue #211 項目2/項目4) — 機種不明のエントリは安全側に倒して連結しない、ゲート化はまだしない
- [D109](./archive.md#d109-tabs_hold_50-の実測-issue-197--予算は-50-タブでも守られるd97-revisit-2-を閉じ1-の結論を-50-タブまで広げる残るのは深さの妥当性) — `tabs_hold_50` の実測 (Issue #197) — 予算は 50 タブでも守られる。D97 Revisit (2) を閉じ、(1) の結論を 50 タブまで広げる。残るのは「深さ」の妥当性
- [D110](./archive.md#d110-176-の最初の一手を計測にする-issue-176-stage-1--tabs_hold_resume_n-で休止の対価を測れるようにする) — #176 の最初の一手を「計測」にする (Issue #176 Stage 1) — `tabs_hold_resume_N` で休止の**対価**を測れるようにする
- [D111](./archive.md#d111-計測ページを選べるようにし結果に記録する-issue-176--どのページで測ったかを結果から消さない) — 計測ページを選べるようにし、結果に記録する (Issue #176) — 「どのページで測ったか」を結果から消さない
- [D112](./archive.md#d112-休止の対価はwebview-の作り直しであってページの再読み込みではない-issue-176-stage-1--予測を実測が否定した) — 休止の対価は「webview の作り直し」であって「ページの再読み込み」ではない (Issue #176 Stage 1) — 予測を実測が否定した
- [D113](./archive.md#d113-搭載-ram-の検出を入れる-issue-176--d93-子-issue-案-b--予算の式はまだ変えない) — 搭載 RAM の検出を入れる (Issue #176 / D93 子 Issue 案 B) — 予算の式はまだ変えない
- [D114](./archive.md#d114-既定のメモリ予算を搭載-ram-相対にする-issue-176--d93-子-issue-案-c--ただし今日より小さくしないを制約に据える) — 既定のメモリ予算を搭載 RAM 相対にする (Issue #176 / D93 子 Issue 案 C) — ただし「今日より小さくしない」を制約に据える
- [D115](./archive.md#d115-複数ページを同一-run-内で計測できるようにする-issue-231--scenarios-と同じ答えをページに対しても出す) — 複数ページを同一 run 内で計測できるようにする (Issue #231) — `scenarios` と同じ答えを、ページに対しても出す

---

- [設計判断アーカイブ (全文)](./archive.md)
