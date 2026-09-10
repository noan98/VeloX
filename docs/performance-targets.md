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

| メトリクス | 定義 | OS |
| --- | --- | --- |
| `startup_to_load_ms` | プロセス spawn の瞬間から、**ページ自身の `load` イベント**が発火するまでの実時間。ページに注入した beacon が loopback の HTTP サーバを叩き、その到着時刻で測る | 共通 |
| `pss_bytes` | load から `--settle-secs` 秒後の、プロセスツリー全体の **PSS** 合計 (`smaps_rollup` のカーネル計算値) | Linux のみ |
| `private_bytes` | 同時点の **Private Working Set** 合計 = **真の PSS の下界** | Windows のみ |
| `pss_upper_bytes` | 私有ページ + 共有ページを `ShareCount` で割った和 = **真の PSS の上界**。近似値ではない (下記参照) | Windows のみ |
| `rss_bytes` | 同時点の RSS / Working Set 合計 (**比較には使わない**。Linux は PSS が、Windows は `pss_upper_bytes` がある) | 共通 |
| `lower_bytes` / `upper_bytes` | **真の PSS を挟む区間**。Linux では PSS 自身なので下界 = 上界 (幅ゼロ)、Windows では `private_bytes` と `pss_upper_bytes` | 共通 |
| `process_count` | 同時点のプロセス数 | 共通 |

