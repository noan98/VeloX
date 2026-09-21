# Performance Decisions

性能計測・ベンチマーク・メモリ削減・プロファイリング・回帰検知に関する Decision の入口です。

## 対象

主担当は **D16, D19, D21, D41–D49, D56–D58, D79–D82, D84–D90, D92–D97, D99, D101, D104–D106, D109–D115, D117–D118, D120–D125, D127, D129–D130, D132, D135, D137–D138, D142–D145, D147–D152** の 69 件 (網羅的な一覧は下の「詳細」節。ここは読み進める手がかりとしての要約)。

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

**このカテゴリが主担当の Decision: D16, D19, D21, D41–D49, D56–D58, D79–D82, D84–D90, D92–D97, D99, D101, D104–D106, D109–D115, D117–D118, D120–D125, D127, D129–D130, D132, D135, D137–D138, D142–D145, D147–D152** (69 件)

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
- [D117](./archive.md#d117-ram-相対の予算は重いページほど効かない-issue-176--d114-の効果測定--予算の調整で届く範囲の上限が見えた) — RAM 相対の予算は「重いページほど効かない」 (Issue #176 / D114 の効果測定) — 予算の調整で届く範囲の上限が見えた
- [D118](./archive.md#d118-メモリの内訳はvelox-自身-vs-エンジンで割る--プロセス名によるロール分類は-windows-では情報を増やさない-issue-176-stage-1) — メモリの内訳は「VeloX 自身 vs エンジン」で割る — プロセス名によるロール分類は Windows では情報を増やさない (Issue #176 Stage 1)
- [D120](./archive.md#d120-webview2-の休止-api-は-2-つとも実機で使える--ただし使えると効くは別で効き目はまだ測っていない-issue-176-stage-2) — WebView2 の休止 API は 2 つとも実機で使える — ただし「使える」と「効く」は別で、効き目はまだ測っていない (Issue #176 Stage 2)
- [D121](./archive.md#d121-trysuspend-は採らない--メモリを返さないからで遅いからではない-issue-243--ただし休止の対価は下げられないd112-は機構としては誤りだった) — `TrySuspend` は採らない — メモリを返さないからで、遅いからではない (Issue #243) — ただし「休止の対価は下げられない」(D112) は機構としては誤りだった
- [D122](./archive.md#d122-memoryusagetargetlevellow-は効く--5660-返る-issue-242--そして-d121-が書いたプロセスが残れば常駐も残るは誤りだった) — `MemoryUsageTargetLevel(LOW)` は効く — 56〜60% 返る (Issue #242) — そして D121 が書いた「プロセスが残れば常駐も残る」は誤りだった
- [D123](./archive.md#d123-windows-では背景タブに-memoryusagetargetlevellow-を既定で伝える-issue-242--hint-が効くと休止は発火しなくなりd105--d112-の対価は日常的には払わなくなる) — Windows では背景タブに `MemoryUsageTargetLevel(LOW)` を既定で伝える (Issue #242) — hint が効くと休止は発火しなくなり、D105 / D112 の対価は日常的には払わなくなる
- [D124](./archive.md#d124-hint-はメモリを減らす機構ではなく同じ予算で生きたタブを増やす機構だった-issue-247--既定は維持し休止は安全網として残す) — hint は「メモリを減らす機構」ではなく「同じ予算で生きたタブを増やす機構」だった (Issue #247) — 既定は維持し、休止は安全網として残す
- [D125](./archive.md#d125-撤回--beacon-は計測タブから-1-本も来ていなかった-issue-247配線を直しlow-の判断は再計測まで保留する) — 撤回 — beacon は計測タブから 1 本も来ていなかった (Issue #247)。配線を直し、`LOW` の判断は再計測まで保留する
- [D127](./archive.md#d127-背景タブは-low-を受けても-js-を止めていない-issue-247--ただし-lownormal-はまだ分離できておらずd124-決定5-の問いは開いたまま) — 背景タブは `LOW` を受けても JS を止めていない (Issue #247) — ただし `low`/`normal` はまだ分離できておらず、D124 決定5 の問いは開いたまま
- [D129](./archive.md#d129-low-は背景タブの実行を何も変えない-issue-247--腕を分離して確定ただし-d124-決定5-の本体はまだ開いている) — `LOW` は背景タブの実行を何も変えない (Issue #247) — 腕を分離して確定。ただし D124 決定5 の本体はまだ開いている
- [D130](./archive.md#d130-low-が縮めたのは-js-ヒープではない-issue-247--桁が-3-倍足りないページから手の届く範囲は測り切った) — `LOW` が縮めたのは JS ヒープではない (Issue #247) — 桁が 3 倍足りない。ページから手の届く範囲は測り切った
- [D132](./archive.md#d132-週次計測を-resultshistory-へ自動で取り込む-issue-211-項目4-の後半--main-へ直接-push-する選択肢は最初から無くgithub_token-で作った-pr-は誰にも気付かれない) — 週次計測を `results/history/` へ自動で取り込む (Issue #211 項目4 の後半) — main へ直接 push する選択肢は最初から無く、`GITHUB_TOKEN` で作った PR は誰にも気付かれない
- [D135](./archive.md#d135-stage-3-の優先度の梯子は今は作らない-issue-176--段に割り当てる別々の動作がまだ無く信用できない入力を必要と分かる前に入れない) — Stage 3 の優先度の梯子は今は作らない (Issue #176) — 段に割り当てる別々の動作がまだ無く、信用できない入力を必要と分かる前に入れない
- [D137](./archive.md#d137-d135-決定3決定4-の前提は誤っていた-issue-176-stage-3--243-は-d135-より前に終わっており段に割り当てる動作は既に在る) — D135 決定3・決定4 の前提は誤っていた (Issue #176 Stage 3) — #243 は D135 より前に終わっており、段に割り当てる動作は既に在る
- [D138](./archive.md#d138-メモリ予算はメモリを返す休止しか命じない-issue-176-stage-3--見込み解放量を機構に聞く機構は設定値ではなく実際に使えるものを使う) — メモリ予算は「メモリを返す休止」しか命じない (Issue #176 Stage 3) — 見込み解放量を機構に聞く。機構は設定値ではなく実際に使えるものを使う
- [D142](./archive.md#d142-フォーム入力の検知手段はある-176-stage-3-の前提調査--ただし-iframe-からは必ず親フレーム経由で中継する直接送信は-windows-で黙って消える) — フォーム入力の検知手段は「ある」 (#176 Stage 3 の前提調査) — ただし iframe からは必ず親フレーム経由で中継する。直接送信は Windows で黙って消える
- [D143](./archive.md#d143-入力中のフォームを持つタブは休止しない-issue-272--フラグは-tab-に置き政策の切り替えは信号の出口ではなく入口で行う) — 入力中のフォームを持つタブは休止しない (Issue #272) — フラグは `Tab` に置き、政策の切り替えは信号の出口ではなく入口で行う
- [D144](./archive.md#d144-タブのピン留めを実装し休止から保護する-issue-277--pinned-は-has_form_input-と違いページではなくトラステッドな-ui-が出す信号なので絶対保護に入れる) — タブのピン留めを実装し、休止から保護する (Issue #277) — `pinned` は `has_form_input` と違い、ページではなくトラステッドな UI が出す信号なので絶対保護に入れる
- [D145](./archive.md#d145-揺り戻しは-trial-の外で起きていた-issue-279--176-stage-3--窓を足すのは新しいシナリオで集計は最後の-measure_start-を基準に導出する) — 揺り戻しは trial の外で起きていた (Issue #279 / #176 Stage 3) — 窓を足すのは新しいシナリオで、集計は最後の `measure_start` を基準に導出する
- [D147](./archive.md#d147-試行ごとの-perf-ログ-生の-json-lines-を-perf-windows-の-artifact-に残す-issue-176-stage-3--d145-revisit-condition-1--velox-bench-run---keep-logs-で結果ファイル名に揃えて残し集計は-1-バイトも変えない) — 試行ごとの perf ログ (生の JSON Lines) を perf-windows の artifact に残す (Issue #176 Stage 3 / D145 Revisit condition (1)) — `velox-bench run --keep-logs` で結果ファイル名に揃えて残し、集計は 1 バイトも変えない
- [D148](./archive.md#d148-休止スイープの時系列は-job-summary-に出す-issue-176-stage-3--d147-revisit-condition-2--生ログを読む最初の形は判定ごとの要求タブ数-vs-実際の休止数で予算は-ram-から-rust-と同じ式で再現する) — 休止スイープの時系列は Job Summary に出す (Issue #176 Stage 3 / D147 Revisit condition (2)) — 生ログを読む最初の形は「判定ごとの要求タブ数 vs 実際の休止数」で、予算は RAM から Rust と同じ式で再現する
- [D149](./archive.md#d149-スクリプトの形は定数のまま計測用の上書きを-velox-bench-run-の引数で通し結果-json-に残す-issue-176-stage-3--477--効かないシナリオへの指定はエラーラウンド数の上限はコンパイル時の前提と同じ-12) — スクリプトの形は定数のまま、計測用の上書きを `velox-bench run` の引数で通し、結果 JSON に残す (Issue #176 Stage 3 / §47.7) — 効かないシナリオへの指定はエラー、ラウンド数の上限はコンパイル時の前提と同じ 12
- [D150](./archive.md#d150-rss-レコードに私的コミット-windows-の-pagefileusage-を並べる-issue-176-stage-3--478--予算の判定は変えずまず山がコミットの増加かを数字で決める) — `rss` レコードに私的コミット (Windows の `PagefileUsage`) を並べる (Issue #176 Stage 3 / §47.8) — 予算の判定は変えず、まず「山がコミットの増加か」を数字で決める
- [D151](./archive.md#d151-予算の判定が比べる量を-velox_memory_budget_input-で選べるようにする-issue-176-stage-3--479--既定は従来どおりワーキングセットprivate-は計測の腕既定を替えるのは-ab-の後) — 予算の判定が比べる量を `VELOX_MEMORY_BUDGET_INPUT` で選べるようにする (Issue #176 Stage 3 / §47.9) — 既定は従来どおりワーキングセット、`private` は計測の腕。既定を替えるのは A/B の後
- [D152](./archive.md#d152-windows-の予算の判定は私的コミットを既定にする-issue-176-stage-3--4710--velox_memory_budget_input-の既定を-private-に替えresident-はオプトアウトとして残す) — Windows の予算の判定は私的コミットを既定にする (Issue #176 Stage 3 / §47.10) — `VELOX_MEMORY_BUDGET_INPUT` の既定を `private` に替え、`resident` はオプトアウトとして残す

---

- [設計判断アーカイブ (全文)](./archive.md)