> ### Windows には PSS が無いので「挟み込む」 (Issue #197)
>
> Linux の `Pss:` は共有ページを共有プロセス数で割った値を**カーネルが計算して**
> 返す。Windows にこれに相当するものは無い (D88)。
>
> D88 は `QueryWorkingSetEx` の `ShareCount` で `1/ShareCount` を足し上げる近似を
> 検討し、「正確さが自明でない」として見送った。**近似値として使う限り、その判断は
> 正しい** — `PSAPI_WORKING_SET_BLOCK` の `ShareCount` は **3 bit しかなく 7 で
> 飽和する**ので、8 個以上のプロセスが同じ DLL ページを共有する状況では共有ページの
> 重みが実際より重く出るうえ、**飽和の度合いがプロセス数に依存する**。
>
> **しかし上界として使えば飽和は破綻しない。** 報告値を c、実際の共有プロセス数を
> n とすると、飽和していなければ n = c、飽和していれば n >= 7 = c なので、
> **どちらでも n >= c**。よって各ページの寄与は `page_size / n <= page_size / c`
> であり、和は必ず真の PSS 以上になる。飽和は上界を緩めるだけで、上界であること
> 自体を壊さない (D99 決定1)。
>
> そこで近似値は作らず、**真の PSS を上下から挟む厳密な値**を採る。
> `QueryWorkingSet` は各ページの `Shared` と `ShareCount` を返すので、1 回の
> 呼び出しですべて得られる。
>
>     private_bytes  <=  真の PSS  <=  pss_upper_bytes  <=  rss_bytes (Working Set 合計)
>
> `rss_bytes` も正しい上界だが「c = 1 と置いた」のと同じで最も緩い。実測では
> 4 倍ほど緩く、**そのままでは T2 を判定できなかった** (§29.8 → §29.10)。
>
> 判定は区間で行い、**区間が重なる間は「判定不能」と言う** — 片方の端を代表値に
> 選んで断定するのは、測れていないものを測れたことにする行為である
> (`compare_bounds`)。Linux では下界 = 上界なので、従来どおりの PSS 比較に退化し、
> **既存の評価方法は変わらない。**
>
> ⚠️ **Windows の下界/上界を Linux の PSS と並べてはならない。** 算出方法が違う
> ので OS をまたいだ数値比較は成立しない (D88 の警告、Epic #57 の絶対ルール5)。

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

> **⚠️ 2026-09-03 (#61) の実測で切り分けが完了した。詳細は
> [docs/memory-analysis.md](memory-analysis.md) を参照。**
>
> 「VeloX 側のオーバーヘッドなのか、WebKitGTK と Blink の差なのか」という
> 上記の問いへの答え: **主因は WebKitGTK と Blink のエンジン差ではなく、
> VeloX 自身の実装 (toolbar を独立 webview にしている設計 + webview を
> 作るたびに独立した `WebContext` を作っている wry の呼び出し方) だった。**
> 単一 webview だけの最小 WebKitGTK アプリを作って計測すると PSS は約
> 296〜299 MiB で、これは Chromium (318〜328 MiB) より**軽い** — WebKitGTK
> というエンジン自体が重いという証拠はこの環境では見つからなかった。VeloX
> が実際に重いのは、1 タブでも toolbar 用 + content 用の 2 つの webview
> (=2 組の `WebKitWebProcess`+`WebKitNetworkProcess`) を同時に持っている
> ためで、この超過分だけで 1 タブ時の Chromium 超過分のほぼ全部を説明できる。
> さらにタブが増えるたびに VeloX は正確に +2 プロセスするのに対し
> Chromium は概ね +1 プロセスで済んでおり (タブごとに独立した `WebContext`
> を作っているため)、20 タブでは VeloX は Chromium の約 5.2 倍の PSS になる。
> VeloX 自身の Rust heap (heaptrack 実測 32.08 MiB、タブ数やページの重さに
> 依らず一定) はツリー全体 PSS の 1〜8% 程度に過ぎず、削っても全体にはほぼ
> 効かないことも確認済み。**Epic #57 の「エンジンをブラックボックスとして
> 扱う」原則には反しない** (WebKit 内部ではなく wry への webview の作り方の
> 話) が、**未実装・未検証の仮説**であることに変わりはない — 実装・計測は
> #62 に引き継ぐ (`docs/decisions.md` D48)。

Phase 3 のメモリ最適化 (#61 / #62 / #63) は「Chromium より軽い」を出発点にできない。
上記のとおり、原因の切り分け自体は #61 で完了した。

> **⚠️ 2026-09-03 (#118) で toolbar/タブ間の `WebContext` 共有を実装・計測
> した。詳細は [docs/memory-analysis.md](memory-analysis.md) §9、
> `docs/decisions.md` D49 を参照。**
>
> 結論: **効果は実測できる程度に有意だが、部分的。** `WebKitNetworkProcess`
> はタブ数に関わらず 1 個に統合できた (タブ 1 個あたりの増分プロセス数が
> +2→+1 に、Chromium と同じ増分パターンになった) が、PSS の大半を占める
> `WebKitWebProcess` は webview ごとに独立したまま統合されなかった
> (WebKitGTK の `WebContext` は `NetworkProcess` の生成単位ではあるが
> `WebProcess` の生成単位ではないと判明)。結果として PSS は 1/5/10/20 タブ
> で -2.5%〜-11.0% 減少したが、**T2 (Chromium 比 +10% 以内) には遠く届いて
> いない** (1 タブで +45.5%、20 タブで +402.2%、変更前はそれぞれ +48.6%/
> +428.1%)。プライベートモードは D15/D48 の予測どおり wry の制約で共有が
> 効かず、変更前と同一の挙動 (webview ごとに独立した `WebProcess`+
> `NetworkProcess`) のまま。**効果がゼロではなく measurable なため実装は
> 残したが、T2 達成には別の手段 (`WebProcess` 自体の共有) が必要。**

> **⚠️ 2026-09-04 (#124) でその「別の手段」— `with_related_view` による
> タブ間の `WebKitWebProcess` 共有 — を実装・計測した。詳細は
> [docs/memory-analysis.md](memory-analysis.md) §10、`docs/decisions.md`
> D54 を参照。**
>
> 結論: **効果は大きいが T2 には届かない。** content タブの `WebProcess`
> を最大 4 タブごとに 1 つへ統合し (読み込み中のタブがいるプロセスには
> 相乗りしない)、PSS は 5/10/20 タブで -16.8% / -22.6% / -25.6%、1 タブ
> あたりの増分は 92.4 → 63.3 MiB/タブ。Chromium 比は 20 タブで +365.0% →
> +246.2%。1 タブ時は共有相手が無いため +45% のまま。全タブを 1 プロセス
> に乗せる案は burst オープン時のページロードが直列化 (`tab_switch` の
> `page_load_ms` +638%) して Epic #57 ルール 4 に抵触したため不採用。
> 残りの超過 (同一プロセス内でもページ 1 枚あたり 54 MiB、Chromium の
> 5.6 倍) の切り分けは #63 (Adaptive Tab Suspension) に引き継ぐ。

### タブ数に対する増え方 (2026-09-03、#61 で測定 / #118 で before/after 追記 / #124 で再測定)

1/5/10/20 タブで `minimal.html` を計測 (`scripts/bench/tab_scaling.py`、
各 3 試行の中央値、詳細は `docs/memory-analysis.md` §4/§9)。

| タブ数 | before (#61時点) VeloX PSS (MiB) | after (#118: WebContext共有後) VeloX PSS (MiB) | after (#124: WebProcess共有後) VeloX PSS (MiB) | Chromium PSS (MiB) |
| ---: | ---: | ---: | ---: | ---: |
| 1  | 409.6  | 408.5  | 409.0  | 276.7 |
| 5  | 777.7  | 790.9  | 655.7  | 315.6 |
| 10 | 1387.0 | 1234.5 | 989.5  | 366.0 |
| 20 | 2421.9 | 2322.5 | 1612.3 | 464.8 |

（#124 列は 2026-09-04 の別セッションの計測。同一セッション内の before/after
比較は `docs/memory-analysis.md` §10.2 を参照 — before 409.2/787.9/1277.7/
2165.8 MiB → after 409.0/655.7/989.5/1612.3 MiB、同時測定した Chromium は
281.2/320.4/367.3/465.8 MiB。）

（#61 と #118 は別セッションの計測のため、上表の Chromium 列は #61 時点の
参考値。#118 は同一セッション内の before/after 比較を別途行っており、その
数値は `docs/memory-analysis.md` §9.2 を参照— before 419.0/834.8/1386.4/
2456.6 MiB → after 408.5/790.9/1234.5/2322.5 MiB、-2.5%〜-11.0%。）

1 タブあたりの増分は before 約 107 MiB/タブ → after 約 101 MiB/タブ
(#118、同一セッション比較)。Chromium は約 9.9 MiB/タブ。原因はプロセス数の
増え方 (before: VeloX +2/タブ → after: VeloX +1/タブ。Chromium は +1/タブ
程度) と一致する — `WebKitNetworkProcess` の統合分だけ増分が縮んだ
(`docs/memory-analysis.md` §9.3)。

### 未測定

ページロードの内訳 (DNS/TLS/レンダリング)、バッテリー/
アイドル消費。タブ生成/切替のレイテンシと複数タブ時のメモリは #61/#112 で
測定可能になった (`velox-bench run --scenario tab_create|tab_switch|tabs_N`、
`scripts/bench/tab_scaling.py`) — メモリ側は上表のとおり測定済み。ただし
`velox-bench run --scenario tabs_N` 自体の `pss_total_bytes`/`rss_total_bytes`
は既定の RSS サンプリング間隔 (5000ms) がシナリオの所要時間より長いことが
多く、タブ数に対する増え方の指標としては現状使えない
(`docs/memory-analysis.md` §4.1、`docs/decisions.md` D48)。

## 6. VeloX の性能目標

上記 baseline を踏まえた Phase 3 の目標。**すべて同一環境・同一ページでの中央値**で
評価する。

| # | 目標 | 現在値 | 目標値 | 根拠 |
| --- | --- | ---: | ---: | --- |
| T1 | 起動〜load の優位を**維持**する | Chromium 比 -22〜-32% | **Chromium より速い状態を維持** (目安 -20%) | 既に勝っている領域を最適化で失わないことが最優先。回帰ゲート (#72) の対象 — **判定方式は §10 で確定**。単発の測定値で -20% を割ったことを回帰と判定してはならない (§10 参照) |
| T2 | メモリ (PSS) で Chromium と**同等**まで詰める | Chromium 比 +45% (1 タブ)。**既定 (Issue #184/D90、メモリ予算 700 MiB) で** 10/20 タブ +26% / +32%、上限を明示 (`VELOX_MAX_LIVE_TABS=4`) すると 10/20 タブで +14% / +20% (§12・§23・§25・`docs/memory-analysis.md` §11) | **Chromium 比 +10% 以内** | 「低メモリ」を名乗る最低条件。**#61 で主因を特定 → #118 で `WebContext` 共有 (-2.5〜-11%) → #124 で `WebKitWebProcess` 共有 (5/10/20 タブで -17/-23/-26%、D54) → #63 で Adaptive Tab Suspension (D56): 空にできるプロセスグループを丸ごと休止する適応ポリシーで、有効時は 20 タブ -65% (1612 → 560 MiB、Chromium 比 +245% → +20%) → #184 でこれを既定 ON に (D90): メモリ予算 700 MiB のみを既定で有効化し、20 タブ +245% → +32% を設定なしで誰でも得られるようにした。**T2 (+10% 以内) は未達のまま**。残りは 1 タブ時の toolbar 用 `WebProcess` (+45%) と生存タブ分 — §12・§23 参照。→ #176 Stage 1 (D93・§25) が、**休止ポリシー側にはもう余地がほとんど無い**ことを実測で確定させた: 既定 700 MiB は 10 タブの時点で既に背景タブ 9 個中 8 個を休止しており、予算をさらに下げても 1 タブ時の下限 (約 397 MiB) に阻まれる。T2 の残りを埋めるには下限そのもの (toolbar 用 `WebProcess`) を削るしかない**
| T2-W | **Windows で** メモリの区間比較を `met` にする (§30) | `inconclusive`。VeloX [88.9, 198.7] MiB / Edge [172.4, 311.7] MiB (§29.10) | **VeloX の上界 ≤ 比較相手の下界 × 1.10** (`compare_bounds` が `met`) | **T2 は PSS で定義されており、Windows には PSS が無いので評価できない** (§29.10、D99)。CLAUDE.md が Windows を最優先と定める以上、**最優先 OS で評価できない目標を完了条件に据えることはできない。** 区間比較なら PSS 無しで厳密に判定でき、しかも現在は未達 (上界が 4.8% 超過)。定義と、達成手段に課す制約は §30 を参照 |
| T3 | `startup_toolbar_ready_ms` を短縮する | 528.4ms | **300ms 以下** | ⚠️ **保留**。当初「この区間は VeloX 自身のコードでエンジン差ではないから確実に手が出せる」と設定したが、**#59 の実測でこの前提は誤りと判明した** (支配的なのは tao/GTK の初期化と WebKitGTK の webview 生成)。目標値は据え置くが、達成手段は現時点で不明。§9 参照 |
| T4 | 20 タブ時に操作不能な遅延を出さない | **20 タブでタブ切替 0.40ms** (`tab_switch_20`、48 サンプル)、休止タブへの切替 `tab_resume` 2.7ms + 再読み込み 10ms | タブ切替 median **100ms 以下** | ✅ **達成** (#60、§13)。目標を 2 桁下回る。ただしこの指標はメインスレッドのハンドラが返るまでで、描画完了までではない (D57 の「残る限界」) |

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

## 11. T2 (メモリ) の切り分け結果 (Issue #61, 2026-09-03)

**詳細な測定データ・再現手順は [docs/memory-analysis.md](memory-analysis.md)
を参照。ここでは§5〜6 の「なぜ負けているか / T2 は達成可能か」に対応する
結論だけを記録する。**

**結論を先に**: §5 の「VeloX 側のオーバーヘッドなのか、WebKitGTK と Blink
の差なのか」という問いに対する答えは、**主因は VeloX 自身の実装**だった。
実測の柱は 3 つ:

1. **プロセス別 PSS 内訳**: VeloX (1 タブ) は `velox` 本体 (約 70.5 MiB) +
   `WebKitWebProcess`×2 (約 310 MiB) + `WebKitNetworkProcess`×2 (約 35.5
   MiB) の 5 プロセス、合計約 416 MiB。webview が 1 タブしかないのに
   `WebKitWebProcess`/`WebKitNetworkProcess` が 2 個ずつあるのは、VeloX が
   toolbar 用と content 用の 2 つの webview を常に同時に持つ設計
   (`docs/architecture.md` D3) の帰結。
2. **VeloX 自身の Rust heap (`heaptrack` 実測)**: 32.08 MiB で、ページの
   重さ (`minimal.html`/`dom_heavy.html`) にもタブ数 (1〜5 タブ) にも
   依存せず一定。ツリー全体 PSS の 1〜8% に過ぎず、削っても全体にはほぼ
   効かないことを確認した。
3. **エンジンだけの比較**: VeloX の Rust コードを一切含まない、webview 1
   個だけの最小 WebKitGTK C アプリを書いて計測すると、合計 PSS は約
   296〜299 MiB — これは **Chromium (318〜328 MiB) より軽い**。WebKitGTK
   というエンジン自体が Blink より PSS で重いという証拠はこの環境では
   見つからなかった。VeloX (415〜420 MiB) との差 (約 116〜125 MiB) は、
   §4.3 で計測した「webview 1 個あたりの増分 (約 92〜122 MiB)」とほぼ
   同じ大きさで、toolbar 用の 2 個目の webview 1 個分にほぼ対応する。

**タブ数を増やすとさらに悪化する**: 1/5/10/20 タブで計測すると、VeloX の
1 タブあたりの PSS 増分 (約 106 MiB/タブ) は Chromium (約 9.9 MiB/タブ) の
約 10.7 倍で、20 タブでは VeloX は Chromium の約 5.2 倍の PSS になる。原因
はプロセス数の増え方の違い (VeloX は追加タブ 1 個ごとに正確に +2 プロセス、
Chromium は概ね +1 プロセス) と一致しており、ソースコード上も VeloX が
webview を作るたび (`wry::WebViewBuilder::new()`) に `.web_context(...)`
で既存の `WebContext` を共有させておらず、毎回新しい `WebContext` を
作っていることと符合する (wry 0.56.1 の WebKitGTK バックエンドは
`attributes.context` が渡されなければ新しい `WebContext` を作り、
WebKitGTK は `WebContext` ごとに独立した `WebProcess`/`NetworkProcess` の
プールを持つ)。

**T2 (+10% 以内) は「達成不能」ではない。** #59/T3 (D43) とは逆の結果に
なった — あちらは「VeloX 側で手が出せるはず」の区間が実測するとエンジン
側だったが、こちらは「エンジン差だろう」と予想されていた超過分の大半が、
実測すると VeloX 自身の webview/`WebContext` の使い方に起因していた。
toolbar/content 間で `WebContext` を共有する変更は Epic #57 の「エンジンを
ブラックボックスとして扱う」原則に反しない (WebKit の内部ではなく、
wry への webview の作り方の話)。ただし**これは実装・計測していない仮説で
あり**、実際に効果があるかは #62 で実装して計測するまで分からない
(`docs/decisions.md` D48 に条件を記録した)。

**そのまま使えなかった既存の仕組み**: `velox-bench run --scenario
tabs_1/5/10/20` の `pss_total_bytes` はタブ数によらずほぼ一定 (134〜147
MiB) という、明らかに誤った値を返すことが分かった。原因は RSS/PSS サンプラ
(既定間隔 5000ms、起動直後から即座にサンプリングを開始) がシナリオ全体の
所要時間 (3〜4 秒程度で終わることが多い) より長い間隔で動いているため、
「起動直後の 1 回目」の値しか記録に残らないこと。本 Issue では代わりに
`scripts/bench/tab_scaling.py` を新規に書いて計測した
(`docs/memory-analysis.md` §4.1)。

## 12. T2 — Adaptive Tab Suspension の結果 (Issue #63, 2026-09-04)

**詳細な測定データ・再現手順は [docs/memory-analysis.md](memory-analysis.md)
§11 を、設計判断は `docs/decisions.md` D56 を参照。**

`browser::suspension` に 3 つの独立したシグナル (アイドル時間
`VELOX_AUTO_SUSPEND_AFTER_MS` / 生存タブ上限 `VELOX_MAX_LIVE_TABS` / メモリ
予算 `VELOX_MEMORY_BUDGET_MB`) を持つ適応ポリシーを実装した。**既定はすべて
無効** (D9 のオプトインを維持) で、その場合の挙動と数値は before と同一
(回帰ゲート 3 シナリオ OK、`tab_scaling.py` も誤差範囲)。

| タブ数 | before (MiB) | `VELOX_MAX_LIVE_TABS=4` | `VELOX_MEMORY_BUDGET_MB=700` | Chromium (MiB) | Chromium 比 (上限 4) |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 1  | 409.0  | 409.3  | 407.3  | 281.9 | +45.2% |
| 5  | 653.6  | 553.7  | 659.6  | 320.3 | +72.9% |
| 10 | 990.6  | 419.8  | 476.2  | 366.9 | **+14.4%** |
| 20 | 1612.1 | 560.4  | 615.1  | 466.7 | **+20.1%** |

**本 Issue で分かった、今後の最適化すべてに効く事実**: 同一 `WebKitWebProcess`
内で webview を破棄しても、解放されたヒープはプロセスに残り PSS はほとんど
下がらない (生存 3 ページで 496 MiB の例)。**メモリが確実に OS へ返るのは
プロセスが終了したときだけ**なので、休止はタブ単位ではなくプロセスグループ
単位で行う (空にできる LRU グループを丸ごと落とす) のが正しく、v2 → v3 で
20 タブ 904 → 560 MiB の差になった。これは D54 の「案 A (全タブ 1 プロセス)
でもページ 1 枚あたり 54 MiB」の観測とも整合する: 1 プロセスに集めるほど、
そのプロセスは終了できなくなる。

**復帰コスト** (`tab_resume` シナリオ、`minimal.html`): webview の再構築
2.7ms (中央値) + 再読み込み 10.1ms。D54 の共有プロセスに related view と
して作り直すため、新プロセスの起動を伴わない。

**T2 は未達** (+10% 以内に対し、最良で 10 タブ +14% / 20 タブ +20%)。残りは
(1) 1 タブ時の toolbar 用 `WebProcess` (+45%、休止では届かない)、(2) 生存
タブ分 (上限を下げるほど減るが復帰が増える)、(3) 本計測が `minimal.html`
であること (実サイト未評価)。D56 の Revisit condition を参照。

## 13. タブ生成・切替のベースラインとボトルネック (Issue #60, 2026-09-05)

**設計判断と考察は `docs/decisions.md` D57 を参照。** ここでは数値だけ記録する。

### 13.1 ベースライン (既定設定、`minimal.html`、各 6 試行 = 48 サンプル)

`velox-bench run --scenario tab_create_N|tab_switch_N` で、**すべてのサンプルを
タブ数 N ちょうどで**採ったもの (`mark` による warm-up 切り捨て、D57)。

| タブ数 | `tab_create_ms` | `tab_switch_ms` | `page_load_ms` (新しいタブ) | プロセスに空きがあるか |
| ---: | ---: | ---: | ---: | --- |
| 1  | 2.00 | 0.10 | 6.95  | あり |
| 5  | 2.25 | 0.50 | 6.00  | あり |
| 10 | 2.20 | 0.50 | 6.70  | あり |
| 20 | 2.60 | 0.40 | **14.60** | **無し (20 は上限 4 の倍数)** |

`tab_switch_1` の 0.10ms は「自分自身への切替」で、下限の目安。

### 13.2 ボトルネック: web プロセスを起こすかどうか

20 タブでの `VELOX_MAX_TABS_PER_PROCESS` スイープ (各 3 試行):

| 上限 | グループ構成 | `page_load_ms` | `tab_create_ms` | プロセス数 |
| ---: | --- | ---: | ---: | ---: |
| 4  | 4×5 (満杯)      | 15.8 | 2.75 | 8 |
| 5  | 5×4 (満杯)      | 15.1 | 2.90 | 8 |
| 6  | 6+6+6+2         | 7.3  | 2.15 | – |
| 7  | 7+7+6           | 9.2  | 2.00 | 6 |
| 8  | 8+8+4           | 7.8  | 2.10 | – |
| 9  | 9+9+2           | 6.5  | 2.00 | 6 |
| 10 | 10+10 (満杯)    | 16.1 | 2.70 | 6 |
| 12 | 12+8            | 8.2  | 2.05 | – |

**上限 9 と 10 はプロセス数が同じ 6 なのに 6.5ms と 14.6〜16.1ms に分かれる。**
効いているのはプロセス数ではなく、新しいタブが既存プロセスに相乗りできるか
(空きがあるか) である。相乗りできれば 6.5〜9.2ms、新しいプロセスを起こすなら
14.6〜16.1ms と約 2 倍。

### 13.3 上限 8 との比較 (既定を変えなかった根拠)

| 指標 (20 タブ) | 上限 4 (既定) | 上限 8 | 判定 |
| --- | ---: | ---: | --- |
| PSS | 1652.0 MiB | 1543.7 MiB | 上限 8 が -6.6% |
| プロセス数 | 8 | 6 | 上限 8 が少ない |
| `tab_switch` シナリオ (バースト) `page_load_ms` | 17.6 | 19.8 | 誤差範囲 |
| `tab_resume` シナリオ `page_load_ms` | 7.6 | 8.5 | 誤差範囲 |

D54 が上限を設けた理由 (バースト時のページロード直列化) は、上限ではなく
「読み込み中のプロセスには相乗りしない」規則が抑えていることが確認できた。
それでも既定を 4 のままにした理由は D57 を参照。

### 13.4 再現手順

```sh
S=/path/to/scratch
cargo build --release
(cd scripts/bench/pages && python3 -m http.server 8731 &)
URL=http://127.0.0.1:8731/minimal.html
XV='xvfb-run -a --server-args=-screen 0 1280x900x24 dbus-run-session --'

# 13.1 ベースライン
for n in 1 5 10 20; do
  $XV target/release/velox-bench run --scenario tab_create_$n --trials 6 \
    --velox-bin target/release/velox --url $URL --output $S/create-$n.json
  $XV target/release/velox-bench run --scenario tab_switch_$n --trials 6 \
    --velox-bin target/release/velox --url $URL --output $S/switch-$n.json
done

# 13.2 上限スイープ (順序の影響を避けるため上限をラウンドロビンで回す)
for trial in 1 2 3; do for cap in 4 5 6 7 8 9 10 12; do
  VELOX_MAX_TABS_PER_PROCESS=$cap $XV target/release/velox-bench run \
    --scenario tab_create_20 --trials 1 --velox-bin target/release/velox \
    --url $URL --output $S/cap-$cap-$trial.json
done; done

# 13.3 メモリ
for cap in 4 8; do
  VELOX_MAX_TABS_PER_PROCESS=$cap $XV python3 scripts/bench/tab_scaling.py \
    --velox target/release/velox --page minimal.html --tab-counts 1,5,10,20 --trials 3
done
```

### 13.5 計測上の注意 (実際に踏んだ罠)

**同一バイナリ・同一スクリプトでも、実行順で結果が 3 倍変わる。** 最初の調査で
`page_load_ms` が 20 タブで 25.4ms と出たが、同じ設定を後から測ると 7.8ms
だった。差はタブ数ではなく「その run がその日の 1 本目か」で、ページキャッシュ
などのウォームアップが効いている。**条件をまとめて連続実行すると、最初に測った
条件だけが不当に遅く出る。** 条件はラウンドロビンで回し、1 回目は捨てること。

## 14. バックグラウンドタブの CPU (Issue #64, 2026-09-05)

**設計判断と考察は `docs/decisions.md` D58 を参照。**

### 14.1 実測 (各 3 試行、16 秒窓、`scripts/profile/cpu_usage.py`)

負荷源は `scripts/bench/pages/busy.html` (`requestAnimationFrame` ループ +
10ms タイマー + CSS アニメーションで実際に CPU を焼く)。1 コアを 100% と
した比率。

| 状態 | CPU | 内訳 |
| --- | ---: | --- |
| busy がアクティブタブ | **100.0〜101.2%** | WebProcess 92〜93% + `velox` 本体 7.7〜7.9% |
| busy がバックグラウンドタブ | **0.4〜0.6%** | WebProcess 0.3% |
| busy を休止 (#63) | 0.0〜0.2% | — |
| 静的ページ 2 タブ (対照) | 0.1% | — |

**バックグラウンド化だけで 99.4% 減る。** VeloX 側の実装によるものではなく、
タブ切替の `set_visible(false)` が GTK ウィジェットを hide し、WebKitGTK が
そのページを「隠れている」と扱う結果である (D58)。

### 14.2 止まってはいない (`?beacon=1` による外形計測)

| 状態 | ビーコン到達 | 元の間隔 |
| --- | ---: | --- |
| visible | 2.17 件/秒 | 500ms タイマー = 2 件/秒 |
| hidden  | 1.10 件/秒 | 約 1000ms に間引き |

タイマーは約 1/2 の頻度で回り続け、バックグラウンドからのネットワーク
リクエストも通る。`document.visibilityState` が `hidden` を返していることも
同時に確認できる。

### 14.3 2 つの計測手段の使い分け

| 手段 | 出せるもの | 注意 |
| --- | --- | --- |
| `scripts/profile/cpu_usage.py` | **絶対値** (「何 % 使っているか」) | VeloX の外から `/proc` を 2 点だけ読むので測定コストが被測定側に乗らない |
| `velox-bench run --scenario background_cpu` の `cpu_percent` | **回帰検知** (before/after 比較) | VeloX 自身のサンプラが /proc を歩くコストが乗る (この環境で数 %)。同一シナリオ同士なら打ち消し合う |

この環境での `background_cpu` の `cpu_percent` は中央値 5.45% (3 試行、
12 サンプル)。上表の 0.5% と食い違うのはサンプラ自身のコストで、**絶対値を
語るときは `cpu_usage.py` の数字を使うこと。**

### 14.4 再現手順

```sh
P=$PWD/scripts/bench/pages
XV='xvfb-run -a --server-args=-screen 0 1280x900x24 dbus-run-session --'

# 14.1 アクティブ / バックグラウンド
printf 'wait 60000\nquit\n' > /tmp/active.txt
printf 'open file://%s/busy.html\nwait 1500\nopen file://%s/minimal.html\nwait 60000\nquit\n' \
  $P $P > /tmp/bg.txt
VELOX_MAX_TABS_PER_PROCESS=1 $XV python3 scripts/profile/cpu_usage.py \
  --velox target/release/velox --script /tmp/active.txt \
  --homepage file://$P/busy.html --label active --window-secs 16
VELOX_MAX_TABS_PER_PROCESS=1 $XV python3 scripts/profile/cpu_usage.py \
  --velox target/release/velox --script /tmp/bg.txt \
  --homepage file://$P/minimal.html --label background --window-secs 16

# 14.3 回帰検知用のシナリオ
(cd scripts/bench/pages && python3 -m http.server 8731 &)
$XV target/release/velox-bench run --scenario background_cpu --trials 3 \
  --velox-bin target/release/velox --url http://127.0.0.1:8731/busy.html
```

## 15. バックグラウンドタブのネットワーク活動 (Issue #65, 2026-09-07)

**設計判断と考察は `docs/decisions.md` D80 を参照。** 測定環境は §1 のとおり
(このリポジトリの Linux/WebKitGTK 環境。Windows/macOS の実力値ではない)。

負荷源は `scripts/bench/pages/network_activity.html` (新規、Issue #65)。
サーバ側のアクセスログで数える `scripts/profile/network_activity.py` を
使う — VeloX 自身にはリクエスト単位のイベントが存在しない理由は
スクリプト冒頭のコメントと D80 を参照。

### 15.1 典型的なポーリング間隔 (2 秒) — 各 3 試行、16 秒窓

| 状態 | polling (件/16s) | background resource (件/16s) | websocket (件/16s) | 合計 |
| --- | ---: | ---: | ---: | ---: |
| アクティブタブ | 5, 5, 5 | 4, 4, 4 | 6, 6, 6 | 15, 15, 15 |
| バックグラウンドタブ | 5, 6, 5 | 4, 4, 4 | 6, 7, 6 | 15, 17, 15 |

有意差なし。

### 15.2 高頻度ポーリング (`?poll_ms=200`) — 各 3 試行、8 秒窓

| 状態 | polling (件/8s) | background resource (件/8s) | websocket (件/8s) |
| --- | ---: | ---: | ---: |
| アクティブタブ | 31, 31, 31 | 2, 2, 2 | 4, 4, 4 |
| バックグラウンドタブ | 8, 8, 8 | 2, 2, 2 | 5, 5, 5 |

polling のみ 74% 減 (3.9→1.0 件/秒)。1 秒以上の間隔を持つ他 2 系統は
このケースでも間引かれていない。

### 15.3 prefetch — 全試行で 0 件

`GET /prefetch-target` は §15.1/§15.2 のどの試行 (アクティブ・
バックグラウンドとも計 12 試行) でも 1 件も記録されなかった。一方
「`<link rel=prefetch>` を挿入した直後」に無条件で送る `GET
/prefetch-armed` は毎回 1 件記録されている — スクリプト自体は実行された
が、WebKitGTK 2.52.6 がこの prefetch ヒントを実行しない。

### 15.4 Adaptive Tab Suspension (#63) との連携 — 各 3 試行、16 秒窓 (`--suspend-after-ms 3000`, `--settle-secs 8`)

| 状態 | 合計イベント数 (16s窓) |
| --- | ---: |
| バックグラウンド、suspension 無効 (§15.1 と同条件) | 15, 17, 15 |
| バックグラウンド、suspension 有効・休止後 | **0, 0, 0** |

### 15.5 再現手順

```sh
XV='xvfb-run -a --server-args=-screen 0 1280x900x24 dbus-run-session --'

# 15.1: 通常のポーリング間隔、アクティブ vs バックグラウンド
printf 'wait 16000\nquit\n' > /tmp/na_active.txt
printf 'wait 1500\nopen about:blank\nwait 16000\nquit\n' > /tmp/na_bg.txt
$XV python3 scripts/profile/network_activity.py --velox target/release/velox \
  --script /tmp/na_active.txt --label active --settle-secs 6 --window-secs 16
$XV python3 scripts/profile/network_activity.py --velox target/release/velox \
  --script /tmp/na_bg.txt --label background --settle-secs 6 --window-secs 16

# 15.2: 高頻度ポーリング (?poll_ms=200)
printf 'wait 10000\nquit\n' > /tmp/na_fast_active.txt
printf 'wait 1500\nopen about:blank\nwait 10000\nquit\n' > /tmp/na_fast_bg.txt
$XV python3 scripts/profile/network_activity.py --velox target/release/velox \
  --script /tmp/na_fast_active.txt --label fast_active --fixture-query "poll_ms=200" \
  --settle-secs 4 --window-secs 8
$XV python3 scripts/profile/network_activity.py --velox target/release/velox \
  --script /tmp/na_fast_bg.txt --label fast_background --fixture-query "poll_ms=200" \
  --settle-secs 4 --window-secs 8

# 15.4: suspension との連携 (idle_after を意図的に短くする)
printf 'wait 1500\nopen about:blank\nwait 26000\nquit\n' > /tmp/na_suspend.txt
$XV python3 scripts/profile/network_activity.py --velox target/release/velox \
  --script /tmp/na_suspend.txt --label suspended --settle-secs 8 --window-secs 16 \
  --suspend-after-ms 3000
## 16. メモリ/リソースライフタイム監査 (Issue #62, 2026-09-07)

**詳細な調査・実測データ・再現手順は [docs/memory-analysis.md](memory-analysis.md)
§12 を、判断の根拠は `docs/decisions.md` D79 を参照。要点のみここに残す。**

Issue #62 は #61/#63/#64 のどの計測にも無かった軸 — **タブ数を一定に保った
まま開閉「だけ」を繰り返す (churn) と PSS がじわじわ増えないか** — を新設の
`scripts/bench/tab_churn.py` で測った (`browser::automation` スクリプトで
「K 個開く→落ち着かせる→K 個とも閉じて 1 タブに戻す→落ち着かせる」を 1
ラウンドとして繰り返し、ラウンド境界ごとに PSS を採る)。

**コード上のリソースライフタイム監査で 1 件、本物のバグを見つけて修正した**:
`app::run` の `page_load_timers` (タブ latency 計測用マップ、`perf_metrics`
有効時のみ書き込まれる) が、タブを閉じてもエントリを一度も削除しておらず、
`WindowId`/`TabId` は再利用されないため**開いたタブの延べ数に比例して
無制限に増え続けていた**。`LoadFinished` でエントリを `remove` し、
`close_tab`/`close_window_by_tao_id` でも明示的に破棄するよう修正
(`src/app.rs`)。回帰ゲート (`cold_startup`/`tab_create`/`tab_switch`、
baseline 8 試行 + candidate 2×8 試行) は 3 シナリオとも総合判定 OK。ただし
このエントリ 1 件は高々 100 バイト程度で、本 Issue で実際に流せた churn の
規模 (最大 72 回の open/close) では `/proc` 経由の PSS/RSS 計測でこの修正の
効果を独立に検出できるだけの大きさにならなかった (このコンテナには
`heaptrack` が無く、malloc 単位の独立検証もできなかった) — 正しさの根拠は
単体/統合テストによる直接確認であり、PSS 計測で改善を実測したわけではない。

**churn そのものによる PSS 増加は観測されたが、無制限ではなく有界だった**:
`minimal.html`・6 タブ/ラウンド・12 ラウンド (のべ 72 回の open/close、42.1
秒) では round 1 (558.3 MiB) → round 2 (638.7 MiB) で跳ねた後、round 3〜12
は 581〜693 MiB の範囲で**増加が止まる** (round 3〜12 の最小二乗傾きは
-0.756 MiB/round)。プロセス数は全ラウンドを通じて一定 (4)。#63 (D56) が
「休止でも解放ヒープはプロセスに残り、プロセス終了だけが確実に OS へ返す」
と結論した現象が、休止だけでなく本当の tab close (相乗りしているプロセス
の一部だけ閉じる場合) にも及ぶことを示す一方、**アロケータのアリーナが
一度ピーク相当まで育った後は使い回されるだけで、無制限には育たない**
(いわゆる leak ではなく、有界な 1 回きりのウォームアップコスト) ことも
同時に確認できた。よって本 Issue ではプロセスグループの寿命ベース recycle
のような新規の設計変更には着手せず、D79 に Revisit condition として残した。

**「1 時間以上の連続利用」の外挿について**: 実際に流せたのは最大 15
ラウンド (60 回の open/close、約 39 秒) までで、1 時間相当 (1000+ ラウンド)
は本 Issue でも実測していない。`tab_churn.py` は実測区間の平均ラウンド
所要時間から単純な線形外挿も出力するが、実測データ自体が「数ラウンドで
頭打ちになる非線形な形」を示しているため、この線形外挿の数値そのものは
信頼できる予測として使わない — 出力にもその旨を明記している。

再現コマンド:

```sh
xvfb-run -a --server-args="-screen 0 1280x900x24" dbus-run-session -- \
  python3 scripts/bench/tab_churn.py --velox target/release/velox \
    --page minimal.html --rounds 12 --tabs-per-round 6 \
    --output results/tab-churn.json
```
## 17. Performance Dashboard (Issue #71, 2026-09-07)

**設計・実装の詳細は [docs/performance-dashboard.md](performance-dashboard.md)
を、設計判断は `docs/decisions.md` D82 を参照。** ここでは §10 (回帰ゲート)
との関係だけを記す。

`velox-bench gate` (§10) は CI が PR をブロックするかどうかを**同一ジョブ内で
測った baseline/candidate** から判定する。それに対し Performance Dashboard
(`scripts/dashboard/`) は、**過去の計測結果を `results/history/` に追記保存し、
人間が経時トレンドを一覧できる静的レポートを生成する**別の役目を持つ —
CI のブロッキング判定を置き換えるものではない。

保存形式は `velox-bench run`/`aggregate`/`gate` が使う `BenchmarkResult`
JSON をそのまま包んだものにしてあり (§7 の保存形式と地続き)、perf-gate の
出力形式とダッシュボードの保存形式が食い違わないようにしてある。

**この文書 §10/D46 が明らかにした「異なるセッション/マシンの数値を比較して
はならない」という原則は、ダッシュボードの設計そのものに組み込まれている**:
比較 (差分の計算・折れ線での接続) は明示的に同一とマークされた
「セッション」の中でだけ行われ、セッションを跨いだ点は時系列上に並べて
表示はするが、線ではつながず、差分も計算しない。判定ロジックそのものも
独自実装ではなく `velox-bench gate` (`evaluate_gate`) をそのまま呼び出す
ため、CI と同じ閾値 (`GateThresholds` 既定値 warn=20%/fail=60% + メトリクス
ごとの最小絶対差) が二重管理にならない。詳細は
`docs/performance-dashboard.md` §2/§5 を参照。
## 18. IPC (WebView ↔ Rust) の計測結果 (Issue #66, 2026-09-07)

**設計判断は `docs/decisions.md` D81 を参照。** ここでは実測データと
結論だけを記録する。この節の数値はすべて §1 の環境 (Ubuntu 24.04.4 /
WebKitGTK 2.52.6 / Xvfb、GPU なし) での計測であり、**Windows
(WebView2) の実力値ではない** — IPC の実装 (wry の `evaluate_script`/
`with_ipc_handler`) は OS 間でほぼ同じコードパスだが、未計測の OS へ
そのまま外挿しないこと。

### 18.1 何を計測できるようにしたか

`metrics::PerfRecord::Ipc` (Issue #66) が JS → Rust (`direction=in`、
`ToolbarCommand`) と Rust → JS (`direction=out`、
`ui::window::BrowserWindow::eval_toolbar` の呼び出し元名) の両方向を
1 メッセージ単位で記録する。`velox-bench ipc-summary` がそれを
`(direction, name)` ごとに件数・合計バイト数・`duration_ms` 分布へ集計する
(`docs/benchmarking.md` §6)。**`duration_ms` は Rust 側のコスト
(`parse_command`/`evaluate_script` の呼び出し) のみで、JS 実行や DOM
更新は含まない** — Epic #57 ルール 3 (WebView をブラックボックスとして
扱う) のとおり、VeloX 側から測れるのはここまで。

### 18.2 セッション実測 (`velox-bench ipc-summary`)

自動操作スクリプトでタブ 20 個 (`minimal.html`) を開き、10 回切替 + 2 回
ナビゲーション + 2 回クローズを行う「20 タブセッション」と、タブ 3 個で
同種の操作を行う「3 タブセッション」を 1 回ずつ実行 (各 1 試行、
`VELOX_MAX_TABS_PER_PROCESS=4`、既定)。再現手順は §18.5。

**20 タブセッション、修正前 (this issue 着手前のコード) の内訳
(上位 5、`total_bytes` 降順)**:

| dir | name | count | total_bytes | median_ms | p95_ms |
| --- | --- | ---: | ---: | ---: | ---: |
| out | `set_tabs` | 120 | 256,648 | 0.000 | 0.105 |
| out | `set_history` | 67 | 15,723 | 0.000 | 0.100 |
| out | `set_url` | 91 | 4,536 | 0.000 | 0.200 |
| out | `set_bookmark_active` | 91 | 2,730 | 0.000 | 0.100 |
| out | `set_loading` | 91 | 2,033 | 0.000 | 0.100 |
| — | 合計 | 502 | 284,268 | — | — |

`in` 側は `script_started`(24 bytes)/`ready`(15 bytes) の 2 件のみ —
16.4 で理由を説明する。

### 18.3 高頻度イベントの特定と結論

- **`set_tabs` (タブストリップの全件再送信) が量・回数とも最大**
  (20 タブセッションで合計バイト数の 90%)。1 タブ操作 (open/navigate/
  close/switch) につき最大 3 回 (`NavigationStarted`/`LoadFinished`/
  `FaviconResolved` それぞれが `sync_tab_strip` を呼ぶ) 送られており、
  120 件 / 約 34 回のタブ影響操作という比率もこれと整合する。**しかし
  実測コストは無視できる規模だった**: 20 タブという本プロジェクトで
  最も重いケースでも `duration_ms` の中央値は 0.000ms、p95 でも
  0.105ms、120 件中の最悪値でも 3.5ms (1 件のみ、他はすべて 2ms 未満)。
  1 メッセージの最大ペイロードも 3,585 bytes (20 タブ分の
  `TabSummary` 配列) で、JSON 化・`evaluate_script` 呼び出しという
  Rust 側の処理は sub-millisecond。タブストリップは**常時表示**の UI
  であり、3 回の送信はそれぞれ「読込中インジケータ」「URL/タイトル」
  「favicon」という実際に変化した状態を反映しているため、**意図的な
  設計であり、バッチ化・削減の実測上の必要性は見つからなかった。**
  T4 (タブ切替 100ms 以下、#60 で 0.40ms 達成済み) を脅かす要素はない。
- **`set_history` (履歴パネルの全件再送信) は不要イベントだった。**
  `refresh_history_panel` は履歴パネルが**閉じていても**
  `LoadFinished`/`PageTitleResolved`/`FaviconResolved` の 3 箇所から
  無条件に呼ばれており、閲覧のたびに `config.history_panel_limit`
  (既定 200 件) 分の履歴を JSON 化して送っていた。履歴パネルは
  ほとんどの時間閉じているため、この送信の大部分は**誰にも見られない
  DOM 更新**だった。§18.4 で before/after を示す。
- それ以外の `out` イベント (`set_url`/`set_bookmark_active`/
  `set_loading`/`set_block_count` など) は 1 件あたり数十バイト、
  `duration_ms` はほぼ 0 — 削減の対象にならない規模。

### 18.4 実施した削減と before/after (同一自動操作スクリプトでの比較)

`app::refresh_history_panel_if_open` を追加し、`LoadFinished`/
`PageTitleResolved`/`FaviconResolved` の 3 箇所を
`refresh_history_panel`(無条件) から `refresh_history_panel_if_open`
(`window.open_panel() == Some(Panel::History)` のときだけ実際に送信)
に変更した。パネルを開く操作 (`ToolbarCommand::TogglePanel`) は
既存のまま無条件に `refresh_history_panel` を呼ぶので、**パネルを
開いた瞬間に最新データが表示される挙動は変わらない** — 変わるのは
「閉じている間、誰も見ない更新を送り続けない」点のみ。

| セッション | 指標 | before | after | 変化 |
| --- | --- | ---: | ---: | ---: |
| 20 タブ | `set_history` 件数 | 67 | 1 | **-98.5%** |
| 20 タブ | `set_history` bytes | 15,723 | 555 | **-96.5%** |
| 20 タブ | ipc イベント総数 | 502 | 430 | -14.3% |
| 20 タブ | ipc 総バイト数 | 284,268 | 269,277 | -5.3% |
| 3 タブ | `set_history` 件数 | 13 | 1 | **-92.3%** |
| 3 タブ | `set_history` bytes | 7,614 | 887 | **-88.4%** |
| 3 タブ | ipc イベント総数 | 96 | 84 | -12.5% |
| 3 タブ | ipc 総バイト数 | 20,262 | 13,917 | -31.3% |

(`set_tabs` は before/after で変化なし — 120 件 → 120 件、256,648 →
257,023 bytes。誤差は自動操作スクリプトの実行ごとの URL/タイトル文字数の
揺れによるもので、意図的な変更はしていない。各 1 試行の実測であり、
セッション間ノイズ〔§10〕を統計的に切り分けられるほどの試行数ではない
ことに注意。)

`cargo test` は本変更後も全件成功 (946 ユニットテスト + 10 統合テスト、
xvfb-run + dbus-run-session)。履歴パネル自体の動作 (開いたときに最新の
履歴が表示される、検索、削除、クリア) は `ToolbarCommand::TogglePanel`
/`DeleteHistoryEntry`/`ClearHistory`/`SearchHistory` の呼び出し経路を
変更していないため影響を受けない。

### 18.5 結論

- **IPC の回数・サイズ・時間を継続的に計測できる仕組み**: `metrics::
  PerfRecord::Ipc` + `browser::benchmark::summarize_ipc` +
  `velox-bench ipc-summary` (`docs/benchmarking.md` §6)。#67/#68/#69 は
  このまま (追加の型を作らず) 使える。
- **高頻度イベントの特定**: `set_tabs`(常時表示、意図的、コスト無視できる)、
  `set_history`(閉じたパネルへの無駄な送信、削減済み)。
- **不要イベントの削減**: `set_history` の無条件送信を撤廃 (§18.4)。
- **batching の要否**: **導入しなかった。** `set_tabs` を含むすべての
  `out` イベントの `duration_ms` が sub-millisecond (worst case でも
  3.5ms) であり、複数イベントをまとめて 1 回の `evaluate_script`
  呼び出しに合成する batching は、計測上のボトルネックが無い状態で
  タイマー・デバウンスロジックという複雑さとステイル状態のリスクだけを
  持ち込む — Epic #57 ルール 1 (ベンチマークなしの最適化をしない) に
  照らして見送った。
- **`direction=in` (JS → Rust) の限界**: 自動操作スクリプト
  (`open`/`switch`/`navigate`/`close`) は `AutomationCommand` として
  `app.rs` のハンドラを直接呼ぶため、実際の `window.ipc.postMessage`
  を経由しない (`docs/decisions.md` D81 に理由を記録)。したがって
  `in` 側の実測は起動時ハンドシェイク (`ready`/`script_started`、
  15〜24 bytes、`duration_ms` は測定分解能以下) に限られる。**実際の
  ユーザ操作 (クリック・キー入力) が送る `navigate`/`activate_tab`
  等の `in` メッセージは `ui::toolbar::ToolbarCommand` の定義上、数十
  バイトの固定形状 JSON であることがソースコードから明らかであり**
  (例: `{"cmd":"activate_tab","id":42}` は 27 bytes)、`out` 側で
  実測した「同程度サイズのメッセージは sub-millisecond」という結果と
  合わせて、`in` 側もボトルネックである根拠は無いと判断した。ただし
  これは実測ではなく推論であることを明記する — オムニボックスの
  1 キー入力ごとの `omnibox_input`/`omnibox_close` 往復のような、
  自動操作スクリプトの文法 (`browser::automation`) では今のところ
  再現できない高頻度パスは**未計測**として残す (`docs/decisions.md`
  D81)。

### 18.6 再現手順

```sh
S=/path/to/scratch
cargo build --release
(cd scripts/bench/pages && python3 -m http.server 8731 &)
XV='xvfb-run -a --server-args=-screen 0 1280x900x24 dbus-run-session --'

cat > $S/session.txt << 'SCRIPT'
open http://127.0.0.1:8731/minimal.html
wait 300
(... 18 回繰り返して合計 20 タブ ...)
mark
switch 0
wait 200
(... switch/navigate/close を繰り返す ...)
quit
SCRIPT

$XV env VELOX_PERF_METRICS=1 VELOX_PERF_FORMAT=json \
  VELOX_PERF_OUTPUT=$S/session.jsonl \
  VELOX_HOMEPAGE=http://127.0.0.1:8731/minimal.html \
  VELOX_AUTOMATION_SCRIPT=$S/session.txt \
  target/release/velox

target/release/velox-bench ipc-summary --input $S/session.jsonl \
  --output $S/ipc-summary.json
```

## 19. Browser State / Event Dispatch の計測結果 (Issue #67, 2026-09-07)

**設計判断は `docs/decisions.md` D86 を参照。** ここでは実測データと
結論だけを記録する。この節の数値はすべて §1 の環境 (Ubuntu 24.04.4 /
WebKitGTK 2.52.6 / Xvfb、GPU なし) での計測であり、**Windows
(WebView2) の実力値ではない**。

### 19.1 何を計測できるようにしたか

`metrics::PerfRecord::StateWrite`(Issue #67)が `persistence::save_*`
(`session`/`history`/`bookmarks`/`input_history`)の呼び出しを 1 回ごとに
記録する。§18 の `PerfRecord::Ipc`が計測する「プロセス内 IPC
(`evaluate_script`呼び出し)」とは別物で、こちらは**実際の同期ディスク
I/O**(`fs::create_dir_all` + `serde_json::to_string_pretty` +
`fs::write`)のコストを計測する。同じ `VELOX_PERF_OUTPUT`ログに
`event=state_write`として混在するので、§18.6 と同じログファイルから
`event=="state_write"`の行を抜き出すだけで見える。

### 19.2 セッション実測

§18.2 と同じ「20 タブセッション」「3 タブセッション」を、`persist_
session`の冗長書き込み削減 (D86) の前後でそれぞれ実行した (各 1〜2
試行、再現手順は 19.5)。

**20 タブセッション、`state_write name=session`**:

| 指標 | 修正前 | 修正後 (試行1) | 修正後 (試行2) |
| --- | ---: | ---: | ---: |
| 件数 | 150 | 82 | 82 |
| duration_ms 合計 | 50.7 | 16.7 | 51.7 |
| duration_ms 中央値 | 0.100 | 0.100 | 0.100 |
| duration_ms p95 | 0.300 | 0.200 | 0.300 |
| duration_ms 最悪値 | 10.8 | 4.2 | 38.2 |

件数は **150 → 82 (-45.3%)** — 2 回の修正後試行でどちらも 82 と完全に
一致した (自動操作スクリプトが決定的で、削減対象が制御フローそのもの
であり、タイマー由来のジッタではないため)。`duration_ms`の合計は
試行によって振れる (試行2 は 38.2ms の外れ値 1 件に支配されている —
共有 VM 上のスケジューリング揺らぎとみられ、§10 のセッション間ノイズと
同種) が、**中央値・p95 は前後でほぼ不変**。つまり削減されたのは
「1 回あたりの書き込み速度」ではなく「書き込みが呼ばれる回数」である。

**3 タブセッション、`state_write name=session`**:

| 指標 | 修正前 | 修正後 |
| --- | ---: | ---: |
| 件数 | 39 | 22 |
| duration_ms 合計 | 8.6 | 3.2 |

件数は **39 → 22 (-43.6%)**、20 タブセッションと同傾向。

**対照 (変更していない `persist_history`)**: 同じセッションで
`state_write name=history`の件数は 20 タブで **69 → 69**、3 タブで
**18 → 18**、まったく変化なし — 削減が意図した `persist_session`
だけに効いていることの裏付け。

### 19.3 tab lookup / lock contention

いずれも計測を組む前の設計調査だけで「本 Issue の対象外」と判断した。
理由と根拠は D86 を参照 (要約: lock contention は
`docs/architecture.md`の「全状態はメインスレッドの `UserEvent`
ディスパッチに集約、ロックなし」という既存設計そのものにより発生し
えない。tab/window lookup の `Vec` 線形走査は、走査対象が構造的に小さい
〔タブ数は重量級シナリオでも上限 20〜50、ウィンドウ数は実運用でまず
1〜3〕上に、その全走査を伴う `sync_tab_strip`の `set_tabs`構築コストが
§18 で既に sub-millisecond と実測済みであり、追加のマイクロベンチマークを
組んでも実測ノイズに埋もれる可能性が高いと判断した)。

### 19.4 結論

- **state-write の計測を継続的に行える仕組み**: `metrics::PerfRecord::
  StateWrite` + `app::record_state_write`。#68 はこのまま使える
  (D86 の Revisit condition 参照)。
- **見つかった唯一の redundant update**: `sync_tab_strip`から無条件に
  呼ばれていた `persist_session`(セッションスナップショットの全件
  ディスク書き込み)。`SessionSnapshot`が保持しない`loading`フラグの
  変化だけでも書き込みが走っていた。
- **実施した削減**: 直近に書き込んだスナップショットと比較し、一致
  すれば書き込みをスキップ (§19.2 参照)。batching は導入していない —
  「送るか送らないか」の判断で完結しており、複数書き込みを 1 回に
  まとめる必要が生じる規模のボトルネックではなかった。
- **削減しなかった箇所**: `persist_history`/`persist_bookmarks`/
  `persist_input_history`(実際の内容変更ごとに呼ばれており冗長では
  ない。20 タブセッションで合計 8.3ms、削減を要する規模ではない)。
  `write_json`内の`fs::create_dir_all`(呼び出しごとの stat 相当の
  syscall だが、実測 (中央値 0.1ms) の範囲では埋没している)。tab/
  window lookup、event routing の `match`ディスパッチ、lock
  contention (§19.3)。
- **Regression check**: `velox-bench gate --scenario cold_startup`
  (baseline=修正前コミット、candidate=修正後、各 8 試行 × 2 回) は
  総合判定 OK (`rss_total_bytes`が 1 回だけ WARN を出したが、修正前
  バイナリ同士の比較でも `page_load_ms`が -46.9% 振れるなど同程度の
  ノイズが再現し、かつ `persist_session`はコールドスタート経路では
  一切呼ばれないため、この変更に起因するものではないと判断した — 直後
  の再試行 2 回はいずれも総合判定 OK)。`--scenario tab_create_20`
  (baseline/candidate 各 5 試行 × 2 回) も総合判定 OK。
  `cargo test`は変更後も全件成功 (972 ユニットテスト + 11 統合テスト、
  xvfb-run + dbus-run-session)。

### 19.5 再現手順

§18.6 と同じ環境構築 (HTTP サーバ、`velox`/`velox-bench`のビルド) の後:

```sh
S=/path/to/scratch
cat > $S/session.txt << 'SCRIPT'
open http://127.0.0.1:8731/minimal.html
wait 300
(... open/wait を計 20 回 (最初の 1 回を含む) ...)
mark
(... switch 0..19 を 10 回、switch+navigate を 2 回、close を 2 回 ...)
quit
SCRIPT

$XV env VELOX_PERF_METRICS=1 VELOX_PERF_FORMAT=json \
  VELOX_PERF_OUTPUT=$S/session.jsonl \
  VELOX_DATA_DIR=$S/data \
  VELOX_HOMEPAGE=http://127.0.0.1:8731/minimal.html \
  VELOX_AUTOMATION_SCRIPT=$S/session.txt \
  target/release/velox

python3 -c "
import json
from collections import defaultdict
rows = defaultdict(list)
for line in open('$S/session.jsonl'):
    d = json.loads(line)
    if d.get('event') == 'state_write':
        rows[d['name']].append(d['duration_ms'])
for name, vals in rows.items():
    print(name, 'count', len(vals), 'sum_ms', round(sum(vals), 3))
"
```
## 20. ページロードの段階計測 (Issue #69, 2026-09-07)

**設計判断・調査の経緯は `docs/decisions.md` D87 を参照。** ここでは
実測データと再現手順だけを記録する。この節の数値はすべて §1 の環境
(Ubuntu 24.04.4 / WebKitGTK 2.52.6 / Xvfb、GPU なし) での計測であり、
**Windows (WebView2) / macOS (WKWebView) の実力値ではない** — D87 の
とおり `PageLoadEvent::Started`/`Finished` の意味づけは3バックエンド
共通と wry のソースで確認したが、実際のミリ秒はこの環境固有。

### 20.1 何を計測できるようにしたか

`metrics::PageLoadTimer`(既存、Issue #13) に `mark_load_started` という
任意のチェックポイントを追加し、`page_load` イベントの `duration_ms`
(既存、`NavigationStarted → LoadFinished`) はそのままに、
`engine_duration_ms`(`LoadStarted → LoadFinished`)・
`dispatch_duration_ms`(`NavigationStarted → LoadStarted`、`duration_ms
- engine_duration_ms`) を追加した。`LoadStarted` が来なかったロードは
両方とも `null`。`browser::benchmark::MetricKey` に
`PageLoadEngineMs`/`PageLoadDispatchMs` を追加済み。

**`dispatch_duration_ms` は VeloX 自身のコストではない** — D87 で
ソースを確認したとおり `LoadStarted` はエンジンがロードを commit した
後 (接続・リクエスト送信・レスポンス受信開始後) に発火するため、この
区間にはエンジン側のネットワーク待ちも混ざる。以下の数値を読むときは
必ず D87 の但し書きと合わせて読むこと。

### 20.2 `navigation` シナリオでの実測 (同一セッション内 before/after、各10試行=50サンプル)

`before` = 本 Issue 着手前のコード (`461f435`)、`after` = 本 Issue の
計装追加後 (挙動を変える変更はしていない、`page_load_ms` の計測経路も
不変)。`minimal.html` に対して2ラウンドずつ実行 (交互実行、§13.5 の
「実行順で最初の条件だけ不当に遅くなる」教訓を踏まえた順序):

| ラウンド | ビルド | `page_load_ms` 中央値 | `page_load_engine_ms` 中央値 | `page_load_dispatch_ms` 中央値 |
| --- | --- | ---: | ---: | ---: |
| 1 | before | 6.15 | (フィールド無し) | (フィールド無し) |
| 1 | after  | 5.95 | 1.40 | 4.65 |
| 2 | before | 6.90 | (フィールド無し) | (フィールド無し) |
| 2 | after  | 6.70 | 1.65 | 4.70 |

`page_load_ms`(既存メトリクス) は before/after でほぼ同じ (5.95〜6.90ms
の範囲内、`velox-bench gate` で回帰なしと判定 — §20.3) — 計装追加は
`page_load_ms` の値にも計測経路にも影響していない。新しく見えるように
なった内訳: **`dispatch`(4.65〜4.70ms) が `engine`(1.40〜1.65ms) より
大きい** — ループバック HTTP サーバの `minimal.html`(ほぼ空、DNS/TLS
コスト無し) に対してもこの関係が成り立つ。D87 が指摘するとおり、これは
「VeloX のオーバーヘッドが半分以上」ではなく「エンジンの接続確立/
リクエスト送受信の待ち時間がこの区間の大半」と読むべきで、根拠は
#66/D81 が実測した IPC Rust 側コスト (sub-millisecond) との対比。

より重いページ (`dom_heavy.html`、`after` ビルドのみ、10試行=50サンプル)
で計測すると、この構造が裏付けられる:

| メトリクス | `minimal.html` 中央値 | `dom_heavy.html` 中央値 |
| --- | ---: | ---: |
| `page_load_ms` | 5.95〜6.70 | 84.65 |
| `page_load_engine_ms` | 1.40〜1.65 | 72.00 |
| `page_load_dispatch_ms` | 4.65〜4.70 | 12.65 |

ページが重くなるほど伸びるのは `engine`(1.4ms→72.0ms) であって
`dispatch`(4.7ms→12.65ms、オーダーは同じ) ではない — ページの中身を
処理するコストが `engine` 側に乗っている、という直感どおりの結果。

### 20.3 `velox-bench gate` — 計装追加による回帰の有無

```
regression gate: scenario=navigation candidates=2 (warn>20.0% fail>60.0%)
metric                             baseline       candidates (中央値/変化率)       判定       備考
page_load_ms                           6.15     5.9(-3.3%), 6.7(+8.9%)       OK
pss_process_count                      4.00     4.0(+0.0%), 4.0(+0.0%)       OK
pss_total_bytes                106436608.00 95926784.0(-9.9%), 95651328.0(-10.1%)       OK
rss_process_count                      4.00     4.0(+0.0%), 4.0(+0.0%)       OK
rss_total_bytes                292995072.00 288542720.0(-1.5%), 291096576.0(-0.6%)       OK
startup_first_load_ms                315.10 297.1(-5.7%), 312.2(-0.9%)       OK
startup_rust_setup_done_ms           130.10 124.8(-4.1%), 128.6(-1.2%)       OK
startup_toolbar_ready_ms             285.60 274.5(-3.9%), 283.0(-0.9%)       OK
startup_toolbar_script_started_ms         285.45 274.3(-3.9%), 282.6(-1.0%)       OK
startup_window_created_ms            129.95 124.5(-4.2%), 128.2(-1.3%)       OK
candidate のみに存在: page_load_dispatch_ms, page_load_engine_ms

総合判定: OK
```

baseline (`before` ラウンド1) に対し `after` の2ラウンドを候補として
評価 — 総合判定 OK。`page_load_dispatch_ms`/`page_load_engine_ms` は
baseline 側に存在しない新規メトリクスなので `gate` は個別の合否を出さず
「candidate のみに存在」と表示する (`evaluate_gate` の既存仕様どおり) —
挙動としては正しい。

### 20.4 IPC (unnecessary UI/IPC work during navigation) の再確認

`navigation` シナリオ相当の自動操作 (起動時ロード1回 + `navigate` 3回、
`wait 300` ずつ) で `velox-bench ipc-summary` を実行 (1試行):

| dir | name | count | total_bytes | median_ms | p95_ms |
| --- | --- | ---: | ---: | ---: | ---: |
| out | `set_tabs` | 14 | 2,383 | 0.000 | 1.175 |

4回のロード (起動時1 + navigate 3) に対し `set_tabs` 14件 (≈3.5件/
ロード) — `docs/performance-targets.md` §18 (#66) が報告した比率
(「120件/約34回の操作」≈3.5) とオーダーが一致し、コストも
sub-millisecond のまま。新しい削減対象は見つからなかった (D87)。

### 20.5 DNS/connection/TLS timing 調査 (PoC 出力)

wry のネイティブ API には無い。JS 標準の `PerformanceNavigationTiming`
は WebKitGTK で動作を確認 (`http://127.0.0.1:8731/minimal.html` に対し):

```
{"entryType":"navigation","domainLookupStart":1,"domainLookupEnd":1,
"connectStart":1,"connectEnd":1,"secureConnectionStart":0,
"requestStart":1,"responseStart":2,"responseEnd":14,"fetchStart":1,
"startTime":0,"protocol":"http/1.0"}
```

値がすべて 1ms 前後に潰れているのはループバック接続に実質的な DNS/TCP
コストが無いため — この環境では意味のある DNS/TLS 数値は取れない。
詳しい経緯・結論は D87 を参照。

### 20.6 再現手順

```sh
S=/path/to/scratch
cargo build --release
(cd scripts/bench/pages && python3 -m http.server 8731 &)
URL=http://127.0.0.1:8731/minimal.html
XV='xvfb-run -a --server-args=-screen 0 1280x900x24 dbus-run-session --'

# 20.2 ページロード段階計測
$XV target/release/velox-bench run --scenario navigation --trials 10 \
  --velox-bin target/release/velox --url $URL --output $S/navigation.json

# 20.4 IPC
cat > $S/nav_session.txt << 'SCRIPT'
mark
navigate http://127.0.0.1:8731/minimal.html?velox-bench-step=1
wait 300
navigate http://127.0.0.1:8731/minimal.html?velox-bench-step=2
wait 300
navigate http://127.0.0.1:8731/minimal.html?velox-bench-step=3
wait 300
quit
SCRIPT
$XV env VELOX_PERF_METRICS=1 VELOX_PERF_FORMAT=json \
  VELOX_PERF_OUTPUT=$S/nav_session.jsonl \
  VELOX_HOMEPAGE=$URL \
  VELOX_AUTOMATION_SCRIPT=$S/nav_session.txt \
  target/release/velox
target/release/velox-bench ipc-summary --input $S/nav_session.jsonl \
  --output $S/ipc-summary.json
```
## 21. Windows (WebView2) の実測 (Issue #136)

**設計判断・実装方針は `docs/decisions.md` D88 を参照。**
`.github/workflows/perf-windows.yml` の初回実行
(run [`34127310212`](https://github.com/noan98/VeloX/actions/runs/34127310212)、
ジョブ `velox-bench run (windows-latest)`、job id `101758900568`、
2026-09-07) が `windows-latest` ランナー上で success で完走し、この節に
Windows 側の実測値を記録できるようになった (Issue #180)。

**この初回実行は `workflow_dispatch` (手動実行) ではない。** `perf-windows.yml`
は `workflow_dispatch` に加えて「`perf-windows.yml` 自身を変更する PR」でだけ
`pull_request` トリガーでも走る (D88。`workflow_dispatch` は `main` にマージ
されるまで Actions タブに現れず手動実行できないため、workflow 自身の検証手段
として付けてある)。run 34127310212 はまさにその経路で、`perf-windows.yml` を
新規追加した PR #179 に対する `pull_request` トリガーの自動実行として走った
(ジョブログの checkout は `refs/remotes/pull/179/merge`、run のイベント種別も
`pull_request`)。**したがって「手動実行された初回の計測」ではなく、
「workflow 追加 PR 上での初回の検証実行」である。** 以後シナリオや試行回数を
変えて測る場合は、`main` にマージ済みの `workflow_dispatch` から実行する。D88 が「最大の
リスク」としていた `windows-latest` 上での GUI/WebView2 ウィンドウの起動
可否は、この実行により**起動できた**で決着している (詳細は §21.3)。数値は
すべて run 34127310212 のジョブログ・Actions Artifact に実在するものだけを
転記しており、推定値・補間値は含まない。

> ⚠️ **この節の数値を、§1〜§20 の Linux (WebKitGTK/Xvfb) の数値と直接比較
> しないこと。** OS が異なれば WebView 実装 (WebView2 vs WebKitGTK) も
> プロセスモデルも別物であり、Epic #57 絶対ルール5「OS ごとに結果を分ける」
> のとおり比較は成立しない。特に **PSS 相当は Windows では実装していない
> (D88)** — `pss_total_bytes` は Windows の結果には常に含まれず、
> `total_rss_bytes` のみが入る。将来 Windows 側に PSS 相当の値を追加した
> としても、Linux の PSS (`smaps_rollup` 由来) とは算出方法が全く異なるため
> 直接比較してはならない (D88 参照)。

### 21.1 測定環境

**run 34127310212 の「Record environment info (OS build / CPU / memory /
WebView2 Runtime)」ステップ (2026-09-07 13:25:58〜13:26:00 UTC) のログから
転記。** 数値の捏造・推定はしていない。

| 項目 | 値 |
| --- | --- |
| ランナー | `windows-latest` (**GitHub-hosted の共有・仮想化ランナー。Windows 実機の実力値ではない** — CPU が 2 論理コアしか無く他ジョブと共有される仮想環境で、ばらつきも実機より大きく出うる) |
| OS | Microsoft Windows Server 2025 Datacenter |
| OS ビルド番号 | 26100 |
| CPU | AMD EPYC 9V74 80-Core Processor (2 論理コア) |
| メモリ (物理) | 7.99 GiB |
| WebView2 Runtime バージョン | 151.0.4129.101 |
| rustc | 1.98.1 (48a229cea 2026-09-01) |
| VeloX ビルド | `cargo build --release` (`velox.exe` / `velox-bench.exe`)、commit `b92dc6825c65edda116a021390513f2fba12daf9` |
| ディスプレイ | GitHub-hosted Windows ランナーの対話セッション (Xvfb 相当の仕組みは無いが、§21.3 のとおり GUI/WebView2 ウィンドウは実際に起動できることを確認した) |
| 実行元 | [run 34127310212](https://github.com/noan98/VeloX/actions/runs/34127310212) / job `101758900568` (`velox-bench run (windows-latest)`) |
| 実行トリガー | `pull_request` (PR #179 = `perf-windows.yml` を追加した PR)。**`workflow_dispatch` による手動実行ではない** — 節冒頭の説明を参照 |

### 21.2 シナリオ別の結果

`velox-bench run --scenario cold_startup --trials 10 --url
http://127.0.0.1:8731/minimal.html` (`scripts/bench/pages/minimal.html` を
loopback の `python -m http.server` で配信、§1 と同じくネットワーク非依存の
固定ページ) を実行した結果。形式は §4/§13/§20 の既存の節にならい、
`velox-bench` の `metrics` キー (`docs/benchmarking.md` 「計測される生データ
との対応」表) をそのまま使う。

```
scenario=cold_startup os=windows cpu=2 trials=10 commit=b92dc6825c65edda116a021390513f2fba12daf9
```

| メトリクス | n | median | p95 | mean | min | max | stddev |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `page_load_dispatch_ms` | 10 | 5.35 | 65.04 | 17.80 | 0.90 | 73.10 | 25.74 |
| `page_load_engine_ms` | 10 | 32.05 | 51.16 | 36.23 | 23.20 | 51.20 | 11.60 |
| `page_load_ms` | 10 | 54.10 | 93.70 | 54.06 | 24.20 | 100.50 | 25.16 |
| `pss_process_count` | 10 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 |
| `rss_process_count` | 10 | 8.00 | 9.10 | 8.20 | 8.00 | 10.00 | 0.63 |
| `rss_total_bytes` | 10 | 385329152.00 | 405977497.60 | 385506918.40 | 368803840.00 | 419270656.00 | 14101030.53 |
| `startup_first_load_ms` | 10 | 716.70 | 743.58 | 720.61 | 699.70 | 750.60 | 15.13 |
| `startup_rust_setup_done_ms` | 10 | 645.30 | 664.39 | 647.60 | 628.70 | 672.40 | 11.10 |
| `startup_toolbar_ready_ms` | 10 | 646.00 | 665.75 | 648.48 | 629.50 | 673.40 | 11.17 |
| `startup_toolbar_script_started_ms` | 10 | 645.90 | 665.69 | 648.39 | 629.50 | 673.30 | 11.16 |
| `startup_window_created_ms` | 10 | 644.05 | 662.91 | 646.15 | 627.80 | 670.20 | 11.05 |

**`pss_process_count` が全 10 試行で 0 なのは異常ではなく想定どおりの挙動。**
D88 のとおり Windows では PSS 相当を実装していないため
(`total_pss_bytes`/`pss_process_count` は Windows の結果では常に欠損として
扱われる)、この節でメモリを見る指標は `rss_total_bytes` のみになる。

結果 JSON (`cold_startup-windows.json`) は Actions Artifact
[`velox-perf-windows-cold_startup`](https://github.com/noan98/VeloX/actions/runs/34127310212/artifacts/10020756565)
(Artifact ID `10020756565`、保持期限 30 日) として run 34127310212 に保存
されている。上表の値はこの Artifact およびジョブログの標準出力
(`velox-bench run` の集計表・JSON 両方) と一致する。

> Linux (§1〜§20) の数値とは比較しないこと。前掲の警告ブロックのとおり。

### 21.3 GUI 起動可否の検証結果

**起動できた。** D88 が「最大のリスク」と位置付けていた、`windows-latest`
ランナー上で VeloX (WebView2 ウィンドウ) が起動できるかどうかは、この初回
実行で解消した。

`perf-windows.yml` の診断ステップ (`velox.exe --homepage
http://127.0.0.1:8731/minimal.html` を直接起動し 8 秒待ってから、プロセス
一覧と perf ログの有無を確認する) の実際の出力:

```
--- process snapshot ---

  Id ProcessName    MainWindowTitle
  -- -----------    ---------------
2948 msedgewebview2
6440 msedgewebview2
7072 msedgewebview2
7104 msedgewebview2
7248 msedgewebview2
7392 msedgewebview2
8316 msedgewebview2
6120 velox          VeloX

--- perf log ---
records: 34
```

プロセス構成は `velox.exe` 1 + `msedgewebview2.exe` 7 で、`velox.exe`
(PID 6120) の `MainWindowTitle` は `VeloX` になっていた — ウィンドウが実際に
作られ、タイトルも設定されている証拠。perf ログ (`VELOX_PERF_OUTPUT`) にも
実際に 34 件のレコードが書けている。続く `velox-bench run --scenario
cold_startup --trials 10` も 10/10 試行すべてで各 32 件のレコードを取得して
完走した (§21.2 の結果はこの 10 試行から集計したもの)。

## 22. Serialization / Allocation 最適化の計測結果 (Issue #68, 2026-09-07)

**設計判断は `docs/decisions.md` D89 を参照。** ここでは実測データと
再現手順だけを記録する。**この節の数値はすべて §1 の環境 (Ubuntu 24.04.4 /
WebKitGTK 2.52.6 / Xvfb、GPU なし) での計測であり、Windows (WebView2) /
macOS (WKWebView) の実力値ではない** — ディスク書き込み (`fs::write`) の
コストは NTFS/APFS のメタデータ操作コストが Linux の ext4/tmpfs と異なる
ため、22.1 の内訳比率がそのまま外挿できる保証はない。

### 22.1 `persistence::write_json` の内訳 (D86 の Revisit condition (2))

`write_json`(`fs::create_dir_all` → `serde_json::to_string_pretty` →
`fs::write`) の 3 ステップを、20 タブ相当の `SessionSnapshot`(シリアライズ後
4,169 bytes) に対して個別に計測した。**`create_dir_all`は毎回すでに存在する
ディレクトリを対象にしている** — `persist_session`が実運用で辿る定常状態
(初回起動直後を除けば常にディレクトリは存在済み) を再現するため。各ステップ
20,000 回、`std::hint::black_box`で結果を消費させて最適化による消失を防止。
3 回実行した結果 (1 回あたり):

| ステップ | 実行1 | 実行2 | 実行3 |
| --- | ---: | ---: | ---: |
| `fs::create_dir_all`(既存ディレクトリ) | 1.337µs | 1.423µs | 1.158µs |
| `serde_json::to_string_pretty` | 3.385µs | 3.331µs | 3.450µs |
| `fs::write`(実ディスク書き込み) | 96.876µs | 91.471µs | 90.298µs |

3 ステップの合計 (約 95.6〜102.7µs ≈ 0.1ms) は §19 が報告した
`state_write name=session` の実測中央値 (0.1ms) とほぼ一致しており、この
マイクロベンチマークが実際の呼び出しコストを正しく再現できていることの
裏付けになっている。**`fs::write`が全体の約 90〜95%を占め、シリアライズの
25〜30 倍のコスト** — D89 が結論づけたとおり、シリアライズ経路の最適化は
`state_write`全体のコストにはほとんど効かない。

再現手順 (このベンチマーク自体は使い捨てのローカルテストとして書き、
リポジトリには残していない — 再現する場合は以下を `tests/` 配下に一時的に
作成して `cargo test --release` で実行する):

```rust
use std::fs;
use std::time::Instant;
use velox::browser::session::{SavedTab, SessionSnapshot};

fn make_snapshot(n: usize) -> SessionSnapshot {
    let tabs = (0..n)
        .map(|i| SavedTab {
            url: format!("https://example.com/page/{i}/some/longer/path/segment"),
            title: Some(format!("Example Page Title Number {i} - Some Longer Title Text")),
            favicon: Some(format!("https://example.com/favicon-{i}.ico")),
        })
        .collect();
    SessionSnapshot { tabs, active_index: 0 }
}

#[test]
fn scratch_write_json_breakdown() {
    let dir = std::env::temp_dir().join(format!("velox-wjb-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("session.json");
    let snapshot = make_snapshot(20);
    let iters = 20_000u32;

    let started = Instant::now();
    for _ in 0..iters {
        std::hint::black_box(fs::create_dir_all(std::hint::black_box(&dir))).unwrap();
    }
    let mkdir_elapsed = started.elapsed();

    let started = Instant::now();
    for _ in 0..iters {
        std::hint::black_box(serde_json::to_string_pretty(std::hint::black_box(&snapshot)).unwrap());
    }
    let serialize_elapsed = started.elapsed();

    let data = serde_json::to_string_pretty(&snapshot).unwrap();
    let started = Instant::now();
    for _ in 0..iters {
        fs::write(std::hint::black_box(&path), std::hint::black_box(&data)).unwrap();
    }
    let write_elapsed = started.elapsed();

    println!(
        "mkdir_per_call={:?} serialize_per_call={:?} write_per_call={:?}",
        mkdir_elapsed / iters, serialize_elapsed / iters, write_elapsed / iters
    );
    fs::remove_dir_all(&dir).ok();
}
```

### 22.2 `escape_js_line_terminators` の before/after (D89)

`toolbar::set_tabs_script`(タブ数 3/20/50 の `TabSummary` 配列) を対象に、
`escape_js_line_terminators`の常時フルコピーを除去する前後でマイクロ
ベンチマークした。各条件 1,000 回のウォームアップの後、本計測を実施
(`black_box`で最適化消失を防止)。それぞれ 3 回実行:

**before (修正前、`e3d1986`)**:

| tabs | iters | 実行1 (1回あたり) | 実行2 | 実行3 |
| --- | ---: | ---: | ---: | ---: |
| 3 | 200,000 | 943ns | 956ns | 1.012µs |
| 20 | 200,000 | 4.721µs | 4.735µs | 4.791µs |
| 50 | 100,000 | 11.747µs | 10.806µs | 11.188µs |

**after (修正後)**:

| tabs | iters | 実行1 (1回あたり) | 実行2 | 実行3 |
| --- | ---: | ---: | ---: | ---: |
| 3 | 200,000 | 856ns | 859ns | 913ns |
| 20 | 200,000 | 4.286µs | 4.432µs | 4.599µs |
| 50 | 100,000 | 10.344µs | 10.390µs | 10.416µs |

3 サイズすべてで after の 3 回の実行値が before の 3 回の実行値をすべて
下回っている (範囲が重ならない) — 単発のノイズではなく再現する差である
ことを示す。20 タブでの改善幅はおおむね 5〜10%。

再現手順 (同じく使い捨てのローカルテストとして `tests/` 配下に一時的に
作成し、修正前後のコミットそれぞれで `cargo test --release --test
<name> -- --nocapture` を実行して比較した):

```rust
use std::time::Instant;
use velox::ui::toolbar::{set_tabs_script, TabSummary};

fn make_tabs(n: usize) -> Vec<TabSummary> {
    (0..n)
        .map(|i| TabSummary {
            id: i as u64,
            url: format!("https://example.com/page/{i}/some/longer/path/segment"),
            title: Some(format!("Example Page Title Number {i} - Some Longer Title Text")),
            favicon: Some(format!("https://example.com/favicon-{i}.ico")),
            loading: i % 3 == 0,
            active: i == 0,
            suspended: false,
        })
        .collect()
}

fn bench(label: &str, n_tabs: usize, iters: u32) {
    let tabs = make_tabs(n_tabs);
    for _ in 0..1000 {
        std::hint::black_box(set_tabs_script(std::hint::black_box(&tabs)));
    }
    let started = Instant::now();
    for _ in 0..iters {
        std::hint::black_box(set_tabs_script(std::hint::black_box(&tabs)));
    }
    let elapsed = started.elapsed();
    println!("{label} tabs={n_tabs} iters={iters} per_call={:?}", elapsed / iters);
}

#[test]
fn scratch_bench_set_tabs_script() {
    bench("set_tabs_script", 3, 200_000);
    bench("set_tabs_script", 20, 200_000);
    bench("set_tabs_script", 50, 100_000);
}
```

### 22.3 `velox-bench gate` — 回帰の有無

`tab_create_20` シナリオ (`sync_tab_strip`/`set_tabs`/`persist_session`を
繰り返し経由する、この Issue の変更が最も効きうるシナリオ) で
baseline=修正前コミット `e3d1986`、candidate=修正後を各 8 試行 × 2 回
計測:

```
regression gate: scenario=tab_create_20 candidates=2 (warn>20.0% fail>60.0%)
metric                             baseline       candidates (中央値/変化率)       判定       備考
page_load_dispatch_ms                  8.65     8.4(-2.9%), 8.9(+2.3%)       OK
page_load_engine_ms                    7.45     7.2(-2.7%), 7.4(-0.7%)       OK
page_load_ms                          16.15   15.2(-6.2%), 15.9(-1.5%)       OK
tab_create_ms                          3.10     3.1(+0.0%), 3.2(+1.6%)       OK

総合判定: OK
```

`tab_create_ms`/`page_load_ms`とも baseline 比で有意な劣化は無い (変化率は
すべて warn 閾値 20%を大きく下回る) — 22.2 のマイクロベンチマークが捉えた
数百 ns〜µs オーダーの差は、この規模のシナリオ計測 (ms オーダー、共有 VM
上のスケジューリングノイズを含む) では検出限界以下であることの確認でもある
(#66/#67 の`set_tabs`/`persist_session`の結論と同じ形: 「呼ばれる回数」や
「アロケーション」を削っても、既にサブミリ秒の処理の`duration_ms`表示上は
変化として見えない)。

### 22.4 再現手順 (`velox-bench gate` 部分)

§20.6 と同じ環境構築 (HTTP サーバ) の後:

```sh
cargo build --release
S=/path/to/scratch
XV_RUN() { xvfb-run -a --server-args="-screen 0 1280x900x24" dbus-run-session -- "$@"; }

# baseline/candidate バイナリをそれぞれ用意 (git stash 等で切り替えてビルド)
XV_RUN target/release/velox-bench run --scenario tab_create_20 --trials 8 \
  --velox-bin "$S/velox-baseline" --url http://127.0.0.1:8731/minimal.html \
  --output "$S/tab_create_20-baseline.json"

for i in 1 2; do
  XV_RUN target/release/velox-bench run --scenario tab_create_20 --trials 8 \
    --velox-bin "$S/velox-candidate" --url http://127.0.0.1:8731/minimal.html \
    --output "$S/tab_create_20-candidate-$i.json"
done

target/release/velox-bench gate \
  --baseline "$S/tab_create_20-baseline.json" \
  --candidate "$S/tab_create_20-candidate-1.json" \
  --candidate "$S/tab_create_20-candidate-2.json" \
  --warn-pct 20 --fail-pct 60 \
  --output "$S/gate-report.json" --markdown-output "$S/gate-summary.md"
```

## 23. 自動タブ休止のデフォルト ON (Issue #184, 2026-09-07)

**設計判断は `docs/decisions.md` D90 を参照。** ここでは既定 ON にした状態
での実測結果だけを記録する。測定環境は §1 と同一 (このコンテナ、
WebKitGTK、Xvfb、GPU なし)。before は本 Issue 着手前 (`beba7b7`、D9/D56 の
「全シグナル無効」既定) のバイナリ、after は本 Issue の変更 (D90、メモリ
予算 700 MiB を既定で有効化) を適用したバイナリで、どちらも同じセッション
で `cargo build --release` した。

> **その後 (Issue #176 Stage 1)**: 本節の既定 ON の挙動を別セッションで
> 再現した上で、**メモリ予算の値そのものを説明変数にした応答曲線**を
> §25 に追加した。本節が「700 MiB という 1 点で何が起きるか」を記録して
> いるのに対し、§25 は「予算を動かすと何が起きるか」を記録している。

### 23.1 1/5/10/20 タブの PSS (`tab_scaling.py`、3 試行の中央値、`minimal.html`)

| タブ数 | Chromium (MiB) | before (旧既定=無効, MiB) | after (新既定=700 MiB, env 上書き無し, MiB) | before→after |
| ---: | ---: | ---: | ---: | ---: |
| 1  |  281.7 |  400.2 |  399.8 | ±0.0% |
| 5  |  321.9 |  646.6 |  646.5 | ±0.0%（予算内のため休止なし） |
| 10 |  369.5 |  979.7 |  464.4 | **-52.6%** |
| 20 |  468.2 | 1655.7 |  617.7 | **-62.7%** |

Chromium 比 (after): 1 タブ +41.9%、5 タブ +100.8%（予算内でまだ休止して
いない分、旧既定と同じ）、10 タブ **+25.7%**、20 タブ **+31.9%**。絶対値は
D56 の元セッション (§12: 409.0/653.6/990.6/1612.1 MiB) と数十 MiB のずれが
あるが (実行時刻・コンテナ負荷によるドリフト。§10/D46 が言う「異なる
セッションを比較してはならない」の対象で、本節の結論はすべて同一セッション
内の before/after 比較のみに基づく)、**相対的な形は D56 と一致**する: 5
タブでは予算 (700 MiB) 内に収まるため休止が起きず before と同一、10/20
タブでは大きく下がる。T2 (Chromium 比 +10% 以内) は引き続き未達 (§12/D56
と同じ結論) — 本 Issue は T2 の目標値に新たに近づけることを目的にしておらず、
既定 ON でも D56 が実測した削減効果 (-65%程度) がそのまま出ることを確認する
のが狙いだった。

### 23.2 軽量ケース (1〜3 タブ) のポーリングコスト

Issue #184 の acceptance criterion: 「休止が発生しない軽量ケースで、メモリ
予算のポーリング自体のコストが無視できること」の確認。`scripts/profile/
cpu_usage.py` (`/proc/<pid>/stat` の utime+stime を起動直後と 30 秒後の 2
点だけ外部から読み、差分を取る — 測定自体のコストが被測定側に乗らない) で、
アイドル 30 秒窓の CPU% を「既定 (メモリ監視 ON)」と「`VELOX_MEMORY_BUDGET_
MB=0` (OFF)」で比較した。既定の `memory_check_interval` は当時 2 秒。

| ケース | 既定 ON (CPU%) | `VELOX_MEMORY_BUDGET_MB=0` (CPU%) | 差分 |
| --- | ---: | ---: | ---: |
| 1 タブ | 1.2%（0.35〜0.36 秒/30 秒、3 試行） | 0.1%（0.04 秒/30 秒） | 約 +1.1pt |
| 3 タブ | 1.5%（0.44 秒/30 秒） | 0.2%（0.05 秒/30 秒） | 約 +1.3pt |

`/proc` 全体を 2 秒ごとに 1 回歩くサンプラ自体のコストは 1 コア換算で約
1〜1.3 ポイント。**この節は当初「実用上無視できる」と結論したが、Issue
#187/#189 でこれを誤りと判定し覆した。** 1 タブのアイドル状態で CPU が OFF
比 12 倍というのは看過すべきでない差であり、`memory_check_interval` の
既定見直しと `process_map` 自体の最適化の両方を行った。詳細は
**§23.2.1**・**§23.2.2** を参照。

#### 23.2.1 `process_map` の二段階化と `memory_check_interval` の間隔別実測 (Issue #187/#189)

`smaps_rollup` 読み取り自体のコストを直接計測するため、
`browser::metrics::imp::process_map()` (Linux) と同じ手順 (`/proc` を
列挙し各 PID の `status`/`smaps_rollup`/`stat` を読む) を踏む外部プローブ
を書いて、フルスキャン 1 回の壁時計時間を計測した (VeloX を 1 タブで起動
した状態、20 試行の中央値、このコンテナ、プロセス総数 88、うち
`smaps_rollup` が読めたもの 21):

| 内訳 | 時間 (中央値) | 全体比 |
| --- | ---: | ---: |
| フルスキャン合計 | 17.008ms | 100% |
| `status` 読み取り (全 87 プロセス) | 1.208ms | 7.1% |
| **`smaps_rollup` 読み取り (21 プロセスのみ)** | **14.568ms** | **85.7%** |
| `stat` 読み取り (全 87 プロセス) | 1.138ms | 6.7% |

`smaps_rollup` はアクセスできた 21/87 プロセス分だけで全体の 86% を占め、
1 プロセスあたりのコストが `status`/`stat` (定数個のフィールドを読むだけ)
とは桁違いに高いことを確認した。**このうち VeloX 自身のツリーは約 9
プロセスだけで、残り約 12 プロセス分の `smaps_rollup` 読み取りは
`build_sample` が最終的に捨てる無駄だった** — レビュー指摘を受け、
`process_map` を「パス 1: 全プロセスの `status`/スナップショット列挙だけ
で `root_pid` の子孫集合を確定 → パス 2: その子孫集合だけに
`smaps_rollup`/`stat` (Windows は `OpenProcess`+`GetProcessMemoryInfo`/
`GetProcessTimes`) を読む」という 2 パスに分割した (`build_sample` は
無変更、意味論を変えない純粋な最適化)。

二段階化の前後で、間隔別のアイドル CPU を `scripts/profile/cpu_usage.py`
(idle 60 秒窓、`--settle-secs` 6〜8 秒、`VELOX_MEMORY_CHECK_INTERVAL_MS`
だけを変えて比較) で計測した:

| 間隔 | 1 タブ (一段階) | 1 タブ (二段階) | 3 タブ (一段階) | 3 タブ (二段階) |
| ---: | ---: | ---: | ---: | ---: |
| 2000ms (旧既定) | 1.2〜1.3% | **0.8%** | 1.4% | **1.1%** |
| 5000ms | 0.5〜0.6% | **0.4%** | 0.6% | **0.5%** |
| 10000ms | 0.3% | **0.2%** | 0.4% | **0.3%** |
| 30000ms | 0.2% | (未計測) | 0.2% | (未計測) |
| (参考) OFF | 0.1% | 0.1% | 0.2% | 0.2% |

2000ms で約 35%、5000ms で約 25〜30% の追加削減 (二段階化そのものの
効果)。**結論として採用した間隔は 5 秒** (10 秒ではない) — 二段階化後の
5000ms のコスト (1 タブ 0.4%・3 タブ 0.5%) が一段階読み時代の 10000ms
とほぼ同等かそれ以上に下がったため、検知遅延を犠牲にしてまで 10 秒へ
延ばす理由が無くなった。`SuspensionPolicy::DEFAULT_MEMORY_CHECK_INTERVAL`
を最終的に **2 秒 → 5 秒** に変更 (旧既定比で 1 タブ CPU **1.2% → 0.4%、
約 67% 削減**)。設計判断の詳細は `docs/decisions.md` D90
(「#187/#189: `memory_check_interval` の既定見直し、および `process_map`
の二段階化」) を参照。

**10/20 タブでの削減効果・収束時間の再確認** (`tab_scaling.py`、新既定
[メモリ予算 700 MiB・5 秒間隔・二段階読み] を使用、`--stabilize-secs` を
振って比較):

| stabilize 秒数 | 10 タブ PSS | 20 タブ PSS | 状態 |
| ---: | ---: | ---: | --- |
| 3 秒 (旧 2 秒間隔・一段階読みでの §23.1 計測と同じ待ち時間) | 466.5 MiB | 625.6 MiB (試行によりばらつき) | ほぼ収束 |
| 8 秒 | 464.5 MiB | 621.0 MiB | 収束済み |
| 15 秒 (script の新しい既定、§23.2.2 参照) | 463.9〜619.6 MiB | 同左 | 収束済み |

**最終到達点は変わらない** (§23.1 の 2 秒間隔の値 [464.4/617.7 MiB] と
誤差範囲で一致)。二段階化 + 5 秒化により、10 秒間隔だった時点で必要だった
15 秒という収束待ちが **8 秒で確実に収束**するまで短縮された (2 秒間隔・
旧既定の「3 秒で収束」に近い水準まで回復) — CPU と検知遅延を両方改善する
結果になった。

**回帰ゲート** (`cold_startup`、baseline=旧既定 [2 秒間隔・一段階読み]、
candidate=新既定 [5 秒間隔・二段階読み] ×2、各 8 試行): 総合判定 **OK**
(`startup_toolbar_ready_ms` は -0.7%〜-2.2%、`rss_total_bytes` は
-23%〜-24% といずれも改善方向、悪化した指標は無い)。

**Windows 版 `process_map` も同じ構造で二段階化したが未検証**: Windows
実装 (D88/#136) も全プロセスに `OpenProcess`+`GetProcessMemoryInfo`/
`GetProcessTimes` を無条件に呼ぶ同じ構造の無駄を持っていたため、同じ
二段階化 (パス 1 はスナップショット列挙のみ、パス 2 で子孫集合だけに
`OpenProcess` 系 API を呼ぶ) を適用した。**このコンテナには Windows 実機
/CI が無く、実際の削減効果は測定できていない** — `cargo check --target
x86_64-pc-windows-msvc --all-targets` による型チェックのみ確認済み
(`docs/decisions.md` D90 の Revisit condition (7) 参照)。

#### 23.2.2 `scripts/bench/tab_scaling.py` の既定待ち時間を間隔に追従させる (Issue #189、Codex 指摘)

レビューで、`tab_scaling.py` の `--stabilize-secs` 既定値 (3.0 秒固定)
が `memory_check_interval` の実際の値と無関係にハードコードされており、
間隔を変えるたびに人手で追随させないと**静かに「休止前の PSS」を報告
する**という指摘を受けた (§23.2.1 の「10 秒間隔・3 秒 stabilize で
980.4/1668.9 MiB = 未収束」がまさにこれを裏付けていた)。

対処として `tab_scaling.py` に `--memory-check-interval-ms` を新設した
(既定値は `SuspensionPolicy::DEFAULT_MEMORY_CHECK_INTERVAL` と同期させた
Python 定数 `DEFAULT_MEMORY_CHECK_INTERVAL_MS = 5000`)。この値を VeloX
の子プロセスに `VELOX_MEMORY_CHECK_INTERVAL_MS` として明示的に渡し
(呼び出し元のシェルが既に設定していればそちらを優先)、`--stabilize-secs`
未指定時はそこから `max(3.0, 3.0 * interval_secs)` で算出する — 「この
待ち時間はこの間隔を前提にしている」という関係をスクリプト内で自己完結
させた。実際に動かして確認: 既定 (5000ms) では「15.0 秒を既定値として
使います」と表示され 20 タブで 619.6 MiB (収束済み) を報告、
`--memory-check-interval-ms 2000` を明示すると「6.0 秒」と表示され 10
タブで 465.4 MiB (収束済み) を報告 — env 経由の上書きと待ち時間算出の
両方が連動して動くことを確認した。`scripts/bench/`・`scripts/profile/`
の他のスクリプトを確認したが、同種の依存 (メモリサンプラ間隔への暗黙の
固定待ち時間) は他に無かった。

### 23.3 `velox-bench gate`

`--warn-pct 20 --fail-pct 60`、baseline=旧既定バイナリ、candidate=新既定
バイナリ ×2、各 8 試行:

| シナリオ | 総合判定 | 備考 |
| --- | --- | --- |
| `cold_startup` | OK | `startup_toolbar_ready_ms` など全項目 -3.6%〜+2.6% |
| `tab_create` | OK | `tab_create_ms` -4.9%〜+9.8% |
| `tab_switch` | OK | `tab_switch_ms` ±0.0% |
| `tab_create_20` | OK | `tab_create_ms` -1.3%〜±0.0%、`page_load_*` 数%以内 |
| `tab_switch_20` | **OK — ただし比較不能** | 下記参照 |

上位 4 シナリオはいずれも少タブ (数タブ) しか開かないため 700 MiB を超えず、
既定を変える前と実質的に同じものを測っている。

**`tab_switch_20` は「回帰なし」の確認になっていない、という発見**: 20
タブまで開くこの手動シナリオでは、新既定のメモリ予算がベンチマーク実行中
に実際にバックグラウンドタブを休止させてしまうため、`switch` コマンドの
大半が `tab_switch` ではなく `tab_resume`(+`page_load`) として記録される。
baseline の出力は `tab_switch_ms` のみ、candidate の出力は
`tab_resume_ms`/`page_load_*` のみとなり、`velox-bench gate` は「比較可能な
メトリクスがありません」として機械的に**総合判定 OK** を返す — これは
性能に問題が無いことの確認では**ない**。`VELOX_MEMORY_BUDGET_MB=0` を明示
すると `tab_switch_ms` は再び記録され、baseline とほぼ同じ値 (0.80ms 中央値、
両者一致) になることを確認した — 休止の無効化は完全に機能している。

**影響範囲は限定的**: 自動化されている唯一の回帰ゲート
(`.github/workflows/perf-gate.yml`) は `cold_startup` のみを対象にしており、
20 タブ級のシナリオは走らせていないため、CI の自動回帰検知がサイレントに
機能を失っているわけではない。ただし今後 `tab_switch_20`/`tab_create_20`
のような多タブシナリオを手動で再計測する際は、`VELOX_MEMORY_BUDGET_MB=0`
を明示しない限り「switch のレイテンシ」のつもりが実際には「resume の
レイテンシ」を測ってしまう点に注意が必要 (`docs/decisions.md` D90 の
Revisit condition (4))。

### 23.4 再現手順

```sh
cargo build --release
S=/path/to/scratch
cp target/release/velox "$S/velox-after"
git stash && cargo build --release && cp target/release/velox "$S/velox-before" && git stash pop
CHROME=/opt/pw-browsers/chromium-1194/chrome-linux/chrome
XV_RUN() { xvfb-run -a --server-args="-screen 0 1280x900x24" dbus-run-session -- "$@"; }

# 23.1: タブ数スケーリング (旧既定 = before は Chromium も同時計測)
XV_RUN python3 scripts/bench/tab_scaling.py --velox "$S/velox-before" --chromium "$CHROME" \
  --page minimal.html --tab-counts 1,5,10,20 --trials 3 --output "$S/before.json"
XV_RUN python3 scripts/bench/tab_scaling.py --velox "$S/velox-after" \
  --page minimal.html --tab-counts 1,5,10,20 --trials 3 --output "$S/after-default.json"

# 23.2: 軽量ケースのポーリングコスト
P=$PWD/scripts/bench/pages
printf 'wait 40000\nquit\n' > "$S/light1.txt"
printf 'open file://%s/text.html\nwait_load\nopen file://%s/dom_heavy.html\nwait_load\nwait 40000\nquit\n' \
  "$P" "$P" > "$S/light3.txt"
XV_RUN python3 scripts/profile/cpu_usage.py --velox "$S/velox-after" \
  --script "$S/light1.txt" --homepage "file://$P/minimal.html" \
  --settle-secs 6 --window-secs 30 --label 1tab_on
VELOX_MEMORY_BUDGET_MB=0 XV_RUN python3 scripts/profile/cpu_usage.py --velox "$S/velox-after" \
  --script "$S/light1.txt" --homepage "file://$P/minimal.html" \
  --settle-secs 6 --window-secs 30 --label 1tab_off
# 3 タブも同様に light3.txt で

# 23.3: gate (§22.4 と同じ形。cold_startup/tab_create/tab_switch/tab_create_20/tab_switch_20 を
# baseline=velox-before, candidate=velox-after ×2 で。tab_switch_20 のみ VELOX_MEMORY_BUDGET_MB=0
# を付けた追加実行で無効化の効果も確認した)
(cd scripts/bench/pages && python3 -m http.server 8731 &)
XV_RUN target/release/velox-bench run --scenario tab_switch_20 --trials 8 \
  --velox-bin "$S/velox-before" --url http://127.0.0.1:8731/minimal.html \
  --output "$S/tab_switch_20-baseline.json"
for i in 1 2; do
  XV_RUN target/release/velox-bench run --scenario tab_switch_20 --trials 8 \
    --velox-bin "$S/velox-after" --url http://127.0.0.1:8731/minimal.html \
    --output "$S/tab_switch_20-candidate-$i.json"
done
target/release/velox-bench gate \
  --baseline "$S/tab_switch_20-baseline.json" \
  --candidate "$S/tab_switch_20-candidate-1.json" --candidate "$S/tab_switch_20-candidate-2.json" \
  --warn-pct 20 --fail-pct 60 --output "$S/gate-report.json" --markdown-output "$S/gate-summary.md"

# 23.2.1 (Issue #187/#189): 間隔別のアイドル CPU。velox-onepass は process_map
# 二段階化「前」(#187 時点)、velox-after は二段階化「後」(#189、DEFAULT_
# MEMORY_CHECK_INTERVAL=5s) のコードでそれぞれビルドしたバイナリ
printf 'wait 70000\nquit\n' > "$S/light1_70.txt"
for interval in 2000 5000 10000; do
  for bin in velox-onepass velox-after; do
    VELOX_MEMORY_CHECK_INTERVAL_MS=$interval XV_RUN python3 scripts/profile/cpu_usage.py \
      --velox "$S/$bin" --script "$S/light1_70.txt" --homepage "file://$P/minimal.html" \
      --settle-secs 6 --window-secs 60 --label "1tab_${bin}_${interval}ms"
  done
done

# smaps_rollup 読み取りコストの分離 (process_map() を模した外部プローブ、Linux 専用)
# scripts/profile/ には存在しないため、本節のためだけに一時スクリプトとして書いた:
# /proc を列挙 -> 各 PID の status/smaps_rollup/stat を開いて読むだけの Python 20 試行、
# VeloX を 1 タブで起動した状態で並行実行。詳細な実装は本節の数値の再現時に
# `browser::metrics::imp::process_map()` (Linux, src/browser/metrics.rs) をそのまま
# Python に書き写せば良い。

# 10/20 タブでの収束確認 (5 秒間隔・二段階読みの新既定、stabilize-secs を変えて再計測。
# --memory-check-interval-ms/--stabilize-secs 未指定なら §23.2.2 のスクリプト側の
# 変更により自動的に 15 秒が使われる)
XV_RUN python3 scripts/bench/tab_scaling.py --velox "$S/velox-after" \
  --page minimal.html --tab-counts 10,20 --trials 3 --stabilize-secs 3 \
  --output "$S/after-5s-3s-stabilize.json"
XV_RUN python3 scripts/bench/tab_scaling.py --velox "$S/velox-after" \
  --page minimal.html --tab-counts 10,20 --trials 3 --stabilize-secs 8 \
  --output "$S/after-5s-8s-stabilize.json"
XV_RUN python3 scripts/bench/tab_scaling.py --velox "$S/velox-after" \
  --page minimal.html --tab-counts 10,20 --trials 3 \
  --output "$S/after-5s-auto-stabilize.json"  # 23.2.2: 既定 (自動算出 15 秒)
```

### 23.5 複数ウィンドウでのメモリ予算 (Issue #186)

設計判断・不具合の詳細は `docs/decisions.md` D90 (「#186: 複数ウィンドウで
メモリ予算が機能しない不具合の修正」) を参照。ここでは実測だけを記録する。

**再現テスト**: `tests/integration.rs::memory_budget_signal_reaches_a_
second_windows_background_tabs`。ウィンドウ 1 はホームタブのみ (休止候補
なし)、`new_window` で開いたウィンドウ 2 に背景タブ 2 つ、
`VELOX_MEMORY_BUDGET_MB=1`・`VELOX_MEMORY_CHECK_INTERVAL_MS=100`。修正前の
コードでは `tab_suspend` イベントが 0 件 (100ms ごとにサンプルが取れて
いるにもかかわらず、ウィンドウ 2 の背景タブは一度も休止されない) — 実際に
失敗することを確認した上で、ラウンドロビン方式 (D90) で修正した。

**過剰回収が起きていないことの確認**: `tests/integration.rs::memory_
budget_signal_never_suspends_more_than_one_windows_tabs_per_sample`。両
ウィンドウに背景タブ 2 つずつ (計 4 つ、すべて休止対象) を持たせ、
`tab_suspend` (reason=memory) のタイムスタンプを 50ms 以内でクラスタ化し、
どのクラスタも 2 件 (1 ウィンドウ分) を超えないことを確認 (green)。
`choose_memory_sample_window` (純粋関数、`src/app.rs`) 自体のユニット
テスト 5 本が、1 回の呼び出しで選ばれるウィンドウが常にちょうど 1 つで
あることを構造的に保証している。

**回帰ゲート** (`cold_startup`/`tab_create`/`tab_switch`、baseline=修正前
バイナリ、candidate=修正後バイナリ ×2、各 8 試行): いずれも**総合判定
OK**。

**再現手順**:

```sh
cargo build --release
S=/path/to/scratch
cp target/release/velox "$S/velox-fixed"
git stash && cargo build --release && cp target/release/velox "$S/velox-before" && git stash pop
XV_RUN() { xvfb-run -a --server-args="-screen 0 1280x900x24" dbus-run-session -- "$@"; }

# 再現テスト (修正前のバイナリでは失敗、修正後は成功)
XV_RUN cargo test --test integration memory_budget_signal_reaches_a_second_windows
XV_RUN cargo test --test integration memory_budget_signal_never_suspends_more

# 過剰回収なしのユニットテスト
cargo test --lib app::tests::a_single_call_never_serves_more_than_one_window
cargo test --lib app::tests::repeated_calls_round_robin_through_every_window_in_order
```

## 24. 起動の内訳: `process_start` → `window_created` の分解 (Issue #182, 2026-09-07)

**設計判断・実装方針は `docs/decisions.md` D92 を参照。**

§21 の Windows 初回実測 (Issue #136/#180) で、`startup_first_load_ms` の
中央値 716.7ms のうち **`startup_window_created_ms` が 644.1ms** を占める
ことが分かった。`page_load_ms` は 54.1ms で、ページロード自体はボトルネック
ではない。一方この 644ms は「プロセス開始から `BrowserWindow::new` が
返るまで」という 1 つの大きなバケツで、tao のイベントループ生成・VeloX 自身の
Rust セットアップ・ネイティブウィンドウ生成・WebView2 の初期化がすべて
混ざっており、**どこに手を入れれば効くのかを判断する材料が無かった。**

本節はその 644ms を 4 つの中間チェックポイントで分解し、
「VeloX 側で改善できる区間」と「tao / OS の GUI / WebView2 という外部要因」を
切り分けるための計測基盤と、その最初の結果を記録する。

### 24.1 追加したチェックポイント

`browser::metrics::StartupTimestamps` に 4 点を追加した (D43 が
`window_created` → `toolbar_ready` に対して行ったのと同じやり方を、その
手前の区間に適用したもの)。既存の 5 点と同じく、値は**すべてプロセス開始
からの累積 ms** であり、区間の長さは隣り合う値の差である。

| チェックポイント | メトリクス名 | ここまでに終わっていること |
| --- | --- | --- |
| `event_loop_built` | `startup_event_loop_ms` | `EventLoopBuilder::with_user_event().build()` が返った。Linux の `gtk_init` 相当、Windows ではウィンドウクラス登録・OLE/COM 初期化・DPI awareness など |
| `pre_window_setup_done` | `startup_pre_window_setup_ms` | `settings.json` の読込・適用、ブロックリスト/サイト例外の構築、サイト権限とセッション復元のディスク読込、`Windows`/`Tabs` の構築。**`BrowserWindow::new` を呼ぶ直前** |
| `native_window_built` | `startup_native_window_ms` | tao の `WindowBuilder::build()` が返った。ネイティブウィンドウ (HWND / GtkWindow) は存在するが webview はまだ 0 個 |
| `toolbar_webview_built` | `startup_toolbar_webview_ms` | ツールバー webview (**プロセス最初の webview**) が attach された。Windows なら WebView2 環境の生成と `msedgewebview2.exe` の起動を含む |
| `window_created` (既存) | `startup_window_created_ms` | content webview も attach され `BrowserWindow::new` が返った |

したがって 5 つの区間に分かれる:

1. `process_start` → `event_loop` — **tao / OS の GUI 初期化** (外部要因)
2. `event_loop` → `pre_window_setup` — **VeloX 自身の Rust コード** (唯一 VeloX が短縮できる区間)
3. `pre_window_setup` → `native_window` — **tao のウィンドウ生成** (外部要因)
4. `native_window` → `toolbar_webview` — **web エンジンの初回初期化** (外部要因、Epic #57 ルール 3 のブラックボックス)
5. `toolbar_webview` → `window_created` — 2 個目の webview (エンジンの初回コストを払い終えた後)

> **`toolbar_ready` と `first_load` の間には順序保証が無い。** content タブの
> `LoadFinished` とツールバーの `ready` ハンドシェイクは独立した経路であり、
> ページの方が先に終わることが実際にある (下記 Linux 実測で
> `toolbar_ready` → `first_load` の最小値が **-8.1ms**)。この 2 点の差を
> 「区間」として読まないこと。1〜5 の 5 区間と `window_created` →
> `toolbar_ready` は同一の呼び出し順序で到達するため順序が保証される。

### 24.2 区間ごとの値を求める手順

結果 JSON に入るのは**累積値の統計**なので、`startup_toolbar_webview_ms` の
中央値から `startup_native_window_ms` の中央値を引いた値は、**区間の中央値
ではない** (それぞれの中央値は別々の試行から来うる)。ざっくりどの区間が
支配的かを見るにはこの引き算で十分で、`perf-windows.yml` の
「Startup breakdown summary」ステップが Job Summary にその表を出す。

区間そのものの分布 (中央値・p95・最小・最大) を論じる場合は、**試行ごとに
引き算してから集計する**必要がある。`velox-bench run` は試行ごとの生ログを
残さないため、VeloX を直接起動して `startup` レコードを集める:

```sh
# 固定ページを loopback で配信 (§1 と同じ)
(cd scripts/bench/pages && python3 -m http.server 8731 &)

printf 'wait_startup\nquit\n' > /tmp/velox-startup.txt
for i in $(seq 1 10); do
  VELOX_PERF_METRICS=1 VELOX_PERF_FORMAT=json \
  VELOX_PERF_OUTPUT=/tmp/velox-startup-$i.jsonl \
  VELOX_DATA_DIR=/tmp/velox-startup-data-$i \
  VELOX_HOMEPAGE=http://127.0.0.1:8731/minimal.html \
  VELOX_AUTOMATION_SCRIPT=/tmp/velox-startup.txt \
    xvfb-run -a --server-args="-screen 0 1280x900x24" \
      dbus-run-session -- ./target/release/velox
done
# 各ファイルの event=="startup" レコードから隣接チェックポイントを引き算して集計する
```

`wait_startup` (D85) は `startup` レコードが書かれるまでブロックするため、
固定 wait を挟まずに 1 試行が完結する。Windows では `xvfb-run` /
`dbus-run-session` のラップが不要になるほかは同じ。

### 24.3 Linux (WebKitGTK/Xvfb) での実測 — **計測基盤の検証であって、Windows の答えではない**

> ⚠️ **この節の数値を Windows の実力値として扱わないこと** (Epic #57 絶対
> ルール 5)。ここに Linux の数値を載せているのは、追加したチェックポイントが
> 実際に妥当な値を出すことと、区間の切り分けが機能することを確かめるため
> であって、§21 が示した Windows の 644ms を説明するものではない。
> WebKitGTK と WebView2 はプロセスモデルからして別物である。

環境は §1 と同一 (Ubuntu 24.04.4 / Intel Xeon @ 2.80GHz 4 コア / 15 GiB /
Xvfb `-screen 0 1280x900x24` / GPU なし / WebKitGTK 2.52.6 / rustc 1.94.1 /
`cargo build --release`)。ベースは commit `1904d86e0f4407551b9061a2564356e17fee27fe`
に本 Issue の計測追加を載せた作業ツリー。

**(a) `velox-bench run --scenario cold_startup --trials 10 --url
http://127.0.0.1:8731/minimal.html`** — 累積値。形式は §4/§21 と同じ。

| メトリクス | n | median | p95 |
| --- | ---: | ---: | ---: |
| `startup_event_loop_ms` | 10 | 13.90 | 93.39 |
| `startup_pre_window_setup_ms` | 10 | 14.05 | 93.49 |
| `startup_native_window_ms` | 10 | 44.65 | 130.19 |
| `startup_toolbar_webview_ms` | 10 | 148.85 | 210.33 |
| `startup_window_created_ms` | 10 | 154.45 | 213.76 |
| `startup_rust_setup_done_ms` | 10 | 154.70 | 213.96 |
| `startup_toolbar_script_started_ms` | 10 | 343.30 | 412.95 |
| `startup_toolbar_ready_ms` | 10 | 343.35 | 413.41 |
| `startup_first_load_ms` | 10 | 368.40 | 427.73 |

**(b) §24.2 の手順で試行ごとに引き算した区間** — 別の 10 試行 (上表とは
別セッションなので、(a) の値と直接は突き合わせられない)。

| 区間 | n | median | p95 | min | max |
| --- | ---: | ---: | ---: | ---: | ---: |
| `process_start` → `event_loop` | 10 | 12.15 | 135.30 | 11.20 | 135.30 |
| `event_loop` → `pre_window_setup` (**VeloX 自身**) | 10 | **0.10** | 0.20 | 0.10 | 0.20 |
| `pre_window_setup` → `native_window` | 10 | 29.90 | 34.70 | 28.90 | 34.70 |
| `native_window` → `toolbar_webview` | 10 | **95.95** | 102.60 | 60.40 | 102.60 |
| `toolbar_webview` → `window_created` | 10 | 1.60 | 2.30 | 1.50 | 2.30 |
| `window_created` → `toolbar_ready` | 10 | 171.65 | 187.70 | 162.10 | 187.70 |
| `toolbar_ready` → `first_load` | 10 | 12.55 | 30.90 | **-8.1** | 30.90 |

この 10 試行の `window_created` 累積中央値は 140.80ms。

**Linux で読み取れること**:

- **VeloX 自身の Rust セットアップは 0.10ms** — `window_created` 140.8ms の
  **0.07%** でしかない。設定・ブロックリスト・サイト権限・セッション復元の
  ディスク読込をすべて含めてこの値であり、D43 が
  `window_created → rust_setup_done` について出した「約 0.1ms、測定誤差の
  範囲」という結論と同じ桁である。
- **支配的なのは最初の webview の生成 (95.95ms、約 68%)**。2 個目の
  content webview は 1.60ms しかかからない — エンジンの初回初期化コストが
  1 個目に集中していることが、これで恒久的なメトリクスとして観測できる
  ようになった。D43 は使い捨ての診断コードで同じ非対称性を見ていたが、
  本 Issue でそれが常設の計測になった。
- 残りは tao のイベントループ生成 (12.15ms) とネイティブウィンドウ生成
  (29.90ms) で、いずれも VeloX のコードではない。
- `process_start` → `event_loop` は min 11.20 / max 135.30 とばらつきが
  大きい (p95 が中央値の 10 倍超)。この環境固有のノイズの可能性が高く、
  この区間だけで回帰を論じるのは避けること。

### 24.4 Windows (WebView2) での実測 — run 34178072263

**取得済み。** `perf-windows.yml` に本 Issue の「Startup breakdown summary」
ステップを追加した本 PR に対する `pull_request` トリガーの自動実行
(run [`34178072263`](https://github.com/noan98/VeloX/actions/runs/34178072263)、
ジョブ `velox-bench run (windows-latest)`、job id `101911349058`、
2026-09-08 01:51〜01:57 UTC) が success で完走し、Windows 側の内訳が取れた。
§21 の初回実測 (run 34127310212) と同じ経路である (D88、§21 冒頭を参照)。

数値はすべて当該ジョブのログおよび Artifact
`velox-perf-windows-cold_startup` の `cold_startup-windows.json` に実在する
ものだけを転記しており、**推定値・補間値は含まない**。

測定条件: シナリオ `cold_startup` / 試行 10 回 /
URL `http://127.0.0.1:8731/minimal.html` (固定ページを loopback 配信) /
結果 JSON の `environment` は `os=windows`, `cpu_count=2`,
`git_commit=70c543017093cf5131e03a2c293bd7352435c60a` (PR head をベースに
マージしたコミット) / rustc 1.98.1 x86_64-pc-windows-msvc /
`cargo build --release`。ランナーは §21.1 と同じ `windows-latest`
(**GitHub-hosted の共有・仮想化ランナー。Windows 実機の実力値ではない**)。
本 run の OS ビルド番号・WebView2 Runtime バージョンは
「Record environment info」ステップのログにあるが、本節には転記していない。

**(a) 累積値** — 形式は §4/§21/§24.3(a) と同じ。

| メトリクス | n | median | p95 | min | max |
| --- | ---: | ---: | ---: | ---: | ---: |
| `startup_event_loop_ms` | 10 | 4.50 | 8.56 | 3.70 | 10.50 |
| `startup_pre_window_setup_ms` | 10 | 5.05 | 9.02 | 4.20 | 11.00 |
| `startup_native_window_ms` | 10 | 63.95 | 85.25 | 55.70 | 85.70 |
| `startup_toolbar_webview_ms` | 10 | 736.10 | 857.99 | 644.80 | 867.80 |
| `startup_window_created_ms` | 10 | 922.35 | 1108.91 | 871.50 | 1129.30 |
| `startup_rust_setup_done_ms` | 10 | 926.60 | 1112.30 | 872.60 | 1130.70 |
| `startup_toolbar_script_started_ms` | 10 | 927.90 | 1113.21 | 874.70 | 1132.20 |
| `startup_toolbar_ready_ms` | 10 | 928.00 | 1113.31 | 874.70 | 1132.30 |
| `startup_first_load_ms` | 10 | 1028.05 | 1210.45 | 968.00 | 1234.80 |

同 run の参考値: `page_load_ms` median 80.95 / p95 117.80、
`rss_total_bytes` median 385,269,760、`rss_process_count` median 8.00、
`pss_process_count` 0.00 (Windows では PSS を実装していない。D88)。

**(b) 区間 — 累積中央値どうしの差**

> ⚠️ **これは「区間ごとの中央値」ではない。** 各メトリクスの中央値は別々の
> 試行から来うるため、差は区間の中央値と一致する保証が無い。§24.3(b) の
> Linux 側は試行ごとに引き算してから集計しているが、**Windows では試行ごとの
> 生データを手元に取り込んでいないため、その形式では出せていない。**
> 区間の分布 (p95 / min / max) まで見るには §24.2 の手順を Windows 上で
> 実行する必要がある。Job Summary の表も同じ注意書きを出力する。

| 区間 | 累積中央値の差 (ms) | `window_created` 比 |
| --- | ---: | ---: |
| `process_start` → `event_loop` | 4.50 | 0.5% |
| `event_loop` → `pre_window_setup` (**VeloX 自身**) | **0.55** | **0.06%** |
| `pre_window_setup` → `native_window` | 58.90 | 6.4% |
| `native_window` → `toolbar_webview` | **672.15** | **72.9%** |
| `toolbar_webview` → `window_created` | 186.25 | 20.2% |
| `window_created` → `toolbar_ready` | 5.65 | — |
| `toolbar_ready` → `first_load` | 100.05 | — |

**Windows で読み取れること**:

- **VeloX 自身の Rust セットアップは 0.55ms** — `window_created` 922.35ms の
  **0.06%**。設定・ブロックリスト・サイト権限・セッション復元のディスク読込を
  すべて含めてこの値である。Linux (§24.3、0.10ms / 0.07%) と**同じ結論**で
  あり、これが本 Issue で最も重要な結果である。**VeloX 側のコードを速くしても
  起動時間はまず動かない。**
- **支配的なのは最初の webview の生成 (672.15ms、72.9%)** — WebView2 環境の
  生成と `msedgewebview2.exe` の起動を含む区間。Epic #57 ルール 3 の
  「WebView はブラックボックス」に該当し、VeloX 側から短縮する手立ては
  現時点で無い。
- **2 個目 (content) の webview が 186.25ms かかる — ここは Linux と質的に
  違う。** Linux では 1.60ms しかかからず「初回コストは 1 個目に集中する」
  と読めたが、Windows では 2 個目にも 186ms 残る。**Linux から外挿していたら
  見落としていた差である** (Epic #57 ルール 5 の実例)。
- `window_created` → `toolbar_ready` は 5.65ms で、Linux の 171.65ms と
  逆転している。

**§21 の値との差について → §24.5 で切り分け中**:

§21 (run 34127310212) は同じ `windows-latest` / `cold_startup` / 10 試行で
`startup_window_created_ms` 中央値 **644.05ms**、`startup_first_load_ms`
**716.70ms** だった。本 run はそれぞれ **922.35ms / 1028.05ms** で、
**約 1.43 倍**である。**その後の追加計測で「本 Issue の計装が原因」という
可能性は否定された** (計装の無い `main` でも 885.65ms)。ただし**真の原因は
まだ特定できていない**。経緯と 4 つの計測点は §24.5 にまとめてある。

したがって **本節の絶対値を §21 と 1 対 1 で比較しないこと。** 本節が答えを
出しているのは「644ms (あるいは 922ms) が**どの区間に割れるか**」という比率
の問いであって、Windows の起動時間の代表値ではない。代表値を論じるには
§21 と同様に複数 run を積む必要がある。

**結論 (Epic #57 ルール 1 の下での判断)**:

`pre_window_setup` 区間が 0.06% である以上、**この Issue では最適化を行わない
のが正しい**。#59/#60/#64/#66/#69 と同じ決着である。次に見るべき候補は
「2 個目の webview の 186ms」だが、これも WebView2 側のコストであり、
着手するなら**まず区間の分布 (§24.2 の手順を Windows で実行) を取ってから**
別 Issue として起票する。

### 24.5 §21 との差の切り分け — **計装は原因ではない。原因は未特定**

§24.4 の `startup_window_created_ms` 922.35ms は §21 の 644.05ms の約 1.43 倍
だった。§24.4 執筆時点では「共有ランナーのばらつき」が第一候補だったが、
**その後の 2 回の計測でこの説明は苦しくなった。**

**(a) 4 つの計測点**

| # | run | head | 計装 (#182) | 日時 (UTC) | `window_created` median | `first_load` median |
| ---: | --- | --- | :---: | --- | ---: | ---: |
| 1 | [34127310212](https://github.com/noan98/VeloX/actions/runs/34127310212) (§21) | PR #179 の merge ref | 無 | 09-07 13:26 | **644.05** | **716.70** |
| 2 | [34178072263](https://github.com/noan98/VeloX/actions/runs/34178072263) (§24.4) | `a791800` | 有 | 09-08 01:57 | 922.35 | 1028.05 |
| 3 | [34178644598](https://github.com/noan98/VeloX/actions/runs/34178644598) | `542c852` | 有 | 09-08 02:05 | 905.80 | 1026.05 |
| 4 | [34178981601](https://github.com/noan98/VeloX/actions/runs/34178981601) | `c38016c` (**main**) | **無** | 09-08 02:12 | **885.65** | **987.60** |

すべて `windows-latest` / `cold_startup` / 10 試行 / 同じ固定ページ。
run 4 は `workflow_dispatch` による**対照実験**で、`main` には #182 の計装が
入っていないことを利用して「計装そのものが遅くしているのか」を判定するために
実行した。run 4 の値はジョブ `velox-bench run (windows-latest)`
(job id 101913982142) の「Show result summary」ステップの JSON からの転記
(`window_created` p95 974.74 / min 849.40 / max 1017.00、
`first_load` p95 1060.03 / max 1086.40 / mean 996.05 / stddev 38.98。
`first_load` の min はログの取得範囲に入っていなかったため記載しない)。

**(b) 読み取れること**

- **#182 の計装は原因ではない。** 計装の無い run 4 (885.65ms) が、計装のある
  run 2/3 (922.35 / 905.80ms) と同じ帯にある。**この可能性は否定された。**
- **run 2/3/4 は互いに約 4% 以内に収まり、run 1 だけが 27% 低い。**
  「共有ランナーのばらつき」で片付けるには、run 1 だけが低い側に外れている
  形が不自然である。
- したがって、**run 1 (09-07 13:26) から run 4 (09-08 02:12) の間に何かが
  変わった**と考えるのが自然である。

**(c) 原因は特定できていない**

run 1 の計測対象は `main` の `1aa24f4` (PR #177) に PR #179 (workflow の追加
のみ) を載せた ref である。それ以降 `main` にマージされ、**実行時の挙動を
変えうる** PR は次の 5 本:

| merge (UTC) | PR | Issue | 内容 |
| --- | --- | --- | --- |
| 09-07 13:40 | #178 | #67 | 状態更新のディスパッチ |
| 09-07 14:11 | #183 | #68 | シリアライズの重複排除 |
| **09-07 15:37** | **#185** | **#184** | **自動タブ休止をメモリ予算ベースでデフォルト ON** |
| 09-07 17:04 | #189 | #187 | メモリサンプラの間隔 |
| 09-07 17:33 | #193 | #186 | ウィンドウ跨ぎの予算判定 |

**最有力の仮説は #185 (と、同じ subsystem を触る #189 / #193) である** —
デフォルト ON になったことで起動直後からメモリサンプリングが走るようになった。
ただし**これは仮説であって、実証していない。** 加えて、ランナーイメージや
WebView2 Runtime のバージョンが 09-07 13:26 から 09-08 02:12 の間に更新された
可能性も潰せていない。

**run 1 側は 1 回しか測っていない**点にも注意する。run 1 自体が低い側の外れ値
である可能性は、現在のデータでは排除できない。

**(d) 切り分けの手順**

`perf-windows.yml` の `workflow_dispatch` は ref (ブランチ / タグ) を取るため、
候補コミットにブランチを立てて実行すれば二分探索できる。最小の実験は
`beba7b7` (#185 の 1 つ前) と `a6aa703` (#185) の 2 点で、それぞれ 10 試行を
複数回。**#185 が原因だと分かった場合でも「戻す」が答えとは限らない** —
#185 は 10 タブで -52.6% / 20 タブで -62.7% のメモリ削減を得ている
(§23) ため、**Epic #57 ルール 4 の「メモリと速度のトレードオフを評価する」
そのものの判断になる。**

この切り分けは本 Issue (#182、計装の追加) のスコープ外であり、別 Issue に
分けてある。

## 25. Memory Budget Manager — Stage 1 の現状計測 (Issue #176, 2026-09-07)

**設計判断は `docs/decisions.md` D93 を参照。** ここでは Issue #176 の
Stage 1 (現状計測) で採った数値だけを記録する。測定環境は §1 と同一
(このコンテナ、WebKitGTK 2.52.6、Xvfb、GPU なし、搭載 RAM 15.70 GiB =
`/proc/meminfo` の `MemTotal: 16461028 kB`)。

**この節の数値はすべて同一セッション・同一バイナリ (`main` の `1904d86`、
`cargo build --release`) で採ったものであり、§12/§23 の数値とは
セッションが異なる** (§10/D46 の「異なるセッションを比較してはならない」)。
§23 の再現確認 (24.1) も、§23 の値そのものと突き合わせるのではなく
「同じ相対的な形が再現するか」だけを見ている。

### 25.1 ベースラインの再現 (既定 ON = メモリ予算 700 MiB)

`scripts/bench/tab_scaling.py`、`minimal.html`、3 試行の中央値、
`--stabilize-secs` は既定 (5 秒間隔から自動算出される 15 秒)。

| タブ数 | VeloX PSS (MiB) | VeloX RSS (MiB) | VeloX procs | Chromium PSS (MiB) | Chromium RSS (MiB) | Chromium procs | Chromium 比 (PSS) |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1  | 396.7 |  713.2 | 4 | 289.8 |  800.7 |  9 | +36.9% |
| 5  | 646.5 | 1114.7 | 5 | 328.8 | 1218.6 | 13 | +96.6% |
| 10 | 466.5 |  784.9 | 4 | 373.4 | 1728.1 | 18 | +24.9% |
| 20 | 596.9 | 1048.0 | 5 | 464.8 | 2764.2 | 28 | +28.4% |

§23.1 (464.4 / 617.7 MiB、Chromium 比 +25.7% / +31.9%) と**相対的な形が
一致**する: 5 タブでは予算内で休止が起きず、10/20 タブで大きく下がる。
T2 (Chromium 比 +10% 以内) は引き続き未達。

**RSS/PSS 比を併記したのは Windows のため** (24.5)。VeloX のプロセスツリー
では 1.68〜1.80 倍 (1 タブ 1.80 / 10 タブ 1.68 / 20 タブ 1.76)、同一
セッションの Chromium では 2.76〜5.95 倍 (1 タブ 2.76 / 10 タブ 4.63 /
20 タブ 5.95) だった。**これはどちらも Linux の数値であり、Windows の
値ではない。**

### 25.2 メモリ予算の値を振ったときの応答曲線

**Issue #176 の中心課題「搭載 RAM 相対の予算」(D90 Revisit condition (2))
を設計するために、予算そのものを説明変数にして応答を採った。** 同じ
`tab_scaling.py` を `VELOX_MEMORY_BUDGET_MB` だけ変えて回した (5/10/20
タブ、各 3 試行の中央値)。`0` は「メモリ予算シグナル無効」。

**PSS (MiB) / 括弧内はプロセス数の中央値:**

| `VELOX_MEMORY_BUDGET_MB` | 5 タブ | 10 タブ | 20 タブ |
| ---: | ---: | ---: | ---: |
| 0 (無効) | 645.5 (5) | 980.4 (6) | 1626.5 (9) |
| 300 | 488.3 (4) | 450.3 (4) | 558.3 (4) |
| 400 | 490.1 (4) | 451.7 (4) | 552.4 (4) |
| 500 | 480.1 (4) | 464.1 (4) | 551.8 (4) |
| **700 (現在の既定)** | **646.7 (5)** | **464.1 (4)** | **619.3 (4)** |
| 1000 | 646.4 (5) | 980.6 (6) | 892.1 (5) |
| 1400 | 646.1 (5) | 982.8 (6) | 1163.3 (6) |

**RSS (MiB)、同じ実行から:**

| `VELOX_MEMORY_BUDGET_MB` | 5 タブ | 10 タブ | 20 タブ |
| ---: | ---: | ---: | ---: |
| 0 (無効) | 1113.8 | 1608.8 | 2700.3 |
| 300 | 804.7 | 766.5 | 874.2 |
| 400 | 806.1 | 767.9 | 868.7 |
| 500 | 796.1 | 782.8 | 866.9 |
| **700 (現在の既定)** | **1114.0** | **782.6** | **943.2** |
| 1000 | 1114.7 | 1608.7 | 1371.9 |
| 1400 | 1114.2 | 1611.3 | 1799.1 |

**1 タブでは予算の値が結果に一切影響しない**ことも別途確認した (同じ
スクリプト、`--tab-counts 1 --trials 3`): 予算 0 で 396.8 MiB / RSS 713.4、
予算 300 で 396.4 MiB / RSS 713.0、予算 700 (24.1) で 396.7 MiB / RSS
713.2。休止できる背景タブが存在しないので当然の結果だが、**下限を割る
予算を設定しても軽量ケースを壊さない**ことの確認になっている。

この表から読み取れる 3 つの事実 (解釈と設計への含意は D93):

1. **1 タブの PSS 約 397 MiB が下限で、予算では下げられない。** 予算
   0/300/700 のいずれでも 396.4〜396.8 MiB。内訳は
   `docs/memory-analysis.md` §2.1 のとおり toolbar 用と content 用の
   `WebKitWebProcess` ×2 が支配的で、**休止はこのどちらにも手が届かない。**
2. **予算 500 MiB 以下では応答が飽和する。** 300/400/500 の 3 点は
   5 タブ 488.3/490.1/480.1、10 タブ 450.3/451.7/464.1、20 タブ
   558.3/552.4/551.8 と、いずれもタブ数内での散らばりが 14 MiB 以下で
   互いに区別がつかない。**下限より低い予算は値を変えてもほとんど
   意味が無い。**

   > **プロセス数の読み方に注意。** この環境の VeloX は 1 タブで
   > プロセス数 4 (velox 本体 1 + `WebKitWebProcess` 2 [toolbar 用 +
   > content 用] + `WebKitNetworkProcess` 1。`WebContext` 共有 [#118] 後の
   > 構成で、`docs/memory-analysis.md` §2.1 の 5 とは内訳が異なる)。
   > したがって **`procs=4` は「content 用の `WebProcess` が 1 つだけ
   > 残っている」という意味であって、「背景タブが 1 つも生きていない」
   > という意味ではない** — D54 の設計上その 1 プロセスは最大 4 タブを
   > 収容できるため、アクティブタブ以外にも生存タブが居りうる。実際
   > 25.3 の休止イベント数がそれを裏付けている: 10 タブで予算 300 は
   > 背景 9 個を全部休止する (9 件) のに対し、予算 500/700 は 8 件で
   > 背景タブが 1 つ生き残っており、PSS の差 13.8 MiB (450.3 対 464.1)
   > はその 1 タブ分にあたる。20 タブでの 500 対 700 の差 67.5 MiB
   > (551.8 対 619.3) も、`ESTIMATED_BYTES_PER_TAB` の 64 MiB とほぼ
   > 一致する 1 タブ分である。**予算の実効的な分解能はおよそ 1 タブ
   > (64 MiB) 刻みであり、それより細かい差は観測できない。**
3. **予算は「休止しなかった場合の使用量」に対する閾値として階段状に効き、
   発動した後の着地点は予算をかなり下回る。** 10 タブは休止 OFF で 980.4
   MiB なので、予算 1000 では一度も発動せず 980.6 MiB (= OFF と同じ)、
   予算 700 では発動して 464.1 MiB に落ちる — **予算を 300 MiB 動かすと
   10 タブの PSS が 516 MiB 動く。** プロセスグループ単位で回収するため
   (D56) 常にオーバーシュートし、20 タブでも予算 700 に対し着地は 619.3
   MiB。

### 25.3 低い予算は「暴走」しない — 休止イベント数の実測

25.2 の事実 2 から「下限を割る予算ではサンプルのたびに sweep が走り続けて
churn するのではないか」という懸念が立つ。**実測した結果、これは起きて
いない。**

10 タブを開いて 40 秒保持するだけの自動操作スクリプトを
`VELOX_PERF_METRICS=1 VELOX_PERF_FORMAT=json VELOX_PERF_OUTPUT=<file>` で
走らせ、`tab_suspend` レコードを数えた (各予算 1 回):

| `VELOX_MEMORY_BUDGET_MB` | `tab_suspend` 件数 | 対象タブの異なり数 | 発生時刻 | 40 秒間の再発 |
| ---: | ---: | ---: | --- | --- |
| 300 | 9 | 9 | 5193〜5194ms | なし |
| 500 | 8 | 8 | 5195〜5197ms | なし |
| 700 | 8 | 8 | 5205〜5207ms | なし |

**同じタブが 2 回休止されたケースは 1 件も無く**、すべてタブを開き終えた
直後の最初のメモリサンプル (約 5.2 秒) で 2〜3ms のバースト 1 回に収まって
いる。休止済みタブは次回以降 `plan` の候補に入らないため、**下限を割る
予算は「churn」ではなく「背景タブを 1 回ずつ全部休止して、そのまま」**
という状態に退化する — 実質 `max_live_tabs=1` と同じ挙動になる。コストは
CPU ではなく、タブ切替のたびに再読み込みが要ること (状態の喪失) である。

**もう 1 つ重要な読み取り: 現在の既定 700 MiB は、10 タブの時点ですでに
背景タブ 9 個中 8 個を休止している。** 「もっと積極的に休止する」方向の
余地はほとんど残っていない — 予算を 300 MiB まで下げても、増えるのは
休止 1 件 (背景タブ 9 個中 9 個目) だけで、PSS の差は 13.8 MiB である。
残りはすべて 25.2 の事実 1 の下限とアクティブタブが占めている。

> **この 3 本の実行は 24.1/25.2 と PSS を比較できない。**
> `VELOX_PERF_METRICS=1` は `spawn_rss_sampler` (既定 5 秒間隔の `/proc`
> 走査) を追加で動かすため、被測定側の条件が変わる。この表から使うのは
> **イベント数と時刻だけ**で、メモリの値は一切使っていない。

**この節の Windows 版 (D97 Revisit (4) / §28.7 項目4)**: ここでの `tab_suspend`
の数え上げは手動のワンオフ計測だったが、D105 で `velox-bench` の
`MetricKey::SuspendedTabCount` (`suspended_tab_count`) として恒久化し、
`perf-windows.yml` の A/B 比較表・タブ数スケーリング表に RSS と並べて出せる
ようにした。この節の実測値 (300/500/700 MiB での 9/8/8 件) 自体は Linux での
1 回きりの手動計測のままであり、Windows で測り直したものではない。

### 25.4 搭載 RAM 相対にしたときに何が起きるか (このマシンの 1 点から)

現在の既定 700 MiB は、この測定環境の搭載 RAM 15.70 GiB に対して
**4.35%** にあたる (700 / 16075.2 MiB)。この 1 点を「比率」として他の
マシンに当てはめたときの値を、25.2 の応答曲線に重ねると:

| 想定マシンの RAM | 4.35% の予算 | 25.2 から予想される挙動 |
| ---: | ---: | --- |
| 4 GiB | 178 MiB | 下限 (約 397 MiB) を大きく割る → 25.2 事実 2 の飽和域。背景タブを常に全部休止 |
| 8 GiB | 357 MiB | 同上 (下限を割る) |
| **15.70 GiB (本環境)** | **700 MiB** | 実測どおり: 10 タブ 464.1 / 20 タブ 619.3 MiB |
| 32 GiB | 1427 MiB | 25.2 の 1400 MiB 行に相当: 10 タブでは一度も発動せず 982.8 MiB、20 タブで 1163.3 MiB |
| 64 GiB | 2854 MiB | 20 タブ (休止 OFF で 1626.5 MiB) でも発動しない → 実質 OFF |

**この表は「4.35% を採用すべき」という主張ではない。** 25.2 の予算軸の
応答は同一マシン上の実測だが、**「その RAM のマシンにとってその予算が
妥当か」はこの環境では測れない** (搭載 RAM を変えられるマシンが 1 台も
無い)。この表が示すのは**式の形の問題**だけである — 裸の比率では、
現実的な RAM 容量 (4〜8 GiB) で下限を割り、大容量側 (64 GiB) では実質
無効化される。

### 25.5 Windows について、既存データから言えること / 言えないこと

D90 Revisit condition (1) は「Windows 実機/CI での 700 MiB の実効性は
未計測」としている。D90 は §21 の存在には触れつつ「メモリ予算シグナルを
対象にしていない」として使わなかったが、**§21.2 が載せている
`rss_total_bytes` は Windows でメモリ予算と突き合わされる値そのもの**で
あり、1 タブ時点についてはこれで答えが出る。

| 出典 | 環境 | タブ数 | 指標 | 値 |
| --- | --- | ---: | --- | ---: |
| §21.2 (run 34127310212) | `windows-latest` / WebView2 / RAM 7.99 GiB | 1 (`cold_startup`) | `rss_total_bytes` 中央値 | 385,329,152 B = **367.5 MiB** (プロセス数 8) |
| 25.1 (本セッション) | このコンテナ / WebKitGTK / RAM 15.70 GiB | 1 | PSS | **396.7 MiB** (プロセス数 4) |

**言えること**: Windows のメモリ予算判定に使われる値 (RSS) の 1 タブ時の
実測は 367.5 MiB で、**700 MiB の予算に対して約 332 MiB の余裕がある。**
「Windows では RSS の二重計上で 700 MiB が極端に早く発動する」という
D90 の懸念は、少なくとも 1 タブ時点では成立していない。

**言えないこと (Epic #57 ルール 5)**: 上の 2 行は OS もエンジンも
プロセスモデルも異なり、**比較として読んではならない。** 並べたのは
「Windows 側の絶対値が 700 MiB に対してどの位置にあるか」を見るためだけ
である。そして**未知なのは切片ではなく傾き**である — タブ数を増やした
ときに Windows の RSS がどう伸びるかは一度も測られていない。25.1 で
測った Linux の RSS/PSS 比 (VeloX 1.68〜1.80 倍) をそのまま Windows に
当てはめる根拠は無く、**同じセッションの Chromium が 20 タブで 5.95 倍
だった**ことは、Chromium 系のプロセスモデルを持つ WebView2 で比がもっと
速く開きうる可能性への注意喚起にはなるが、**予測ではない。**
`perf-windows.yml` に `tab_scaling.py` 相当のタブ数スケーリングを追加して
実測するのが唯一の解決手段である (D93 の子 Issue 案 A)。

### 25.6 再現手順

```sh
cargo build --release
S=/path/to/scratch
cp target/release/velox "$S/velox-main"
CHROME=/opt/pw-browsers/chromium
XV_RUN() { xvfb-run -a --server-args="-screen 0 1280x900x24" dbus-run-session -- "$@"; }

# 24.1: ベースライン (VeloX 既定 ON + Chromium)
XV_RUN python3 scripts/bench/tab_scaling.py --velox "$S/velox-main" --chromium "$CHROME" \
  --page minimal.html --tab-counts 1,5,10,20 --trials 3 --output "$S/baseline-default.json"

# 24.2: 予算スイープ (--stabilize-secs は既定の自動算出 15 秒のまま)
for B in 0 300 400 500 700 1000 1400; do
  VELOX_MEMORY_BUDGET_MB=$B XV_RUN python3 scripts/bench/tab_scaling.py \
    --velox "$S/velox-main" --page minimal.html --tab-counts 5,10,20 --trials 3 \
    --output "$S/sweep-$B.json"
done
# 1 タブでの不変性の確認
for B in 0 300; do
  VELOX_MEMORY_BUDGET_MB=$B XV_RUN python3 scripts/bench/tab_scaling.py \
    --velox "$S/velox-main" --page minimal.html --tab-counts 1 --trials 3 \
    --output "$S/onetab-$B.json"
done

# 24.3: 休止イベント数 (10 タブを開いて 40 秒保持するだけのスクリプト)
P=$PWD/scripts/bench/pages
{ for i in $(seq 1 9); do echo "open file://$P/minimal.html"; echo "wait 300"; done;
  echo "wait 40000"; echo "quit"; } > "$S/hold10.txt"
for B in 300 500 700; do
  VELOX_MEMORY_BUDGET_MB=$B VELOX_PERF_METRICS=1 VELOX_PERF_FORMAT=json \
  VELOX_PERF_OUTPUT="$S/perf-$B.jsonl" VELOX_AUTOMATION_SCRIPT="$S/hold10.txt" \
  VELOX_HOMEPAGE="file://$P/minimal.html" VELOX_DATA_DIR="$S/data-$B" \
    XV_RUN "$S/velox-main" >/dev/null 2>&1
  # 件数・異なり数・再発の有無は tab_suspend レコードの tab_id/ts_ms を集計して確認
  grep tab_suspend "$S/perf-$B.jsonl" | wc -l
done
```

---

## 26. 起動回帰の A/B 切り分け (Issue #208, 2026-09-08)

§24.5 に記録した 4 点の計測で、`windows-latest` の `cold_startup` は
`startup_window_created_ms` の中央値が **644.05ms (run 1) → 885.65ms
(run 4)** と +38% 伸びている。#182 の計装が原因でないことは run 4 (計装
なしの `main`) で否定済みだが、**原因は特定できていない。**

### 26.1 なぜ「コミットの二分探索」ではなく A/B なのか

Issue #208 が最初に挙げた手順は、`beba7b7` (#185 の 1 つ前) と `a6aa703`
(#185) にブランチを立てて `perf-windows.yml` を複数回まわす二分探索である。
これは **run をまたぐ比較**であり、次の交絡がそのまま残る:

- ランナー個体差 (同じ `windows-latest` でも実体は毎回違うマシン)
- ランナーイメージ / WebView2 Runtime の更新 (§24.5 の 4 点は 09-07 13:26
  〜 09-08 02:12 に分散しており、この間の更新を否定できていない)
- run 1 が低い側の外れ値である可能性 (run 1 は 1 回しか測っていない)

**§24.5 が結論を出せずにいるのは、まさにこれらを分離できていないためで
ある。** 二分探索を足しても、同じ交絡を抱えた点が増えるだけになりうる。

そこで `perf-windows.yml` に **同一ジョブ・同一ビルド・同一ランナーの中で
条件 A/B を交互に測る**モードを入れた (入力 `compare_env` / `repeats`)。
上の 3 つはすべて A と B で共通なので丸ごと相殺され、差が出れば
「**その環境変数が起動時間を変えた**」以外の説明が残らない。実験用の
ブランチを候補コミットに立てる必要も無い。

### 26.2 条件

| 条件 | 設定 | 意味 |
| --- | --- | --- |
| A | 既定のまま | #185 (D90) 以降の `main` — メモリ予算 700 MiB による自動タブ休止が既定 ON |
| B | `VELOX_MEMORY_BUDGET_MB=0` | メモリ予算シグナルだけを切る。#185 が入る前と同じ「メモリ signal 無し」の状態 |

`0` を明示したときだけ既定が `Some` でも `None` になるのは
`config::resolve_suspension` の `overridable` の規則で、D90 が既定 ON に
するときに用意した逃げ道そのものである。`settings.json` は起動時に自動
生成されない (`app::run` の `None => config.to_settings()` は in-memory に
留まる) ため、クリーンな CI ランナーでは `apply_settings` が環境変数を
上書きすることもない。

`repeats=2` で **A → B → A → B** の順に計測する。順序効果 (ディスク
キャッシュの温まり、データディレクトリに溜まる履歴/セッション) を検出
するためで、**A 同士のばらつきが A/B 差と同程度なら、その差は結論に
してはならない。**

### 26.3 実行方法

```text
# Actions → Performance (Windows, manual) → Run workflow
scenario:    cold_startup
trials:      10
compare_env: VELOX_MEMORY_BUDGET_MB=0
repeats:     2
```

`perf-windows.yml` 自身を変更する PR でも同じ既定 (cold_startup / 10 試行 /
上の A/B) で自動実行される。`compare_env` を空欄にすると従来どおりの単独
計測に戻る。結果は Job Summary の「A/B 比較 (Issue #208)」表と、artifact
の `results/cold_startup-windows-{baseline,compare}-{1,2}.json` に出る。

### 26.4 結果 (run 34225826524, 2026-09-08 21:23〜21:28 JST)

`cold_startup` / 10 試行 / A → B → A → B / 固定ページ (`minimal.html` を
loopback 配信)。head は `7c489f8` (PR #210)。

#### 測定環境

**このジョブの「Record environment info」ステップのログから転記。**

| 項目 | 値 |
| --- | --- |
| ランナー | `windows-latest` |
| OS | Microsoft Windows Server 2025 Datacenter (ビルド 26100) |
| CPU | INTEL(R) XEON(R) PLATINUM 8573C (**4 論理コア**) |
| メモリ (物理) | **15.99 GiB** |
| WebView2 Runtime | 151.0.4129.101 |
| rustc | 1.98.1 (48a229cea 2026-09-01) |
| 実行元 | [run 34225826524](https://github.com/noan98/VeloX/actions/runs/34225826524) / job `102059582271` |

#### A/B 比較

| メトリクス | A の各 run の median | A 平均 | B の各 run の median | B 平均 | 差 (B-A) | 比 (B/A) |
| --- | --- | ---: | --- | ---: | ---: | ---: |
| `startup_native_window_ms` | 56.00, 53.05 | 54.52 | 55.50, 55.50 | 55.50 | +0.98 | 1.018 |
| `startup_toolbar_webview_ms` | 463.35, 452.40 | 457.88 | 454.25, 453.30 | 453.78 | -4.10 | 0.991 |
| `startup_window_created_ms` | 579.10, 567.80 | 573.45 | 568.30, 568.40 | 568.35 | -5.10 | 0.991 |
| `startup_toolbar_ready_ms` | 581.40, 570.00 | 575.70 | 570.45, 570.55 | 570.50 | -5.20 | 0.991 |
| `startup_first_load_ms` | 2671.00, 2656.90 | 2663.95 | 2658.05, 2656.50 | 2657.28 | -6.67 | 0.997 |
| `page_load_ms` | 2074.00, 2081.10 | 2077.55 | 2076.80, 2076.25 | 2076.52 | -1.03 | 1.000 |
| `rss_total_bytes` | 367,988,736 / 367,665,152 | 367,826,944 | 366,862,336 / 367,859,712 | 367,361,024 | -465,920 | 0.999 |

#### 事実 1: #185 は起動時間の原因ではない

`startup_window_created_ms` の A/B 差は **-5.10ms (-0.9%)** で、しかも符号は
「メモリ予算を切った方がわずかに速い」ではなく **A 同士のばらつき
(579.10 と 567.80 で 11.30ms) の内側**にある。26.2 に書いた読み方の規則
—「A 同士のばらつきが A/B 差と同程度なら結論にしてはならない」— を
そのまま適用して、**差は無い**と読む。全メトリクスで比は 0.991〜1.018 に
収まっており、`rss_total_bytes` すら 0.1% しか動いていない (1 タブでは
700 MiB の予算に一度も触れないので、これは予期どおり)。

**§24.5 の最有力仮説 (#185 が +241ms の原因) は否定された。** 併せて、
起動直後のサンプリングが疑われていた点も、コード上は
`spawn_memory_pressure_sampler` が `sleep(interval)` を**先に**実行する
(既定 5 秒) ため起動区間に重ならない、という読みと整合する。

#### 事実 2: `windows-latest` にはハード構成の異なるランナーが混在する

| | §21.1 (run 1, 09-07) | 本 run (09-08) |
| --- | --- | --- |
| CPU | AMD EPYC 9V74 80-Core | INTEL XEON PLATINUM 8573C |
| 論理コア | **2** | **4** |
| メモリ (物理) | **7.99 GiB** | **15.99 GiB** |
| OS ビルド | 26100 | 26100 |
| WebView2 Runtime | 151.0.4129.101 | 151.0.4129.101 |

**同じ `windows-latest` という指定で、CPU ベンダも論理コア数も搭載 RAM も
違うマシンが割り当てられている。** OS ビルドと WebView2 Runtime は一致して
いるので、§24.5 が疑っていた「ランナーイメージ / WebView2 の更新」ではなく、
**割り当てられるハードウェアそのものが違う。**

#### 事実 3: `window_created` の絶対値は 3 つ目の帯に落ちた

§24.5 の 4 点は 644.05ms (run 1) と 885〜922ms (run 2/3/4) の 2 つの帯に
分かれていたが、本 run は **573.45ms (A) / 568.35ms (B)** で**どちらでもない**。
事実 2 と合わせると、**run をまたいだ Windows の絶対値比較は成立しない。**
§24.5 の「644 → 885ms の回帰」は、回帰ではなくランナー機種差を見ていた
可能性が高い (本 run が最も高スペックで最も速い、という向きも整合する)。

#### 2 回目の A/B (run 34226965961, 2026-09-08 21:37〜21:41 JST)

同じ head の PR チェックとしてもう 1 回走った。**事実 1 は再現した。**

| メトリクス | A 平均 | B 平均 | 差 (B-A) | 比 |
| --- | ---: | ---: | ---: | ---: |
| `startup_window_created_ms` | 624.50 | 623.08 | -1.42 | 0.998 |
| `startup_toolbar_webview_ms` | 494.50 | 491.55 | -2.95 | 0.994 |
| `startup_first_load_ms` | 2719.90 | 2719.40 | -0.50 | 1.000 |
| `page_load_ms` | 2085.35 | 2079.98 | -5.37 | 0.997 |
| `rss_total_bytes` | 368,819,200 | 369,507,328 | +688,128 | 1.002 |

**そして事実 3 も強まった**: `startup_window_created_ms` は 1 回目の
573.45ms に対し **624.50ms** — **同じコミット・同じ workflow・同じ日の
2 つの run で 9% 違う。** run をまたいだ絶対値比較が成立しないことの、
機種差とは独立した確認である。

#### 付随 (**解決済み**): `page_load` の 38 倍の乖離は計測側が原因だった

1 回目・2 回目の `page_load_ms` 中央値は **2077ms / 2085ms** で、同じ固定
ページを使った §21.2 の **54.10ms** から桁が違っていた。原因は
**この PR が初回失敗の対応で足した、固定ページ配信サーバの
`-RedirectStandardOutput` / `-RedirectStandardError`** である。

3 回目 (run 34228350063、リダイレクトを外した head `9cbb2ca`) で
`page_load_ms` は **23.22ms (A) / 22.42ms (B)** に戻った。2 つの run との
差分はこのリダイレクトの有無だけなので、**原因は計測側で確定**である:
`python -m http.server` はシングルスレッドで、リクエストごとに stderr へ
ログを 1 行書く。書き込み先をファイルにしたことでその I/O が応答を
ブロックし、1 ページのロードに 2 秒かかっていた。**VeloX 側の問題では
ない。**

**教訓**: 計測環境に足した「診断のための仕組み」が、計測対象そのものを
歪めることがある。しかも今回は失敗ではなく**それらしい数字**が出るため、
§21.2 と突き合わせなければ気付けなかった。診断用の仕掛けを足したら、
既知の値と一度突き合わせること。

#### 3 回目の A/B (run 34228350063, 2026-09-08 21:50〜21:55 JST)

リダイレクトを外した head での `cold_startup` / 10 試行 / A→B→A→B。

| メトリクス | A 平均 | B 平均 | 差 (B-A) | 比 |
| --- | ---: | ---: | ---: | ---: |
| `startup_window_created_ms` | 505.58 | 505.38 | **-0.20** | 1.000 |
| `startup_toolbar_webview_ms` | 403.40 | 404.68 | +1.27 | 1.003 |
| `startup_first_load_ms` | 541.90 | 542.25 | +0.35 | 1.001 |
| `page_load_ms` | 23.22 | 22.42 | -0.80 | 0.966 |
| `rss_total_bytes` | 367,512,576 | 368,997,376 | +1,484,800 | 1.004 |

**事実 1 (#185 は原因ではない) はこれで 3 回連続の再現である** — A/B 差は
-5.10 / -1.42 / **-0.20** ms と、いずれも A 同士のばらつきの内側に収まる。

**事実 3 も 4 点目を得た**: `startup_window_created_ms` の絶対値は
573.45 → 624.50 → **505.58** ms。**同じ PR の同じ workflow で run ごとに
23% 動く。** §24.5 の 644 → 885ms (+38%) が「回帰」でなかったことは、
これで十分に裏づけられた。

### 26.5 ここから言えること

1. **#208 の「起動が伸びた」は回帰ではない。** #185 は A/B で否定され、
   絶対値の帯は 3 つ目が出た。二分探索を続ける根拠は無くなった。
2. **Windows の性能は同一 run 内でしか比較できない。** 別 run の数値を
   並べて「速くなった / 遅くなった」と言ってはならない — Epic #57 ルール 5
   (OS ごとに分ける) の Windows 版として、**「run ごとに分ける」**が要る。
3. **回帰検知を Windows で本気でやるなら、結果に CPU モデル・論理コア数・
   搭載 RAM を紐付け、同一機種の run 同士でしか比較しない仕組みが要る。**
   本 PR ではその第一歩として、A/B 比較表のすぐ上にランナーの CPU /
   論理コア数 / RAM を出すようにした (`results/environment-info.md` にも
   従来どおり保存される)。仕組みとしての定期計測・機種別集計は #208 の
   残タスクとして残る。
4. **この A/B モードは他の性能仮説にもそのまま使える。** 環境変数で
   切れる機能なら、`compare_env` を差し替えるだけで同じ確度の実験ができる。

---

## 27. Windows でのメモリ予算 700 MiB の実効性 (Issue #197, 2026-09-08)

D90 が既定 ON にしたメモリ予算 700 MiB は Linux/WebKitGTK でしか実測されて
いない。Windows/WebView2 では PSS が無く RSS で判定するため (D88)、同じ
700 MiB の意味が変わる。§26 で入れた A/B モードをそのまま使い、
A = 既定 (700 MiB) / B = `VELOX_MEMORY_BUDGET_MB=0` (予算 OFF) で測る。

### 27.1 1 回目 (run 34227483724, 2026-09-08 21:41〜21:44 JST) — **測定条件が成立していなかった**

`tabs_10` / 3 試行 / A → B → A → B。

| メトリクス | A 平均 | B 平均 | 差 (B-A) | 比 |
| --- | ---: | ---: | ---: | ---: |
| `rss_total_bytes` | 795,060,224 (**758.2 MiB**) | 791,541,760 (**754.9 MiB**) | -3,518,464 | 0.996 |
| `page_load_ms` | 990.12 | 981.65 | -8.47 | 0.991 |
| `startup_window_created_ms` | 656.55 | 646.30 | -10.25 | 0.984 |

**この -0.4% を「Windows では 700 MiB の予算が効いていない」と読んでは
ならない。** `tabs_N` のスクリプトは `tab_count - 1` 個のタブを**待ちを
挟まず連続で**開き、`MEMORY_STABILIZE_MS` (3 秒、`src/browser/automation.rs`)
待って `quit` する。プロセスの生存はおよそ **6 秒**である。一方、休止判定に
使う `spawn_memory_pressure_sampler` は `sleep(interval)` を**先に**実行する
既定 **5 秒**周期なので、**サンプルは 0〜1 回しか取れない。** 1 回取れたと
しても、その直後に `quit` が来るため休止の効果が RSS に反映される余地が
ほとんど無い。**つまりこの run は「予算が効くか」を測れていない** — 差が
出ないのは予算の性質ではなく、シナリオとサンプラ周期の噛み合わせの問題。

**ただし 1 つ確かなことが分かった**: **10 タブ時の Windows の RSS は
約 758 MiB で、既定の 700 MiB 予算を超えている。** 予算が正しく評価される
条件下なら**発動する水準に達している**ということであり、#197 が問うている
「Windows では 700 MiB が早く発動しすぎないか」は、まさにこの領域の話に
なる。なお §21.2 の 1 タブ 367.5 MiB とは **run も機種も違う**ので、
2 点を結んで傾きを語ってはならない (D96 決定 3)。同一 run 内で 1 / 5 / 10 /
20 タブを測るのが正しい採り方である。

### 27.2 再測定の条件

`perf-windows.yml` に **A/B の両方へ共通で環境変数を設定する入力
`common_env`** を足した。27.1 の噛み合わせを外すために使う:

```text
scenario:    tabs_10
trials:      3
compare_env: VELOX_MEMORY_BUDGET_MB=0
common_env:  VELOX_MEMORY_CHECK_INTERVAL_MS=500
repeats:     2
```

`common_env` は **A と B の両方**に効くので、A/B 差には現れない — 測定条件
そのものを成立させるためのものである。500ms 周期なら 6 秒の生存中に 10 回
以上サンプルが取れる。

**この設定自体が §25 (Linux) の測定条件とは異なる**点に注意する。§25 は
既定の 5 秒周期で測っており、周期を詰めれば休止の発動タイミングも変わる。
27.2 で得られる値は「予算が評価される条件下での A/B 差」であって、
「既定設定のままの Windows ユーザが体験する値」ではない。後者を測るには
`tabs_N` より長く生存するシナリオが要る (現状の velox-bench には無い)。

### 27.3 2 回目 (run 34228351011, 2026-09-08 21:51〜21:52 JST) — **Windows でもメモリ予算は効く**

27.2 の条件 (`common_env` に `VELOX_MEMORY_CHECK_INTERVAL_MS=500`) で
`tabs_10` / 3 試行 / A → B → A → B を測り直した。

| メトリクス | A の各 run | A 平均 | B の各 run | B 平均 | 差 (B-A) | 比 |
| --- | --- | ---: | --- | ---: | ---: | ---: |
| `rss_total_bytes` | 689,213,440 / 696,942,592 | **693,078,016 (660.9 MiB)** | 929,456,128 / 936,747,008 | **933,101,568 (889.9 MiB)** | +240,023,552 | **1.346** |

**A = 既定 (700 MiB 予算 ON) / B = `VELOX_MEMORY_BUDGET_MB=0` (予算 OFF)。**
差は **+240 MB / +34.6%** — 言い換えると、**予算を効かせることで 10 タブ時の
RSS が 889.9 MiB → 660.9 MiB に 25.7% 減る。** A 同士 (689/697 MB) と
B 同士 (929/937 MB) のばらつきはそれぞれ 8 MB 以内で、差 240 MB とは
比較にならない。**Windows/WebView2 でも D90 のメモリ予算は機能している。**

#### #197 が問うていたことへの答え

- **「Windows では RSS の二重計上で 700 MiB が極端に早く発動するのでは」**
  → 1 タブでは 367.5 MiB (§21.2) で発動せず、10 タブで初めて予算を超える。
  **早すぎる発動は観測されない。**
- **「発動した結果どこに着地するか」** → **660.9 MiB**、予算 700 MiB の
  5.6% 下である。Linux (§25 事実 3) では予算 700 に対し 464 MiB まで
  オーバーシュートしていたのに対し、**Windows の方が予算に近いところで
  止まっている。** プロセスグループ単位の回収 (D56) が WebView2 の
  プロセスモデルでは違う効き方をしている可能性がある。
- **「OS 別の既定値が必要か」** → **現時点の実測からは不要。** 700 MiB は
  Windows でも「1 タブでは触れず、10 タブでは効き、着地は予算の少し下」
  という素直な挙動になっている。

#### この結果の限界

1. **`VELOX_MEMORY_CHECK_INTERVAL_MS=500` は既定 (5000ms) ではない。**
   27.1 のとおり `tabs_N` はプロセスの生存が約 6 秒しかなく、既定周期では
   予算が原理的に発動しない。**「既定設定のままの Windows ユーザが
   10 タブ開いたときに何 MiB になるか」はまだ測れていない** — それには
   `tabs_N` より長く生存するシナリオが要る。
2. **タブ数は 10 の 1 点のみ。** 1 / 5 / 20 タブは測っていないので、
   D93 子 Issue 案 A が求める「Windows の RSS の傾き」はまだ埋まっていない。
   同一 run 内で複数のタブ数を測る形にする必要がある。
3. **休止による状態喪失のコストは測っていない。** 660.9 MiB という数字は
   「背景タブを何個休止した結果か」を含んでいない。D9 が懸念した
   「タブ切替のたびの再読み込み」がどの程度起きているかは、`tab_suspend`
   レコードを数える別の計測 (§25.3 の Windows 版) が要る。**→ D105 で
   `MetricKey::SuspendedTabCount` (`suspended_tab_count`) として計測基盤が
   できた。** A/B 比較表・タブ数スケーリング表に列を足したので、次に
   `perf-windows.yml` を実行すれば「何個休止した結果か」がこの表に出る。
   復帰にかかる時間 (`tab_resume_ms`) と、状態喪失そのもののユーザ影響は
   別項目のまま残る (D105 の「まだ分からないこと」参照)。
4. 27.1 の B (予算 OFF) が 754.9 MiB、27.3 の B が 889.9 MiB と 135 MiB
   違う。同じ「予算 OFF の 10 タブ」でこれだけ動くのは、27.1 では RSS の
   サンプル数が少なくタブが開ききる前の値を拾っていたためと考えられる。
   **27.1 の絶対値は使わないこと。**

### 27.4 タブ数スケーリングの測定 (Issue #197 の限界 2 / D93 子 Issue 案 A)

27.3 で得たのは **10 タブの 1 点だけ**である。D93 が「Windows の RSS の
**傾き**は未知」と書いた部分はまだ埋まっていない。

**なぜ 1 つの run に収める必要があるか**: D96 決定 3 のとおり
`windows-latest` は run ごとに機種が変わり、`startup_window_created_ms` の
絶対値は同じ head でも 23% 動く。**タブ数ごとに別の run で測ると、傾きに
機種差が混ざって読めなくなる。** そこで `perf-windows.yml` に
**複数シナリオを 1 つのジョブで順に計測する `scenarios` 入力**を足した。
各シナリオがそれぞれ A/B ペアを持ち、Job Summary には

- シナリオごとの A/B 比較表
- **タブ数スケーリング表** (`tabs_N` が 2 つ以上あるときだけ、
  `rss_total_bytes` を MiB で並べたもの)

が出る。**同一 run 内なのでスケーリング表の行どうしは比較してよい** —
それがこの入力を足した理由そのものである。

#### 実行条件

```text
scenarios:   tabs_1,tabs_5,tabs_10,tabs_20
trials:      3
compare_env: VELOX_MEMORY_BUDGET_MB=0
common_env:  VELOX_MEMORY_CHECK_INTERVAL_MS=500
repeats:     2
```

`common_env` が要る理由は 27.1 と同じ (既定 5 秒周期では `tabs_N` の
約 6 秒の生存中に予算が発動しない)。

#### 読み方

- **B (予算 OFF) の列がタブ数に対する素の傾き**である。VeloX が 1 タブ
  あたり何 MiB 増やすかは、この列を見る。
- **A (既定 700 MiB) の列は予算が効き始める点で折れる。** 折れ点より下では
  A ≈ B、上では A が予算近辺で頭打ちになるはずである。27.3 の 10 タブでは
  A 660.9 / B 889.9 MiB だったので、**折れ点は 5〜10 タブの間**にあると
  予想されるが、これは予想であって実測ではない。
- **A の頭打ちが 700 MiB をどれだけ下回るか**が、D56 のプロセスグループ
  単位回収によるオーバーシュートの Windows での大きさである
  (Linux は §25 事実 4 のとおり予算 700 に対し 464 MiB まで行き過ぎる)。

#### 結果 (run 34233777313, 2026-09-08 22:44〜22:49 JST)

**測定環境** (このジョブの env_info ステップから転記):

| 項目 | 値 |
| --- | --- |
| CPU | **Intel(R) Xeon(R) 6973P-C** (4 論理コア) |
| メモリ (物理) | 15.99 GiB |
| 実行元 | [run 34233777313](https://github.com/noan98/VeloX/actions/runs/34233777313) / job `102086190687` |
| 条件 | `tabs_1,tabs_5,tabs_10,tabs_20` / 3 試行 / A→B→A→B / `VELOX_MEMORY_CHECK_INTERVAL_MS=500` |

**この CPU は §21.1 (AMD EPYC 9V74) とも §26.4 (Intel Xeon 8573C) とも違う
3 種類目である。** D96 事実 2 (`windows-latest` は run ごとに別スペック) の
証拠がまた 1 つ増えた。

#### タブ数スケーリング

`rss_total_bytes` の平均 (MiB)。**同一 run 内なので行どうしを比較してよい。**

| シナリオ | A (既定 700 MiB) | B (予算 OFF) | 差 | 比 (B/A) |
| --- | ---: | ---: | ---: | ---: |
| `tabs_1` | 368.8 | 366.9 | -1.9 | 0.995 |
| `tabs_5` | 633.4 | 633.8 | +0.4 | 1.001 |
| `tabs_10` | 658.6 | 892.4 | +233.9 | 1.355 |
| `tabs_20` | **815.9** | 1166.2 | +350.4 | 1.429 |

#### 事実 1: 折れ点は 5〜10 タブの間にある (27.4 の予想どおり)

`tabs_1` と `tabs_5` は A と B の差が **±2 MiB 以内**で、予算が発動して
いない。`tabs_10` で初めて 233.9 MiB の差が出る。`tabs_5` の A が
**633.4 MiB** で予算 700 MiB を下回っていることと整合する。

> ⚠️ **§28.5 で訂正済み。** 以下の事実 2 と事実 3 は、`tabs_N` が
> 「回収が終わる前の途中の値」を測っていたことによる誤りである。待ちを
> 入れた `tabs_hold_N` では傾きは逓減せず (約 65 MiB/タブで線形)、
> 20 タブの A も 616.8 MiB と予算 700 MiB を下回る。**この節の数値は
> 「`tabs_N` で測るとこうなる」という記録として残すが、製品の挙動を
> 論じるときに引用してはならない。**

#### 事実 2: 素の傾き (B) は逓減する

| 区間 | 増分 | 1 タブあたり |
| --- | ---: | ---: |
| 1 → 5 タブ | +266.9 MiB | **66.7 MiB/タブ** |
| 5 → 10 タブ | +258.6 MiB | **51.7 MiB/タブ** |
| 10 → 20 タブ | +273.8 MiB | **27.4 MiB/タブ** |

**タブが増えるほど 1 タブあたりのコストが下がる。** WebView2 が
プロセスを再利用してページを相乗りさせるためと考えられる (D56 が
「プロセスグループ単位」と呼んでいるもの)。**線形外挿してはならない。**

#### 事実 3: **20 タブでは予算 700 MiB を守れていない**

`tabs_20` の A は **815.9 MiB** で、**予算を 115.9 MiB (16.6%) 超過して
いる。** 休止は効いており (B より 350 MiB 低い)、しかし**予算を上限として
守れてはいない。**

これは Linux (§25 事実 4) と**逆向きの外れ方**である。Linux は予算 700 に
対し 464 MiB まで**行き過ぎる** (オーバーシュート) のに対し、Windows は
**届かない**。同じ 700 という数値が、OS によって「厳しすぎる上限」にも
「守られない目安」にもなっている。

**原因は特定していない。** 候補は 3 つで、どれも未検証である:

1. **休止してもプロセスが解放されない。** WebView2 はプロセスを再利用する
   ため、休止したタブの分だけ RSS が下がるとは限らない。
2. **休止対象が足りない。** アクティブタブと toolbar 用プロセスは休止でき
   ないので、20 タブでも「必ず残る分」が大きい。
3. **休止が追いつかない。** `tabs_N` のスクリプトはタブを待ちなしで連続に
   開くため、20 タブを開き切るまでの間に 500ms 周期のサンプラが判定と回収を
   完了できない可能性がある。**この場合は測定条件の問題であって製品の挙動
   ではない。**

**3 を先に潰すべきである** — タブを開く間隔を空ける、あるいは開き終えて
から安定化を長く取るシナリオで測り直せば切り分けられる。1 と 2 なら
#176 (Memory Budget Manager) の設計課題そのものになる。

#### 事実 4: `page_load` は 2 タブ目以降ほぼゼロになる

`tabs_1` の `page_load_ms` は 16.15ms だが、`tabs_5` 以降は **0.1〜0.2ms**
である。同じ `minimal.html` を開き続けるためキャッシュに乗る。
**`tabs_N` の `page_load` をページ読み込み性能の指標として読んではいけない。**
`startup_first_load_ms` がタブ数に比例して伸びる (491 → 902 → 1549 →
2816ms) のも、自動操作スクリプトが全タブを開き終えるまでの時間を含むため
であり、起動性能ではない。

---

## 28. `tabs_hold_N` — 既定設定のままメモリ予算を測る (Issue #197, 2026-09-09)

§27.4 事実 3 で「20 タブでは予算 700 MiB を守れていない (815.9 MiB)」と
分かったが、原因として 3 つの候補が残った。そのうち **候補 3「休止が
追いつかない」は測定条件の問題**であり、製品の挙動を論じる前に潰さなければ
ならない。§27.3 の限界 1 (既定 5 秒周期での値が測れていない) も、根は同じ
である。

### 28.1 なぜ `tabs_N` では測れないのか

`tabs_N` が生成するスクリプトは、

1. `tab_count - 1` 個のタブを**待ちを挟まず連続で** `open` する
2. `MEMORY_STABILIZE_MS` (3 秒) 待つ
3. `quit`

という形で、プロセスの生存はおよそ **6 秒**しかない。一方、休止判定を行う
`app::spawn_memory_pressure_sampler` は `sleep(interval)` を**先に**実行
するため、既定の 5 秒周期では**判定が 0〜1 回しか走らない**。しかも 1 回
走ったとしても、その直後に `quit` が来るので回収の結果が RSS に現れない。

§27.3 / §27.4 はこれを `VELOX_MEMORY_CHECK_INTERVAL_MS=500` で回避したが、
**それは実ユーザの設定ではない。**

### 28.2 `tabs_hold_N` の形

`Scenario::TabCountMemoryHold`。`tabs_N` との違いは 3 点だけである。

1. **タブを開くたびに `STEP_SETTLE_MS` (300ms) 待つ** — 一気に開くと、
   休止判定が「まだ開いている途中」の状態を見てしまう。
2. **開き終えてから `MEMORY_HOLD_SETTLE_MS` (12 秒) 待つ** — 既定 5 秒
   周期でも判定が 2 回以上走る長さ。**環境変数を触らずに済むのがこの
   シナリオの存在理由である。**
3. **そのあとに `mark` を打ち、`MEMORY_HOLD_WINDOW_MS` (8 秒) 保持する** —
   `benchmark::aggregate_trials` は最後の `measure_start` 以降のイベント
   しか集計しないので、**得られる値は「落ち着いた後の定常値」**になる。
   タブを開いている最中の値は混ざらない。

RSS のサンプリング間隔は窓 (8 秒) を基準に決める (`MEMORY_HOLD_WINDOW_MS /
TARGET_STABILIZED_RSS_SAMPLES` = 2 秒)。`MEMORY_HOLD_SETTLE_MS` の側で
採れたサンプルは `mark` より前なので捨てられる。

1 試行の所要時間は 20 タブでおよそ 26 秒 ((20-1) × 0.3 + 12 + 8)。

### 28.3 これで答えが出ること

| 問い | 読み方 |
| --- | --- |
| §27.4 事実 3 の候補 3 (休止が追いつかない) か | `tabs_hold_20` の A が §27.4 の 815.9 MiB より**下がれば候補 3 が主因**。変わらなければ候補 1 (プロセスが解放されない) か 2 (休止対象が足りない) であり、#176 の設計課題になる |
| §27.3 の限界 1 (既定周期での値) | `common_env` を**使わずに** `tabs_hold_N` を回した A の列が、そのまま「既定設定の Windows ユーザが N タブ開いたときの値」になる |

### 28.4 実行条件

```text
scenarios:   tabs_hold_1,tabs_hold_5,tabs_hold_10,tabs_hold_20
trials:      3
compare_env: VELOX_MEMORY_BUDGET_MB=0
common_env:  (空 — 既定周期のまま測るのがこのシナリオの目的)
repeats:     2
```

**`common_env` を空にすることが重要である。** ここで
`VELOX_MEMORY_CHECK_INTERVAL_MS` を指定してしまうと、§27.4 と同じ
「実ユーザの設定ではない条件」に戻ってしまう。

### 28.5 結果 (run 34244194428, 2026-09-09 00:23〜00:41 JST) — **§27.4 の結論を覆した**

**測定環境**: `AMD EPYC 7763 64-Core` (4 論理コア) / RAM 15.99 GiB。
§21.1 (EPYC 9V74) / §26.4 (Xeon 8573C) / §27.4 (Xeon 6973P-C) と違う
**4 種類目**の CPU である (D96 事実 2)。条件は §28.4 のとおりで、
**`common_env` は空 = 既定の 5 秒周期のまま**。

| シナリオ | A (既定 700 MiB) | B (予算 OFF) | 削減 | 比 (B/A) |
| --- | ---: | ---: | ---: | ---: |
| `tabs_hold_1` | 367.2 MiB | 369.7 MiB | 0.7% | 1.007 |
| `tabs_hold_5` | 630.0 MiB | 629.1 MiB | -0.1% | 0.999 |
| `tabs_hold_10` | **463.0 MiB** | 950.0 MiB | **51.3%** | 2.052 |
| `tabs_hold_20` | **616.8 MiB** | 1602.6 MiB | **61.5%** | 2.599 |

#### 事実 1: **20 タブでも予算 700 MiB は守られていた**

§27.4 事実 3 は「20 タブの A が 815.9 MiB で予算を 16.6% 超過している」と
記録したが、**同じ 20 タブが、待ちを入れるだけで 616.8 MiB に収まる。**
予算 700 MiB を**下回っている。**

したがって §27.4 事実 3 の 3 候補のうち、**候補 3「休止が追いつかない」が
主因**だったと確定する。候補 1 (プロセスが解放されない) と候補 2 (休止
対象が足りない) は、**この結果では支持されない** — 十分な時間さえあれば
回収は最後まで進む。**`tabs_N` が測っていたのは「回収が終わる前の途中の
値」だった。**

#### 事実 2: **素の傾きは逓減しない。約 65 MiB/タブでほぼ完全に線形**

| 区間 | B (予算 OFF) の 1 タブあたり |
| --- | ---: |
| 1 → 5 タブ | **64.9 MiB/タブ** |
| 5 → 10 タブ | **64.2 MiB/タブ** |
| 10 → 20 タブ | **65.3 MiB/タブ** |

§27.4 事実 2 は「逓減する (66.7 → 51.7 → 27.4 MiB/タブ)」と読んだが、
**これは錯覚だった。** `tabs_N` はタブを開き切る前に測っていたため、
タブ数が多いほど「まだロードされていない分」が増え、見かけ上傾きが
寝ていただけである。**待てば、VeloX は 1 タブあたり約 65 MiB を素直に
積み上げる。**

この訂正の影響は大きい。20 タブの素の値は §27.4 の 1166.2 MiB ではなく
**1602.6 MiB** であり、**§27.4 の B 列は全体が過小評価**だった。

#### 事実 3: 休止の効果は §27.4 の想定よりはるかに大きい

20 タブで **-61.5% (-985.8 MiB)**。§27.4 の -30% の倍である。10 タブでも
**-51.3%**。しかも `tabs_hold_10` の A (463.0 MiB) は `tabs_hold_5` の A
(630.0 MiB) **より小さい** — 5 タブでは予算 700 に届かないので発動せず、
10 タブで初めて発動して 5 タブ時より下に落ちる。**予算がタブ数ではなく
メモリ量で効いていることの、分かりやすい現れである。**

#### 事実 4: Linux とほぼ同じ着地点

Linux (§25 事実 4) は 10 タブ・予算 700 で **464.1 MiB** に着地した。
今回の Windows 10 タブは **463.0 MiB**。**1 MiB 差**である。

§27.4 で「Linux は行き過ぎ、Windows は届かない」と書いたが、**待ち時間を
揃えると両 OS はほぼ同じところに着地する。** 「逆向きの外れ方」は OS 差
ではなく測定条件の差だった。プロセスグループが Linux にしかない
(`with_related_content_view` は Linux/BSD 限定) ことは事実だが、
**少なくとも着地点においては、その差は現れていない。**

### 28.6 これで訂正される記述

| 場所 | 訂正前 | 訂正後 |
| --- | --- | --- |
| §27.4 事実 2 | 傾きは逓減する (66.7 → 51.7 → 27.4) | **約 65 MiB/タブで線形** |
| §27.4 事実 3 | 20 タブで予算を 16.6% 超過 | **待てば 616.8 MiB で予算内**。超過は測定条件による |
| §27.4 (Linux 比較) | Linux は行き過ぎ、Windows は届かない | **待ち時間を揃えるとほぼ同じ (464.1 / 463.0 MiB)** |
| §27.3 限界 1 | 既定周期での値は未測定 | **本節が答え** (`common_env` 空で測定済み) |

**§27.3 / §27.4 の数値そのものは消さない** — 「`tabs_N` という条件で測ると
こうなる」という事実の記録であり、`tabs_N` を使う限り再現する。ただし
**製品の挙動を論じるときに引用してはならない。**

### 28.7 残る課題

1. ~~**`rss_process_count` を A/B 表に出していない。**~~ → **測定済み。
   §28.8 を参照。休止は WebView2 のプロセス単位で回収できていた。**
2. **`tabs_hold_50` は未測定。** 20 → 50 タブで線形が続くのか、予算が
   どこで頭打ちになるのかは分かっていない。
3. **Chromium との比較が Windows では取れていない** (T2 の目標
   「Chromium 比 +10% 以内」を Windows で評価できない)。`compare_browsers.py`
   は Linux 専用である。
4. ~~**休止による状態喪失のコストは未計測**~~ → **計測基盤ができた
   (D105)。** `MetricKey::SuspendedTabCount` (`suspended_tab_count`) を
   追加し、A/B 比較表・タブ数スケーリング表に列を足した。616.8 MiB のような
   着地値の隣に「背景タブを何個休止した結果か」を出せるようになったが、
   **実測はまだこれから** (`perf-windows.yml` を実行するのは本 PR の
   スコープ外)。復帰にかかる時間は別メトリクス `tab_resume_ms` の対象で
   あり、状態喪失そのもののユーザ影響 (スクロール位置・フォーム入力の
   喪失など) はどちらのメトリクスでも数値化されていない。

### 28.8 プロセス数の実測 (run 34247895418, 2026-09-09 00:56〜01:01 JST)

D97 Revisit condition (1) の答え。`rss_process_count` を A/B 比較表に
足して測り直した (`tabs_hold_1,tabs_hold_20` / 2 試行 / 1 ペア /
`common_env` 空)。ランナーは §28.5 と同じ **AMD EPYC 7763**。

| シナリオ | メトリクス | A (既定 700 MiB) | B (予算 OFF) | 比 (B/A) |
| --- | --- | ---: | ---: | ---: |
| `tabs_hold_1` | `rss_total_bytes` | 379.8 MiB | 364.4 MiB | 0.960 |
| | **`rss_process_count`** | **8** | **8** | **1.000** |
| `tabs_hold_20` | `rss_total_bytes` | 615.4 MiB | 1593.3 MiB | 2.589 |
| | **`rss_process_count`** | **11** | **27** | **2.455** |

#### 事実 1: 休止は WebView2 のプロセス単位で回収できている

**20 タブで 27 → 11、16 プロセスが実際に消えている。** 「webview を drop
してもプロセスが残るのではないか」という懸念 (D97 が候補 1 として挙げた
もの) は**否定された**。#176 の設計課題として、この問題は存在しない。

**RSS 比 2.589 とプロセス数比 2.455 がほぼ一致**していることも重要で、
**メモリ削減がプロセス削減にほぼ比例している**ことを意味する。

#### 事実 2: プロセス構成が数字で説明できる

| 状態 | プロセス数 | 内訳 |
| --- | ---: | --- |
| 1 タブ | 8 | 基本 7 + タブ 1 |
| 20 タブ (予算 OFF) | 27 | 基本 7 + タブ 20 |
| 20 タブ (既定) | 11 | 基本 7 + **生存タブ 4** |

1 タブ 8 と 20 タブ 27 の差がちょうど 19 (= タブ数の差) なので、
**タブ 1 個につき WebView2 プロセスが 1 個**、それとは別に**基本プロセスが
7 個**という構成が読み取れる。予算 ON の 11 は「基本 7 + 生存タブ 4」で、
**16 タブが休止されている**。

**生存タブが 4 という数は、Linux (§25) で「予算 700 の 20 タブは
`max_live_tabs=4` 相当に退化する」と記録した値と一致する。** 偶然かどうかは
分からない — プロセスグループの有無 (D97) が違うにもかかわらず同じ数に
落ち着いた理由は説明できていない。

#### 事実 3: 1 タブでは A のほうが RSS が大きい (379.8 vs 364.4 MiB)

比 0.960 で、**予算 ON のほうが 15.4 MiB 多い。** 1 タブでは休止対象が
無い (アクティブタブは休止できない) ので回収は起きず、**メモリサンプラの
スレッドと `process_map` の走査ぶんだけ増えている**と考えるのが自然である。
ただし**試行 2 回・1 ペアだけの差**であり、ばらつきの範囲を確かめていない。
**この 15 MiB を「常駐コスト」として引用してはならない** — 確かめるなら
試行数を増やして測り直すこと。

#### 見つかった不具合: スケーリング表が `tabs_hold_*` で出ていなかった

タブ数スケーリング表の対象判定が `^tabs_\d+$` だったため、
**`tabs_hold_20` がマッチせず、表が丸ごと出力されていなかった。**
§28.5 は A/B 比較表から手で MiB 換算して書いたので気付かなかった。
`^tabs_(?:hold_)?\d+$` に修正し、並び順も `tabs_hold_N` を数値順に扱う
ようにした。**`tabs_N` と `tabs_hold_N` は測定条件が違う**ので、同じ表に
並んでも系統をまたいで比較してはならない旨を表の説明にも入れた。

## 29. Windows で T2 を評価できるようにする (Issue #197, 2026-09-09)

### 29.1 何が問題だったか

**T2 (メモリで Chromium 比 +10% 以内) の評価は Linux でしか行えていなかった。**
比較ハーネス `scripts/bench/compare_browsers.py` が `/proc` 前提だったためである
(D97 Revisit condition (3))。

これは記録の欠落ではなく、**Stage 1 (Issue #176) を閉じられない理由**だった。
§28 で「Windows の自動タブ休止は正しく機能している (20 タブ -61.5%)」ことは確定
したが、それは VeloX の設定間 (予算 ON / OFF) の比較でしかない。T2 は競合との
比較なので、**Windows で競合を測る手段が無い限り達成も未達も言えない。**
CLAUDE.md が Windows を最優先と定めている以上、Windows で評価できない目標を完了
条件に据えることはできない。

### 29.2 Windows に PSS が無いことをどう扱うか

§3.1 に定義を書いたとおり、**近似値を作らず、真の PSS を上下から挟む。**

D88 が検討した `ShareCount` による `1/ShareCount` 近似は、`ShareCount` が
**3 bit で 7 に飽和する**ため、8 個以上のプロセスが同じページを共有する状況
(まさにブラウザ) で歪む。しかも歪み方がプロセス数に依存するので、プロセス構成の
違うブラウザを比べるという目的に対して最悪の性質を持つ。

代わりに `QueryWorkingSet` を 1 回呼んで、working set の各ページが共有かどうかを
数える。共有でないページの合計が Private Working Set = **下界**、全ページの合計が
Working Set = **上界**である。どちらも近似ではなく、OS が返す値をそのまま数えた
厳密な量である。

判定 (`compare_bounds`) は 3 値を返す。

| 判定 | 条件 | 意味 |
| --- | --- | --- |
| `met` | VeloX の**上界** <= 相手の**下界** × 1.10 | 真値がどこでも達成 |
| `missed` | VeloX の**下界** > 相手の**上界** × 1.10 | 真値がどこでも未達 |
| `inconclusive` | 区間が重なる | 真値の位置次第で結論が変わる |

**`inconclusive` を潰さないことがこの設計の要点である。** 幅がある以上、判定
できない場合は必ず存在する。そこで片方の端を代表値に選んで断定するのは、測れて
いないものを測れたことにする行為になる。Linux では下界 = 上界 (幅ゼロ) なので、
この判定はふつうの PSS 比較に退化し、**既存の評価方法を一切変えない。**

### 29.3 Windows でだけ可能になる切り分け — Edge との比較

Linux の比較は「VeloX 対 Chromium」であると同時に「WebKitGTK 対 Blink」でもあり、
**VeloX 自身のオーバーヘッドとエンジン差を分離できない** (Epic #57 の原則、§2)。

Windows では事情が変わる。**VeloX は WebView2、つまり Edge と同じ Chromium
エンジンを使う。** したがって Edge と比較すればエンジン差が相殺され、**残る差は
VeloX 自身のオーバーヘッドだけになる。** Linux では原理的に不可能だった切り分けが
Windows では可能になる。

`compare-windows.yml` の `baseline` 入力は既定 `auto` で **Edge を優先して探す**。
Chrome との比較も選べるが、そちらはエンジン差を含む (Linux と同じ性質) ことを
結果に明記する。

### 29.4 実装

| 追加 | 役割 |
| --- | --- |
| `scripts/bench/proctree.py` | プロセスツリーのメモリ計測を OS 非依存の形で 1 か所に集約。Linux は `/proc`、Windows は Toolhelp32 + `QueryWorkingSet` (ctypes) |
| `scripts/bench/compare_browsers.py` | 上記を使うよう変更。`DISPLAY` の確認を Linux 限定に、比較相手の自動検出、`compare_bounds` による T2 判定を追加 |
| `.github/workflows/compare-windows.yml` | `windows-latest` 上で実際に比較を走らせる (`workflow_dispatch` 限定) |

`process_tree_memory` は `compare_browsers.py` / `tab_scaling.py` / `tab_churn.py`
に**同じ実装が 3 つ重複していた**。Windows 対応で 4 つ目を増やすのは明らかに悪手
なので、`proctree.py` に集約して 3 スクリプトとも委譲に変えた。

プロセス一覧の取り方は `browser::metrics` の Windows 実装 (D88) と同じ
`CreateToolhelp32Snapshot`、RSS も同じ `GetProcessMemoryInfo` の `WorkingSetSize`
にそろえてある。**§28 の `rss_total_bytes` と同じ定義なので、並べて読める。**

### 29.5 まだ測っていない — この節は「測れるようにした」までである

**数値はまだ 1 つも無い。** この節が記録しているのは手段の整備だけである。

⚠️ **`proctree.py` の Windows 実装 (ctypes による FFI) は、`compare-windows.yml`
が走るまで一度も実行されていない。** 開発環境は Linux コンテナで Windows 実機が
無い (D61/D88 と同じ制約)。D88 が `QueryWorkingSetEx` の実装を見送った理由の 1 つが
「実機で検証する手段が無い」ことだったので、**検証手段を先に用意する**という順序に
してある。単体テストで固定できたのは木の走査・集計 (`collect_tree`) と判定
(`compare_bounds`) だけで、FFI 部分は実機で走るまで未検証と扱うこと。

### 29.6 初回実行 (run 34364067651) — 計測には到達しなかったが、確認できたこと

`compare-windows.yml` の初回実行は**失敗した**。ただし失敗したのは計測ロジック
ではなく、**日本語を `print` した行**である。

```
UnicodeEncodeError: 'charmap' codec can't encode characters in position 0-12
  File "compare_browsers.py", line 411, in main
    print(f"比較相手を自動検出しました: {detected_name} ({baseline_path})")
```

Windows の Python は標準出力が cp1252 になることがあり、日本語を出力しただけで
落ちる。**比較相手の自動検出も、その手前の単体テストもビルドも成功していた**のに、
表示の 1 行で計測全体が終わった。`sys.stdout.reconfigure(encoding="utf-8")` で
修正し、`PYTHONIOENCODING=cp1252` を与えれば Linux でも同じ条件を再現できるので
回帰テストにした。

計測には届かなかったが、**この run で確認できたことが 3 つある。**

**1. 単体テストは Windows でも通る。** Python 3.12.10 で 17 件成功。木の走査・集計
と判定ロジックが OS 非依存であることが実機で確かめられた。

**2. Edge との比較はエンジンが同一であると確認できた。** 環境記録ステップの値:

| 項目 | 値 |
| --- | --- |
| Edge | 151.0.4129.101 |
| **WebView2 Runtime** | **151.0.4129.101** |
| Chrome | 151.0.7922.174 |

**WebView2 Runtime と Edge のバージョンが完全に一致している。** §29.3 では
「VeloX は WebView2 = Edge と同じエンジンを使う」と設計上の理由から述べたが、
これは**実測による裏づけ**である。Chrome は別バージョン (151.0.7922.174) なので、
Chrome との比較にはエンジン差が含まれる。

**3. ランナーの構成。** Windows Server 2025 build 26100 / AMD EPYC 7763
(4 論理コア) / RAM 16 GiB。D96 のとおり `windows-latest` は run ごとに機種が
変わるため、**この値は次回も同じとは限らない。**

`proctree.py` の ctypes FFI (Toolhelp32 / `QueryWorkingSet`) は**まだ一度も実行
されていない** — 失敗が print で起きたため、そこまで到達していない。D99 Revisit
condition (2) は未解決のままである。

### 29.7 2 回目の実行 (run 34364659960) — 計測は成功したが、また表示で落ちた

文字コードを直して再実行すると、**今度は計測そのものが最後まで動いた** — ブラウザ
が起動し、`load` を検知し、メモリも採れた。それでもジョブは失敗した。落ちたのは
**試行ごとの進捗を表示する 1 行**である。

```
File "compare_browsers.py", line 451, in main
    f"pss={result['pss_bytes'] / 1024 / 1024:.1f}MiB "
TypeError: unsupported operand type(s) for /: 'NoneType' and 'int'
```

**サマリ表は Windows 対応にしたのに、この 1 行を直し忘れていた。** `pss_bytes` は
Windows では常に `None` である。

### 同じ失敗が 2 回続いた — 教訓

| 回 | 落ちた場所 | 計測そのもの |
| --- | --- | --- |
| 1 (§29.6) | 自動検出の結果を `print` する行 (cp1252) | 成功していた |
| 2 (本節) | 試行ごとの進捗を `print` する行 (`None` の割り算) | **成功していた** |

**どちらも計測は正しく動いていて、結果を表示する段で落ちている。** 私は
「計測ロジックを Windows 対応にする」ことに注意を集中し、**出力側を同じ厳しさで
見ていなかった。** OS 依存の値を `None` にする設計にした以上、**その値に触る
すべての箇所**が対象なのに、集計側 (`summarize` / サマリ表 / `compare_bounds`)
だけを直して、逐次表示を見落とした。

対策として進捗表示を純粋関数 `format_trial_line` に切り出し、**Linux 形
(`private_bytes` が `None`) と Windows 形 (`pss_bytes` が `None`) の両方を
ユニットテストで踏む**ようにした。文字コードの方も
`PYTHONIOENCODING=cp1252` で Linux から再現できるテストにしてある。
**どちらも「Windows 実機が無いと踏めない」失敗ではなかった** — テストの
形を先に用意していれば防げた。

ランナーは Intel Xeon Platinum 8370C (4 論理コア) で、§29.6 の AMD EPYC 7763 とは
別機種だった。D96 のとおり `windows-latest` は run ごとに機種が変わる。

### 29.8 3 回目の実行 (run 34365218545) — 数値が出た。そして判定できなかった

**Windows で VeloX と Edge を同一条件で比較した最初の数値である。**

環境: Windows Server 2025 build 26100 / AMD EPYC 9V74 (4 論理コア) / RAM 16 GiB /
Edge 151.0.4129.101 / WebView2 Runtime 151.0.4129.101 / `minimal.html` / 5 試行。

| ブラウザ | n | load 中央値 (ms) | メモリ下界 (MiB) | メモリ上界 (MiB) | プロセス数 |
| --- | ---: | ---: | ---: | ---: | ---: |
| velox | 5 | **750.0** | 88.9 | 370.0 | **8** |
| edge | 5 | 1984.0 | 173.3 | 692.5 | 15〜16 |

**T2 の判定: `inconclusive` (判定不能)。** VeloX [88.9, 370.0] と Edge
[173.3, 692.5] の区間が重なるため、真値の位置次第で結論が変わる。

### 区間が広すぎた — D99 Revisit condition (3) が的中した

D99 は「`private_bytes` が下界として十分に締まっているかは未知。区間が広くなり
`inconclusive` ばかりになる可能性がある」と書いていた。**まさにそうなった。**
幅は VeloX が 4.2 倍、Edge が 4.0 倍で、working set の 3/4 以上が共有ページだった。

**それでも判定を歪めなかったことは記録しておく。** VeloX は下界も上界も Edge より
小さい (88.9 < 173.3、370.0 < 692.5) ので、「VeloX の方が省メモリだ」と言いたく
なる。しかし T2 が問うているのは「Chromium 比 +10% 以内か」であり、**VeloX が真値
370.0 で Edge が真値 173.3 なら +113% で未達**になる。区間が重なる以上、真値の
位置次第で答えが変わる — だから判定不能と言うのが正しい。

### 上界の取り方を締めた (D99 決定1 の改訂)

現在の上界 (Working Set 合計) は、**共有ページを「1 プロセスしか使っていない」と
仮定したのと同じ**で、上界としては正しいが極端に緩い。`QueryWorkingSet` は
`ShareCount` も返すので、これで割れば遥かに締まる。

**D88 はこれを「近似値」として使うことを検討して見送った。** 懸念は `ShareCount` が
3 bit で 7 に飽和すること。**しかし上界として使うなら飽和は破綻しない:**

報告値を c、実際の共有プロセス数を n とすると

- c < 7 なら飽和していないので n == c
- c == 7 なら飽和しているので n >= 7 == c

**いずれの場合も n >= c**。よって各ページの寄与は
`page_size / n <= page_size / c` であり、

    private + Σ(page_size / c)  >=  真の PSS

が常に成り立つ。**飽和は上界を緩めるだけで、上界であること自体を壊さない。**
飽和ページが 1 枚も無ければ、この値は PSS そのものになる (等号成立)。

したがって挟み込みは 3 段になる:

    private_bytes  <=  真の PSS  <=  pss_upper_bytes  <=  rss_bytes (working set 合計)

ページ単位の計算は `working_set_pages` として **FFI の外に切り出し**、飽和した
場合を含めてユニットテストで固定した (§29.7 の教訓 — 検証の無い箇所で落ちる)。

### 併せて観測されたこと (T2 とは別)

- **プロセス数: VeloX 8 / Edge 15〜16。** VeloX は約半分である
- **load 到達: VeloX 750ms / Edge 1984ms。** ただしこれは起動込みの外形時間で、
  Edge のシェル起動コストを含む。「VeloX が 2.6 倍速い」と読むのは早計である
- 試行 3 で Edge の 1 プロセスが読めなかった。`unreadable_count` がそれを表に出した
  ので、その試行の合計が過小評価であることが分かる (**黙って落とさない**設計が効いた)
- ランナーは AMD EPYC 9V74。§29.6 が EPYC 7763、§29.7 が Xeon 8370C で、
  **3 回とも別機種**だった (D96 のとおり)

### 次に必要なこと

締めた上界で測り直すまで、**T2 の達成/未達は依然として言えない。** 締めても区間が
重なるなら、それが答えである — その場合は「Windows では現状の手段で T2 を判定でき
ない」ことを結論として記録し、別の手段を検討する。**判定できないことを判定できた
ことにしない。**

### 29.9 4 回目の実行 (run 34366896517) — 同じ関数を 2 回定義していた

上界を締めた版の初回実行は失敗した。原因は計測でも数式でもなく、
**`_windows_nodes` を 2 回定義してしまっていた**ことである。

スクリプトでファイルを書き換える際、削除範囲の終端に
`nodes: dict[int, tuple[int, _ProcMemory | None]] = {}` を使ったが、**この行は
Linux 側の `_linux_nodes` にも存在した。** 最初の一致が Windows 側より前だった
ため、意図した範囲ではなく前方を指し、結果として Windows ブロックが丸ごと複製
された。

**Python は後の定義で静かに上書きするだけで何も言わない。** 構文チェックも
既存のユニットテスト 29 件も通った。そして有効になった「後の定義」は古い実装で、
新しく切り出したヘルパを呼んでいたため、**Windows 上で実行してはじめて
`NameError` になった。**

### 実行なしで構造の壊れ方を捕まえる

Windows 専用のコードは実機がないと実行できない。この環境には静的解析ツールも
無い。そこで **AST を直接見て重複定義を弾くテスト**を足した。あわせて
「`_windows_nodes` が切り出した純粋関数を実際に呼んでいるか」も検査する —
古い実装が残っていると**テストで固定した計算が本番では使われない**という最悪の
形 (テストは通るのに数字が違う) になるためである。

人為的に重複を作って、このテストが実際に落ちることを確認した。

`_linux_nodes` / `_windows_nodes` のように「片方の OS でしか実行されない関数」は
今後も増える見込みなので、この検査は繰り返し効く。

### 失敗の性質が変わってきている

| 回 | 落ちた原因 | 実機でしか分からなかったか |
| --- | --- | --- |
| 1 (§29.6) | 日本語の `print` が cp1252 で落ちた | **いいえ** (`PYTHONIOENCODING` で再現できた) |
| 2 (§29.7) | 進捗表示が `None` を割った | **いいえ** (両 OS の形をテストできた) |
| 3 (§29.8) | — (成功。判定不能という結果) | — |
| 4 (本節) | 関数の重複定義 | **いいえ** (AST で検出できた) |

**4 回の失敗のうち、Windows 実機が必要だったものは 1 つも無い。** すべて
「Linux 側で踏める形にしていなかった」だけである。Windows 実機が無いことを
制約として繰り返し書いてきたが、**実際のボトルネックは検証の形を用意する手間
だった。**

### 29.10 5 回目の実行 (run 34367395559) — 上界を締めても、なお判定できなかった

環境: Windows Server 2025 build 26100 / AMD EPYC 7763 (4 論理コア) / RAM 16 GiB /
Edge 151.0.4129.101 = WebView2 Runtime 151.0.4129.101 / `minimal.html` / 5 試行。

| ブラウザ | n | load 中央値 (ms) | メモリ下界 (MiB) | メモリ上界 (MiB) | プロセス数 |
| --- | ---: | ---: | ---: | ---: | ---: |
| velox | 5 | **640.0** | 88.9 | **198.7** | **8** |
| edge | 5 | 1968.0 | 172.4 | 311.7 | 15 |

### 締めた効果は大きかった

| | 幅 (§29.8) | 幅 (本節) | 上界の縮小 |
| --- | ---: | ---: | ---: |
| velox | 4.2 倍 | **2.2 倍** | -46.3% |
| edge | 4.0 倍 | **1.8 倍** | -55.0% |

`ShareCount` で割ることで、上界は VeloX が 370.0 → 198.7 MiB、Edge が
692.5 → 311.7 MiB になった。**Working Set 合計がいかに緩い上界だったかが分かる。**

### それでも判定は `inconclusive` — しかも僅差である

    met の条件: VeloX 上界 198.7 <= Edge 下界 172.4 × 1.10 = 189.6
      → 198.7 > 189.6。**あと 9.1 MiB (4.8%) 足りない**

**4.8% 足りずに判定できなかった。** ここで注意すべきは、これは**上界どうしの
差であって、真の値の差ではない**ことである。VeloX の真の PSS は 88.9〜198.7 の
どこかにあり、実際にはもっと小さいかもしれない。**「あと少しで達成だった」と
読んではいけない** — 分かったのは「達成とは言い切れない」ことだけである。

念のため確認すると、**「VeloX は Edge 以下」という、T2 より弱い主張すら言えない**
(それには VeloX 上界 198.7 <= Edge 下界 172.4 が必要)。

### なぜこれ以上締められないのか — 手法の限界

`ShareCount` が 7 で飽和する一方、**Edge は 15 プロセスある。** 15 プロセスが
共有する DLL ページは c=7 と報告されるので、そのページの寄与を 1/15 ではなく
1/7 で数えている。**飽和したページでは最大 2 倍ほど多く見積もっている。**

これを解消するには「同じ物理ページを複数プロセスで数えている」ことを検出して
重複を除く必要がある。しかし **`QueryWorkingSet` が返すのは仮想ページ番号で、
物理ページの同一性は分からない。** 同じ物理ページでもプロセスごとに仮想アドレスが
違うため、突き合わせる手段がない。

つまり **`QueryWorkingSet` だけを使う限り、この幅がこの手法の限界である。**

### 結論 — Windows では現状の手段で T2 を判定できない

D99 Revisit condition (1) に「締めても重なるなら、それが答えである」と先に
書いておいた。**その答えが出た。**

**Windows における T2 (Chromium 比 +10% 以内) は、現状の計測手段では達成とも
未達とも判定できない。** これは測定の失敗ではなく、**Windows が PSS 相当を
提供していないことから来る原理的な限界**である。Stage 1 (Issue #176) を
「T2 達成」で閉じることはできない。

判定できるようにするには、`QueryWorkingSet` ではない手段が要る。候補:

- **ETW / Windows Performance Toolkit** — カーネル側のメモリイベントを採る。
  実装コストは大きい
- **`NtQuerySystemInformation(SystemSuperfetchInformation)`** — 物理ページ単位の
  情報が得られるとされるが**非公開 API** であり、計測ツールとはいえ依存するのは
  慎重に判断すべき
- **Windows 向けに T2 の定義自体を見直す** — 例えば「Private Working Set で
  比較する」など、Windows で厳密に測れる量で目標を定義し直す。**測れない量で
  目標を決めていること自体が問題**とも言える

### 併せて観測されたこと (T2 とは別、いずれも 1 環境 1 回の測定)

- **プロセス数: VeloX 8 / Edge 15。** VeloX は約半分
- **Private Working Set: VeloX 88.9 / Edge 172.4 MiB。** これは厳密な量どうしの
  比較なので、**私有メモリに限れば VeloX は Edge の約 52% である**と言える
  (ただし共有ページを一切含まないので、これを「メモリ使用量」と呼んではいけない)
- load 到達は VeloX 640ms / Edge 1968ms。**起動込みの外形時間**で Edge のシェル
  起動コストを含むため、そのまま「3 倍速い」と読むのは早計である
- 試行 1 で Edge の 1 プロセスが読めなかった (`unreadable_count` が表に出した)
- ランナーは AMD EPYC 7763。5 回の実行で 3 機種を踏んだ (D96 のとおり)

## 30. T2-W — Windows で評価できるメモリ目標 (Issue #197, 2026-09-09)

### 30.1 なぜ新しい目標が要るのか

**T2 は PSS で定義されている。Windows に PSS は無い。** §29.10 で、区間で挟む方法を
限界まで締めても判定できないことが確定した (原理的な限界であり、測定の失敗ではない)。

CLAUDE.md は Windows を最優先と定めている。**最優先 OS で評価できない目標を
Stage 1 (#176) の完了条件に据えることはできない。** T2 を Windows で評価する手段が
無い以上、Windows には Windows で判定できる目標が要る。

### 30.2 定義

**T2-W: `compare_bounds` が `met` を返すこと。** すなわち

    VeloX のメモリ上界  <=  比較相手のメモリ下界 × 1.10

上界・下界の意味は §3.1 のとおり (下界 = Private Working Set 合計、上界 = 共有ページを
`ShareCount` で割った和)。**どちらも近似ではなく厳密な量**なので、この判定は
「真の PSS がどこにあっても成り立つ」ことを意味する。

現在の実測 (§29.10):

    VeloX 上界 198.7  >  Edge 下界 172.4 × 1.10 = 189.6      → **4.8% 不足で未達**

### 30.3 なぜ Private Working Set を目標にしなかったのか

**それは目標として機能しないからである。** 実測は VeloX 88.9 / Edge 172.4 MiB
(VeloX は相手の **51.6%**)。「Private Working Set で +10% 以内」という目標は
**既に大差で達成済み**であり、そのように定義し直すことは

- **改善を 1 ミリも駆動しない** (既に通っているので、何もしなくても「達成」になる)
- **測定を都合よく変える**行為そのものである (この Issue で繰り返し戒めてきたこと)

Private Working Set は**共有ページを一切含まない**ため、これを「メモリ使用量」と
呼ぶこともできない。したがって Private Working Set とプロセス数は**回帰ガードの
補助指標**として記録するに留め、**目標には据えない。**

| 指標 | 役割 | 現在値 |
| --- | --- | --- |
| 区間比較 (`compare_bounds`) | **T2-W の判定** | `inconclusive` (4.8% 不足) |
| Private Working Set 合計 | 回帰ガード (厳密に測れる) | VeloX 88.9 / Edge 172.4 MiB |
| プロセス数 | 回帰ガード (厳密に測れる) | VeloX 8 / Edge 15 |

### 30.4 達成手段に課す制約 — ここが最も重要である

T2-W は 2 通りの経路で達成しうる。**片方は正当で、もう片方は不正である。**

**(a) VeloX が実際にメモリを減らす** — 上界が下がって `met` になる。正当。

**(b) 測定の区間が締まる** — 上界の見積もりが下がって `met` になる。**厳密な上下界を
保つ改善に限り正当。** 実際 §29.8 → §29.10 でこれを一度行っている
(`ShareCount` による上界の締め直しで、上界が 46% 縮んだ)。

> ⚠️ **(b) で近似を使ってはならない。** 「たぶんこのくらい」で幅を詰めて `met` を
> 出すのは、**測れていないものを測れたことにする**行為であり、この Issue で
> 繰り返し踏んだ誤り (§27.4 の誤結論、D97 の構造的決めつけ) と同じである。
> 区間を締める変更を入れるときは、**なぜそれが厳密な上界/下界であり続けるのかを
> 証明つきで記録すること** (D99 決定1 が `ShareCount` の飽和について行ったように)。

**(b) だけで `met` に到達した場合は、そう明記すること。** 「測り方が良くなって
判定できるようになった」のと「VeloX が省メモリになった」のは別の主張である。

### 30.5 T2 との関係 — T2-W 達成は T2 達成ではない

**T2-W は T2 より弱い主張である。**

- T2 は PSS という 1 つの値どうしの比較。
- T2-W は「真の PSS がどこにあっても +10% 以内」という**区間についての主張**。
  上界で比較するので、**真の PSS で比べればもっと余裕がある**可能性が高い。

逆に言えば **T2-W を満たせば T2 も満たす** (真の PSS ≤ 上界なので)。つまり
T2-W は T2 の**十分条件**である。**しかし Linux の PSS 比較の結果をそのまま
置き換えるものではない** — OS をまたいだ数値比較は成立しない (Epic #57 の
絶対ルール5、D88)。

> ⚠️ **「十分条件」が成り立つのは、比較相手が同じものを指しているときだけである。**
> 上の「T2-W ⇒ T2」は、**真の PSS ≤ 上界**という不等式から出る。この不等式は
> VeloX 側にも比較相手側にも同じように立つので導出自体は正しいが、それが
> 「T2 を満たした」と言えるかどうかは **T2 の言う「Chromium」と T2-W が実際に
> 測っている相手が同じか**に依存する。現在の実測は **Edge** で取っており、
> **Chrome の数値はまだ 1 度も取っていない** (§29.6〜§29.10 の実測はすべて
> Edge が相手である)。**比較相手を Edge に
> 固定してよいかは未検討である** (D101 Revisit (3))。Edge を優先している理由は
> §29.3 (WebView2 と同一エンジンなのでエンジン差が相殺される) であり、§29.6 で
> バージョン一致を実測して裏づけたが、**それは「T2 の言う Chromium が Edge で
> よい」ことまでは示していない。**
>
> したがって T2-W を達成しても、**その時点で書けるのは「Edge に対して +10%
> 以内」までである。** 「Windows で T2 相当を満たした」と書くには、比較相手の
> 妥当性を先に片付けること。§30.3 で Private Working Set への定義変更を退けたのと
> 同じ理由である — **まだ検証していない前提を結論の側に混ぜてはいけない。**

T2 (Linux/PSS) は目標として残す。§12・§25 のとおり未達である。

### 30.6 現状と、Stage 1 (#176) の扱い

| 目標 | OS | 状態 |
| --- | --- | --- |
| T2 | Linux (PSS) | **未達** (§12・§25) |
| T2-W | Windows (区間) | **未達** (4.8% 不足、§29.10) |

**どちらも未達なので、Stage 1 を「メモリ目標達成」で閉じることはできない。**
ただし §29.10 で分かったとおり、T2-W は 4.8% しか離れていない。**上界を締める
余地 (§29.10 の「なぜこれ以上締められないのか」) か、実際のメモリ削減か、
どちらかで届く距離にある。**
