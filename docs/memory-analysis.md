# VeloX メモリフットプリント切り分け (Issue #61)

Epic #57 / Issue #58 (docs/performance-targets.md) の baseline が示した
「VeloX は Chromium より PSS で 28〜29% 重い」という結果を受けて、**「なぜ
WebKitGTK ベースの VeloX が Blink (Chromium) より PSS で重いのか」を実測で
切り分けるのが本文書の唯一の目的である。**

> **この Issue はコード変更を一切含まない。** ここに書かれているのは実測
> データと、そこから導かれる「次にどこを見るべきか」の結論だけである。
> メモリを削減する実装は #62/#63 以降のスコープ。

---

## 1. 測定条件

| 項目 | 値 |
| --- | --- |
| VeloX commit | `d1f6fef82473de9f6cda135464c374c05254d503` (branch `claude/issue-61-memory-split`) |
| OS | Ubuntu 24.04.4 LTS |
| カーネル | 6.18.44-fc-v24 |
| CPU / メモリ | 4 コア / 15 GiB (`docs/performance-targets.md` §1 と同一コンテナ) |
| ディスプレイ | Xvfb `-screen 0 1280x900x24` (+ `dbus-run-session`、VeloX 起動時のみ) |
| GPU | なし (ソフトウェアレンダリング) — 絶対値は実機と乖離する。相対比較として読むこと |
| WebKitGTK | 2.52.6 |
| Chromium | 141.0.7390.37 (`/opt/pw-browsers/chromium`) |
| rustc | 1.94.1 |
| ページ | `scripts/bench/pages/minimal.html`(既定)、一部 `dom_heavy.html` |

**すべての数値は複数試行の中央値、または明示した範囲 (min–max) で示す。**
1 回の測定だけで結論を出している箇所はない (§7 に各測定の試行回数とばらつきを
まとめた)。

### 使ったツール

| 目的 | ツール |
| --- | --- |
| 競合比較 (外形 PSS) | `scripts/bench/compare_browsers.py` (既存, D41) |
| プロセスごとの PSS 内訳 | `scripts/profile/process_breakdown.py` (**本 Issue で新規追加**) |
| VeloX 自身の Rust heap | `heaptrack` 経由 `scripts/profile/run_heaptrack.py` (既存, D45) |
| タブ数に対する増え方 | `scripts/bench/tab_scaling.py` (**本 Issue で新規追加**。理由は §4) |
| エンジンだけの比較 | `minimal_webkitgtk.c` (使い捨ての最小 WebKitGTK C プログラム。VeloX のビルド成果物ではないためリポジトリには含めていない。§6 に全文とビルド手順を記載) |

---

## 2. プロセス別 PSS 内訳 (1 タブ、`minimal.html`)

### 2.1 VeloX

`scripts/profile/process_breakdown.py` を 3 回実行 (settle 6 秒、既定ホーム
ページ 1 タブのみ、`VELOX_AUTOMATION_SCRIPT` なし)。

| trial | velox 本体 (MiB) | WebKitWebProcess ×2 合計 (MiB) | WebKitNetworkProcess ×2 合計 (MiB) | 合計 PSS (MiB) |
| --- | ---: | ---: | ---: | ---: |
| 1 | 70.5 | 310.8 | 35.4 | 416.7 |
| 2 | 70.9 | 309.9 | 35.5 | 416.3 |
| 3 | 70.5 | 309.3 | 35.6 | 415.4 |
| **中央値** | **70.5** | **309.9** | **35.5** | **416.3** |

**内訳比率 (中央値ベース)**: velox 本体 17.0% / WebKitWebProcess 74.4% /
WebKitNetworkProcess 8.5%。

**プロセス数は常に 5** — `velox` 本体 1 + `WebKitWebProcess` 2 +
`WebKitNetworkProcess` 2。**`WebKitWebProcess`/`WebKitNetworkProcess` が
「2 つずつ」存在するのは、タブが 1 つしか開いていないのに VeloX がエンジンの
webview インスタンスを 2 つ (toolbar 用 + content タブ用) 同時に持っている
ため** — `docs/architecture.md` の D3 (「Browser chrome as an HTML toolbar
in a second webview」) が意図した設計そのものであり、バグではない。詳細と
その帰結は §5 で扱う。

`scripts/bench/compare_browsers.py` (既存の競合比較スクリプト、beacon 到達 +
settle 3 秒で採る) でも同一コミットを 2 セット×5 試行で再計測し、クロス
チェックした:

| セット | VeloX PSS 中央値 (MiB) | Chromium PSS 中央値 (MiB) |
| --- | ---: | ---: |
| 1 | 419.8 | 327.7 |
| 2 | 420.3 | 326.0 |

`process_breakdown.py` (settle 6 秒) の 415.4〜416.7 MiB と
`compare_browsers.py` (settle 3 秒) の 419.8〜420.3 MiB は約 1% の差 — 採取
タイミングの違いによる範囲内であり、**元の baseline (`docs/performance-
targets.md` §4 の 423.6 MiB) とも一致する。** この commit でも
Chromium 比 **+28.1%〜+29.0%** (このセッションの 2 セット) で、baseline の
+28.8% と整合する。

### 2.2 Chromium

同じ `process_breakdown.py` を Chromium に対して 2 回実行 (settle 5 秒)。

| trial | 合計 PSS (MiB) | プロセス数 |
| --- | ---: | ---: |
| 1 | 318.2 | 8 |
| 2 | 318.6 | 8 |

全プロセスが `comm=chrome` で表示され (Chromium はプロセスの役割を `comm` に
出さない)、VeloX のように「toolbar 用/content 用」と役割別に名前で分離する
ことはできなかった。**最大 PSS のプロセス 1 つ (約 145 MiB, 全体の 45%
程度) がメインプロセス相当** で、残りは zygote/レンダラ/GPU/utility 等の
ヘルパー群 (各 12〜47 MiB) に分散している — Chromium は「1 つの大きな
プロセス + 多数の小さなヘルパー」、VeloX は「小さめの本体 + 2 組の中〜大型
ペア」という異なる分布になっている。

`compare_browsers.py` では 326.0〜327.7 MiB・プロセス数 9 (元 baseline の
328.9 MiB とほぼ一致)。`process_breakdown.py` の 8 との差は、`fork` 後すぐ
別プロセスグループに再親される `chrome_crashpad_handler` (§2.1 と同じ木の
辿り方では祖先が root pid から辿れなくなる、実際に `ps` で確認した) を
拾えていない分と考えられる — 1 プロセスの差であり、結論に影響する規模では
ない。

---

## 3. VeloX 自身の Rust heap (`heaptrack`)

`heaptrack` は `LD_PRELOAD` で対象プロセス 1 つだけを記録する (D45/D42 が
すでに確認済み: `WebKitWebProcess`/`WebKitNetworkProcess` 用の `.gz` は
生成されない)。**この境界がそのまま「VeloX 自身の Rust/GTK 側 malloc」と
「WebKit エンジン側」の切り分けになる。**

`target/profiling/velox` (debug symbols 付き、D45) を `heaptrack` で
記録し、`heaptrack_print` の `peak heap memory consumption` を見た:

| 条件 | 試行 | peak heap (MiB) |
| --- | --- | ---: |
| `minimal.html`、1 タブ | 1 | 32.08 |
| `minimal.html`、1 タブ | 2 | 32.08 |
| `minimal.html`、1 タブ | 3 | 32.08 |
| `dom_heavy.html` (5000 `<div>`)、1 タブ | 1 | 32.08 |
| `minimal.html`、自動操作で 5 タブまで開く | 1 | 32.08 |
| `minimal.html`、自動操作で 5 タブまで開く (クリーンな再測定) | 1 | 32.08 |

**6 回すべてで一致した。** ページの DOM が軽い (`minimal.html`) か重い
(`dom_heavy.html`、5000 要素) かに関わらず同じ値なのは、VeloX の Rust 側は
DOM をまったく保持しないという設計 (エンジンの責務、D4/architecture.md) と
整合する。タブを 1→5 個に増やしても変わらないのは、`Tab`/`ContentTab`
構造体 (URL・タイトル・ロード中フラグ程度) が heaptrack の 2 桁 MiB の
解像度では見えない小ささだということを意味する。

**全体に対する比率**:

| 基準 | Rust heap (32.08 MiB) の比率 |
| --- | ---: |
| VeloX 本体プロセスの PSS (§2.1、70.5 MiB) 比 | 45.5% |
| 1 タブ時のツリー全体 PSS (§2.1、416.3 MiB) 比 | 7.7% |
| 20 タブ時のツリー全体 PSS (§4、2421.9 MiB) 比 | 1.3% |

**VeloX 本体プロセスの PSS の半分弱は Rust/GTK 側の実際の malloc heap で
説明でき、残り半分は共有ライブラリ (`libwebkit2gtk`/`libgtk`/`libcairo` 等)
のコード/データページが按分計上された分である。** いずれにせよツリー全体
から見ると Rust heap は 1〜8% 程度に過ぎず、**タブが増えるほど比率はさらに
薄まる。** これは「もし数 MiB 程度なら、削っても全体にはほぼ効かない」と
いう Issue 起票時の懸念に対する直接の答えであり、実際その通りだった:
**Rust heap を工夫して減らしても、ツリー全体の PSS への寄与は 1 桁 % の
範囲を出ない。**

---

## 4. タブ数に対する増え方 (1 / 5 / 10 / 20 タブ)

### 4.1 なぜ `velox-bench run --scenario tabs_N` をそのまま使わなかったか

最初に `velox-bench run --scenario tabs_1/5/10/20 --trials 5` を素直に
実行したところ、**`pss_total_bytes` の中央値がタブ数に関係なくほぼ一定
(134〜147 MiB) という、明らかにおかしい結果になった**:

| scenario | pss_total_bytes 中央値 (MiB, 5 試行) |
| --- | ---: |
| tabs_1 | 140.5 |
| tabs_5 | 132.1 |
| tabs_10 | 128.2 |
| tabs_20 | 131.3 |

原因を辿ると計測方法のギャップだった: VeloX の RSS/PSS サンプラ
(`spawn_rss_sampler`, `src/app.rs`) は `VELOX_PERF_RSS_INTERVAL_MS`
(既定 5000ms, `config::DEFAULT_PERF_RSS_INTERVAL`) 間隔で**起動直後から
即座に**サンプリングを始めるループで、`velox-bench` はこのフラグを明示
指定しない限り既定値のまま渡す。一方 `tabs_N` の自動操作スクリプト
(`browser::automation::generate_bench_script` の `TabCountMemory` 分岐)
は `open` を待ち時間なしで連続実行し、最後に 1 回だけ 3 秒待って `quit`
する。**シナリオ全体の所要時間が 5000ms 未満で終わることが多く
(`tabs_1`/`tabs_5` は実測で 3〜4 秒程度)、この場合サンプラの「起動直後の
1 回目」の値しか記録に残らない** — タブが開き終わる前 (またはごく初期)
の状態を測っていたことになる。**`velox-bench` の `tabs_N` シナリオの
`pss_total_bytes`/`rss_total_bytes` は、現状ではタブ数に対する増え方の
指標として使えない**ことを実測で確認した (D48 に記録)。

そのため本 Issue では `scripts/bench/tab_scaling.py` を新規に書いた
(§1 の表、コード内 docstring に同じ説明がある)。VeloX には各 `open` の後に
明示的な `wait` を挟む自動操作スクリプトを渡し、Chromium にはコマンドライン
に URL を複数渡し (Chromium は追加の URL 引数をそれぞれ新規タブとして開く)、
**タブを全部開き終えて安定させてから** 1 回だけプロセスツリー全体の PSS を
採る。

### 4.2 結果

`minimal.html`、各タブ数につき 3 試行、中央値と範囲 (min–max):

| タブ数 | VeloX PSS 中央値 (MiB) | VeloX 範囲 | VeloX プロセス数 | Chromium PSS 中央値 (MiB) | Chromium 範囲 | Chromium プロセス数 |
| ---: | ---: | --- | ---: | ---: | --- | ---: |
| 1 | 409.6 | 409.5–410.4 | 5 | 276.7 | 276.6–278.3 | 8 |
| 5 | 777.7 | 758.0–825.2 | 13 | 315.6 | 313.9–320.3 | 12 |
| 10 | 1387.0 | 1384.2–1389.2 | 23 | 366.0 | 365.5–368.1 | 17 |
| 20 | 2421.9 | 2345.6–2447.0 | 43 | 464.8 | 463.6–465.2 | 27 |

再現コマンド:

```sh
xvfb-run -a --server-args="-screen 0 1280x900x24" dbus-run-session -- \
  python3 scripts/bench/tab_scaling.py \
    --velox ./target/release/velox --chromium /opt/pw-browsers/chromium \
    --page minimal.html --tab-counts 1,5,10,20 --trials 3 \
    --output results/tab-scaling.json
```

**50 タブは実行していない** (Issue 本文が時間がかかるなら省略可としている
項目。20 タブまでの傾向が既に十分明確だったため、50 タブは今回のスコープ外
とした — §7 の「できなかったこと」参照)。

### 4.3 1 タブあたりの増分

| ブラウザ | 1→5 タブ (MiB/タブ) | 5→10 タブ (MiB/タブ) | 10→20 タブ (MiB/タブ) | 1→20 タブ 全体平均 (MiB/タブ) |
| --- | ---: | ---: | ---: | ---: |
| VeloX | 92.0 | 121.9 | 103.5 | **105.9** |
| Chromium | 9.7 | 10.1 | 9.9 | **9.9** |

**VeloX の 1 タブあたりの PSS 増分 (約 106 MiB/タブ) は Chromium (約
9.9 MiB/タブ) の約 10.7 倍。** プロセス数の増え方もこれと対応している:
VeloX は追加タブ 1 個ごとに正確に **+2 プロセス** (5→13→23→43、`+4 タブ`
で `+8`、`+5 タブ`で `+10`、`+10 タブ`で `+20`)、Chromium は追加タブ 1 個
ごとに概ね **+1 プロセス** (8→12→17→27)。

**この +2/タブが何なのかは §2.1 の内訳とソースコードから特定できる**:
VeloX は `wry::WebViewBuilder::new()` を webview を作るたびに (`toolbar`
用に 1 回、各タブの content webview 用にそのタブごとに 1 回ずつ)
呼んでおり、`.web_context(...)` で既存の `WebContext` を明示的に共有させて
いる箇所はソース中どこにも無い (`grep -rn "\.web_context(" src/` はゼロ件、
`src/ui/window.rs`)。`wry` 0.56.1 の WebKitGTK バックエンド
(`~/.cargo/registry/.../wry-0.56.1/src/webkitgtk/mod.rs` 257〜268 行目) は
`attributes.context` が渡されていなければ `WebContext::builder()` 由来の
**新しい `WebContext` を毎回作る** — WebKitGTK は 1 つの `WebContext` ごと
に独立した `WebProcess`/`NetworkProcess` のプールを持つため、**VeloX が
webview を作るたびに新しい `WebContext` を作っていることが、webview の
数だけ `WebKitWebProcess`+`WebKitNetworkProcess` のペアが増える直接の
原因である。** 実測でも 5 タブ自動操作 (トップページ + 追加 4 タブ + toolbar
= webview 6 個) で `WebKitWebProcess`/`WebKitNetworkProcess` がそれぞれ
ちょうど 6 個ずつ生成されることを確認した (§5.2)。

Chromium も (サイト分離により) タブごとにレンダラプロセスを分けるのは通常
仕様だが、**ネットワークプロセス・GPU プロセス・zygote はブラウザ全体で
共有**されるため、タブが増えてもプロセス増分は軽量なレンダラ 1 個だけで
済む。VeloX は毎回「レンダラ相当 (WebProcess) + ネットワークプロセス相当
(NetworkProcess) のペア」が増える分、増分がまるごと重くなっている。

---

## 5. VeloX の 2-webview アーキテクチャの寄与

### 5.1 1 タブでも webview が 2 つある

`docs/architecture.md` (D3) の設計どおり、VeloX は常に「toolbar 用の
webview 1 つ + アクティブ/開いている各タブ用の content webview」を同時に
持つ。タブを 1 つしか開いていなくても webview は 2 つ (toolbar + content)
存在し、§2.1 で確認したとおり実際に `WebKitWebProcess`/
`WebKitNetworkProcess` がそれぞれ 2 個ずつ生成される。

### 5.2 プロセス数を直接数えた検証

自動操作スクリプトで 4 タブを追加 (トップページ込みで content webview 5 個
+ toolbar 1 個 = webview 6 個) して起動後に確認:

```
webprocs: 6   (WebKitWebProcess の数)
```

（`ps -eo pid,ppid,comm` でも `WebKitNetworkProcess`/`WebKitWebProcess` が
それぞれちょうど 6 個、すべて VeloX 本体プロセスの直接の子であることを確認
済み。）**webview の数と `WebKitWebProcess`/`WebKitNetworkProcess` の数が
常に 1:1 で一致する** — これは §4.3 で述べた「webview を作るたびに新しい
`WebContext` を作っている」ことの直接の帰結である。

### 5.3 エンジンだけの比較 (最小 WebKitGTK アプリ)

VeloX の Rust コードを一切含まない、GTK ウィンドウ 1 つ + `WebKitWebView`
1 つだけの最小 C プログラムを書いて (`gtk_init` → ウィンドウ作成 →
`webkit_web_view_new()` → `minimal.html` を読み込み → 保持するだけ)、
`process_breakdown.py` で同じ条件 (settle 5 秒) で計測した。

```c
/* 全文 (使い捨て、リポジトリには含めていない) */
#include <gtk/gtk.h>
#include <webkit2/webkit2.h>
#include <stdlib.h>

static gboolean quit_cb(gpointer data) { gtk_main_quit(); return FALSE; }

int main(int argc, char **argv) {
  gtk_init(&argc, &argv);
  const char *url = argc > 1 ? argv[1] : "http://127.0.0.1:8731/minimal.html";
  int wait_secs = argc > 2 ? atoi(argv[2]) : 30;
  GtkWidget *window = gtk_window_new(GTK_WINDOW_TOPLEVEL);
  gtk_window_set_default_size(GTK_WINDOW(window), 1280, 900);
  WebKitWebView *webview = WEBKIT_WEB_VIEW(webkit_web_view_new());
  gtk_container_add(GTK_CONTAINER(window), GTK_WIDGET(webview));
  webkit_web_view_load_uri(webview, url);
  gtk_widget_show_all(window);
  g_timeout_add_seconds(wait_secs, quit_cb, NULL);
  gtk_main();
  return 0;
}
```

```sh
gcc minimal_webkitgtk.c -o minimal_webkitgtk \
  $(pkg-config --cflags --libs gtk+-3.0 webkit2gtk-4.1)
```

| trial | 本体プロセス (MiB) | WebKitWebProcess (MiB) | WebKitNetworkProcess (MiB) | 合計 PSS (MiB) |
| --- | ---: | ---: | ---: | ---: |
| 1 | 85.2 | 188.8 | 25.4 | 299.4 |
| 2 | 85.1 | 185.0 | 25.6 | 295.6 |

**単一 webview の WebKitGTK アプリの合計 PSS (約 296〜299 MiB) は Chromium
1 タブ (約 318〜328 MiB) より軽い。** これは重要な結果で、**「WebKitGTK
(Blink 比) がエンジンとして PSS で重い」という証拠はこの環境では見つからな
かった**ことを意味する。むしろ単一 webview の WebKitGTK は Chromium より
軽い。

VeloX の 1 タブ時 (§2.1、415〜420 MiB) との差は約 116〜125 MiB —
これは §4.3 で見た「webview 1 個あたりの増分 (約 92〜122 MiB、平均 106
MiB)」とほぼ同じ大きさであり、**VeloX が単一 webview の WebKitGTK ベース
ラインより重い分は、ほぼ全部「2 個目の webview (toolbar)」1 個分の
コストで説明がつく。**

まとめると、3 つの独立した測定 (§2〜§5) が同じ結論を指している:

```
単一 webview の WebKitGTK        ≈ 296〜299 MiB   ← エンジンの床
Chromium (1 タブ)                ≈ 318〜328 MiB
VeloX (1 タブ、webview 2 個)     ≈ 415〜420 MiB   ← 296〜299 + webview 1個分
```

**エンジン単体では VeloX は不利になっていない。VeloX が Chromium より重く
見えている分は、ほぼ「toolbar を content と別の webview として持ち、かつ
webview ごとに独立した `WebContext` (→ 独立した `WebProcess`+
`NetworkProcess` のペア) を作っている」という VeloX 自身の実装 (より正確
には wry の呼び出し方) に起因する。**

### 5.4 この先どこまで踏み込めるか (未検証、仮説として明記)

**以下は実装・計測していない仮説であり、本 Issue の結論には含めるが、
検証は #62 以降に委ねる。**

`wry` 0.56.1 のソース (`webkitgtk/web_context.rs`) を読む限り:

- **非プライベートモード (`config.private == false`、既定値)** では、
  `WebViewBuilder` に `.web_context(&shared)` で明示的に共有 `WebContext`
  を渡すことができる (`attributes.context` が使われるのは `incognito` が
  `false` のときだけ)。したがって toolbar と各タブの content webview で
  同じ `WebContext` を共有させることは、**wry 側の制約には引っかからない
  はず** — 未検証だが、コード上ブロックする要素は見当たらない。
- **プライベートモード (`private == true`)** では、wry は
  `.with_incognito(true)` を見ると **`attributes.context` を無視して毎回
  `WebContext::new_ephemeral()` を作る** (wry 自身のドキュメントコメントが
  明言している。`docs/decisions.md` D15 参照)。**つまりプライベートモード
  では、webview ごとの `WebContext` 独立は wry 側の設計であり、VeloX 側の
  呼び出し方を変えても (今の wry のバージョンでは) 解消できない可能性が
  高い。** これは Epic #57 が言う「エンジン (今回はエンジンをラップする
  wry) がブラックボックスで手が出せない」領域に該当しうる。

つまり: **通常モードでの toolbar/タブ間の `WebContext` 共有は VeloX 側で
試せる可能性が高い有望な方向だが、プライベートモードでは同じ手が使えない
かもしれない。** どちらも実際にコードを変更して計測するまでは確定できない
— それが #62 の仕事である。

`WebContext` を共有したとしても、WebKitGTK が内部でその 1 つの
`WebContext` に対して何個の `WebProcess` を実際に使うか (related-view
process pool の挙動) はこの調査では確認していない。「webview の数だけ
`WebContext` を作るのをやめれば `WebProcess`/`NetworkProcess` の重複が
まるごと消える」という保証はなく、「消える可能性が高い、実測すべき仮説」
に留める。

---

## 6. 結論

### 6.1 VeloX 側に削れる余地があるか、エンジン差か

**両方が混在しているが、比重は圧倒的に VeloX 側 (正確には VeloX の wry の
使い方) にある。**

| 要因 | 全体 PSS への寄与 (1 タブ, 約 416 MiB 中) | VeloX 側で手が出せるか |
| --- | --- | --- |
| VeloX の Rust heap (§3) | 32.08 MiB (7.7%)、タブが増えるほど比率低下 | 手が出せるが、効果はほぼゼロ (数 MiB を削っても全体は動かない) |
| WebKitGTK エンジン自体の重さ (§5.3) | **無し、むしろ Chromium より軽い** (単一 webview で 296〜299 MiB) | 該当なし (エンジン差は見つからなかった) |
| toolbar を独立 webview にしている設計 (D3) + webview ごとに独立 `WebContext` (§4.3, §5) | 約 116〜125 MiB (1 タブ時の VeloX 超過分のほぼ全部) + タブが増えるごとに約 92〜122 MiB/タブ | **手が出せる可能性が高い (未検証の仮説、§5.4)。Epic #57 の「エンジンをブラックボックスとして扱う」原則には反しない — WebKit の内部を触るのではなく、wry への webview の作り方を変えるだけ** |

**#59 (T3, D43) とは対照的な結果になった。** #59 は「VeloX 側で手が出せる
はず」と思っていた区間が実測するとエンジン側 (tao/GTK 初期化、WebKitGTK
の webview 生成) だったが、**#61 では逆に「エンジン差だろう」と予想され
ていた PSS の超過分の大半が、実測すると VeloX 自身の実装 (wry の
`WebContext` の使い方) に起因していた。** これは Epic #57 の「実測するまで
分からない」を体現する結果であり、憶測で「エンジン差だから手が出せない」
と結論づけていたら見逃していた。

### 6.2 T2 (Chromium 比 +10% 以内) は達成可能か

**「達成不能」ではない。実測に基づく有望な方向が見つかった、というのが
正直な現状である。** T3 (#59, D43) のときのように「効果ゼロと判明したので
見送る」とは違う結果になった。

- 1 タブ相当では、§5.3 の 3 つの独立測定が「toolbar の 2 個目の webview
  さえ無ければ VeloX は単一 webview の WebKitGTK ベースライン (296〜299
  MiB) に近づき、それは Chromium (318〜328 MiB) より軽い」ことを一貫して
  示している。**もし §5.4 の仮説 (toolbar/content の `WebContext` 共有)
  が実装・計測で確認できれば、1 タブでの T2 (+10% 以内) は達成、さらには
  Chromium を下回る可能性すらある。**
- ただし複数タブでは、タブごとに独立した `WebContext` を作っている問題
  ("+2 プロセス/タブ") も合わせて解消しないと、Chromium 比の差はむしろ
  タブ数に応じて拡大したまま残る (§4.3: 20 タブで VeloX は Chromium の
  約 5.2 倍)。toolbar/content の共有と、タブ間の共有は**別の変更**であり、
  どちらも実装して計測するまで実際の効果は確認できない。
- したがって: **T2 は達成不能と判断する根拠は今回の実測には無い。** 逆に
  「達成できる」と断定する根拠も無い (未実装・未計測の仮説だから)。
  **#62 の最初のタスクとして、toolbar/content 間の `WebContext` 共有を
  実装し、`docs/performance-targets.md` §8 の再現手順で before/after を
  比較することを推奨する** (D48 に記録)。

### 6.3 次の Issue (#62 / #63) への引き継ぎ

1. **最優先の実験**: `src/ui/window.rs` で toolbar と (非プライベート
   モードの) content webview に同じ `WebContext` を渡すよう変更し、
   `compare_browsers.py` と `tab_scaling.py` で before/after を計測する。
   effekt が確認できなければ入れない (Epic #57 のルールそのまま)。
2. その上で、タブ間の `WebContext` 共有 (プールする/しない、何個まで
   共有するか) を検討する。WebKitGTK の related-view process pool の挙動
   ("`WebContext` を共有しても `WebProcess` は結局タブごとに分かれるのか"
   ) は未検証 — ここが次の切り分けポイントになる。
3. プライベートモードでは §5.4 のとおり wry 側の制約で同じ手が使えない
   可能性が高い。プライベートモードでの挙動は別途確認が要る。
4. `velox-bench run --scenario tabs_N` の PSS/RSS メトリクスは、現状の
   既定間隔 (5000ms) では**タブ数に対する増え方を正しく測れていない**
   (§4.1)。`tabs_N` を使い続けるなら `--rss-interval-ms` を短くする
   などの改善が要るが、今回は `tab_scaling.py` で代替したため本 Issue の
   スコープには含めていない。

---

## 7. 測定のばらつき

| 測定 | 試行数 | ばらつき |
| --- | --- | --- |
| `compare_browsers.py` (VeloX PSS, minimal, 1 タブ) | 2 セット×5 試行 (計 10) | セット中央値 419.8/420.3 MiB (差 0.1%)。生データ範囲 415.6〜421.4 MiB (約 1.4%) |
| `compare_browsers.py` (Chromium PSS, minimal, 1 タブ) | 2 セット×5 試行 (計 10) | セット中央値 327.7/326.0 MiB (差 0.5%)。生データ範囲 318.8〜328.0 MiB (約 2.8%) |
| `process_breakdown.py` (VeloX, 1 タブ) | 3 | 415.4〜416.7 MiB (0.3%) |
| `process_breakdown.py` (Chromium, 1 タブ) | 2 | 318.2〜318.6 MiB (0.1%) |
| `heaptrack` peak (VeloX Rust heap) | 6 (異なる条件含む) | **完全に一致 (32.08 MiB)** |
| `tab_scaling.py` (VeloX PSS, 各タブ数) | 3×4 タブ数 | 1 タブ 0.2%、5 タブ 8.6%、10 タブ 0.4%、20 タブ 4.2% |
| `tab_scaling.py` (Chromium PSS, 各タブ数) | 3×4 タブ数 | 1 タブ 0.6%、5 タブ 2.0%、10 タブ 0.7%、20 タブ 0.3% |
| 最小 WebKitGTK アプリ | 2 | 295.6〜299.4 MiB (1.3%) |

**PSS は (`docs/decisions.md` D46 が startup 系メトリクスで報告した最大
28.8%/78.9% のセット間ノイズと比べて) 全般に安定していた** — ほとんどが
0〜3% 程度で、最大でも 8.6% に収まっている。例外は `tab_scaling.py` の
VeloX 5 タブ (8.6%) で、
タブを複数個連続で開く自動操作の完了タイミングのばらつきが原因と考えられる
(タブが開き終わるタイミングと固定の settle 時間の相対関係が試行ごとに
微妙にずれるため)。**この程度のばらつきは §4.3 で報告した「VeloX の増分は
Chromium の約 10.7 倍」という結論を揺るがす大きさではない** — 最小の
VeloX 側の値 (5 タブで 758.0 MiB) を使っても増分は
(758.0-409.6)/4=87.1 MiB/タブで、依然として Chromium (9.7〜10.1
MiB/タブ) の 9 倍近い。

---

## 8. 試みたが実行できなかったこと / スコープ外にしたこと

- **50 タブでの計測**: Issue 本文が時間がかかるなら省略可としている項目。
  1/5/10/20 タブの傾向 (§4) が既に明確な線形〜やや加速する増え方を示して
  いたため、追加の情報価値に対して時間コストが見合わないと判断し省略した。
- **`heaptrack -p <PID>` での `WebKitWebProcess` への直接アタッチ**:
  `docs/profiling.md` 自身が「実行中プロセスへのアタッチは不安定でクラッシュ
  しうる」と警告しており、本 Issue でも試していない。WebKitWebProcess 側の
  詳細なアロケーション内訳が要る場合は別 Issue で扱う。
- **macOS (WKWebView) / Windows (WebView2) での検証**: このコンテナには
  無く、`docs/profiling.md` 同様に未検証。WKWebView/WebView2 が toolbar+
  content で `WebContext`/データストアを独立させる挙動を持つかどうかは
  未確認 — §5.4 の仮説が Linux 固有かどうかも今後の課題。
- **`WebContext` 共有の実測**: §5.4 に記載のとおり、これは仮説であり
  コード変更を要するため本 Issue のスコープ外 (「メモリ削減のコード変更は
  行わない」という Issue の制約による)。#62 に引き継ぐ。
- **WebKitGTK の related-view process pool が 1 つの `WebContext` に対して
  実際に何個の `WebProcess` を使うか**の一般的な調査: 今回の最小 WebKitGTK
  アプリは webview 1 個だけなので、複数 webview を 1 つの `WebContext` に
  ぶら下げた場合の挙動は確認できていない。

---

## 9. `WebContext` 共有の実装・計測結果 (Issue #118)

Issue #61 (上記 §4/§5、`docs/decisions.md` D48) が「有望だが未検証」とした
仮説 — toolbar/タブ間で `WebContext` を共有すれば `WebKitWebProcess`/
`WebKitNetworkProcess` の重複が減るのではないか — を実際に実装し、**同一
セッション内で** before/after を計測した。結論は `docs/decisions.md` D49
に記録した。branch `claude/issue-118-webcontext-sharing`。

### 9.1 実装

`src/ui/window.rs` の `BrowserWindow` に `context: Option<wry::WebContext>`
フィールドを追加した。`config.private == false` のときだけ起動時に
`WebContext::new(None)` を 1 つ生成して `Some` で保持し、toolbar と全タブの
content webview の両方が `WebViewBuilder::new_with_web_context(&mut context)`
(wry 0.56.1 が唯一提供する共有経路 — チェーン可能な `.web_context(...)`
メソッドは存在しない。ソース確認済み: `grep -n "fn web_context" wry-0.56.1/src/lib.rs`
はゼロ件) 経由でこの 1 つの `WebContext` を共有して構築されるようにした。
`config.private == true` のときは `context` を最初から `None` にする
(`WebViewBuilder::new()` を使う、従来どおり) — D15 が確認済みのとおり wry
は `.with_incognito(true)` のとき `attributes.context` を無視して毎回
`WebContext::new_ephemeral()` を作るため、共有 context を渡しても無視される
だけであり、そもそも渡さない設計にした。

### 9.2 変更前後の PSS (`scripts/bench/tab_scaling.py`, 各 3 試行の中央値)

同一セッション内で、変更前 (baseline: このブランチの差分を `git stash` で
外してビルドした状態) → 変更後 (差分を適用してビルドした状態) の順に計測
した (`docs/performance-targets.md` §1 の「異なるセッション間の数値を比較
しない」制約を守るため)。

| タブ数 | before PSS (MiB) | after PSS (MiB) | 変化率 | before プロセス数 | after プロセス数 |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 1  | 419.0  | 408.5  | -2.5%  | 5  | 4  |
| 5  | 834.8  | 790.9  | -5.3%  | 13 | 8  |
| 10 | 1386.4 | 1234.5 | -11.0% | 23 | 13 |
| 20 | 2456.6 | 2322.5 | -5.5%  | 43 | 23 |

各セルは 3 試行の中央値。trial 間のばらつき (min-max スプレッド) は before
0.2〜2.2%、after 0.3〜4.5% — §7 で報告した #61 のばらつき (最大 8.6%) の
範囲内。**同時に測定した対照群の Chromium (このセッションでは一切変更して
いない) は同じ 4 条件で -0.6%〜+1.5% の変動しかなく**、上表の VeloX 側の
変化がこの環境のノイズではなく実装変更由来であることを裏付ける。

再現コマンド:

```sh
xvfb-run -a --server-args="-screen 0 1280x900x24" dbus-run-session -- \
  python3 scripts/bench/tab_scaling.py \
    --velox <before または after のバイナリ> --chromium /opt/pw-browsers/chromium \
    --page minimal.html --tab-counts 1,5,10,20 --trials 3 \
    --output results/tab-scaling-{before,after}.json
```

### 9.3 プロセス数の変化: 何が実際に減ったか

プロセス数はどのタブ数でも 3 trial 全てで完全に一致した (分散ゼロ)。増分の
パターンが根本的に変わった:

- before: タブ 1 個増えるごとに正確に **+2 プロセス** (5→13→23→43。
  toolbar+各タブごとに独立した `WebKitWebProcess`+`WebKitNetworkProcess`
  のペア、#61/D48 の指摘どおり)
- after: タブ 1 個増えるごとに正確に **+1 プロセス** (4→8→13→23。これは
  Chromium の増分パターン (+1/タブ、§4.3) と一致する)

`scripts/profile/process_breakdown.py` で実際のプロセス種別を直接確認した
(`VELOX_AUTOMATION_SCRIPT` で toolbar+5 タブ=webview 6 個を開かせ、
`--pid` で実行中プロセスにスナップショット。`VELOX_DEBUG=1` の
`PageTitleResolved` ログで全 5 タブが実際に読み込み完了したことも確認済み):

| comm | after (webview 6 個) | before 相当 (#61 §5.2 実測) |
| --- | ---: | ---: |
| `WebKitWebProces` | **6** (webview 1 個につき 1 個、変化なし) | 6 |
| `WebKitNetworkPr` | **1** (全 webview で共有) | 6 |
| `velox` (本体) | 1 | 1 |
| 合計 | **8** | 13 |

**`WebContext` 共有は `WebKitNetworkProcess` を完全に 1 個へ統合した
(webview がいくつあっても常に 1 個) が、`WebKitWebProcess` は webview 1 個
につき 1 個のまま、まったく統合されなかった。** これは D48/#61 が明記して
いた未検証の留保 「`NetworkProcess` の重複 (1 タブあたり約 17 MiB) は消えて
も、`WebProcess` (同 155 MiB) は残る可能性がある」がそのとおりに的中した
ことを意味する — WebKitGTK の `WebContext` は `NetworkProcess` の生成単位
ではあるが、`WebProcess` の生成単位ではない (related-view process pool を
明示的に使わない限り、webview ごとに独立)。

再現コマンド (実行中プロセスの直接確認):

```sh
python3 scripts/profile/process_breakdown.py --pid <実行中の velox の PID>
```

### 9.4 プライベートモードでの挙動

`VELOX_PRIVATE=1` で toolbar+3 タブ (webview 4 個) を起動し、同じ
`process_breakdown.py` で確認したところ、**`WebKitWebProcess` 4 個 +
`WebKitNetworkProcess` 4 個** — 完全に 1:1 のまま、共有は一切起きて
いなかった。実装のとおり `context: None` を渡しており (§9.1)、D15 が
指摘した wry 自身の制約 (`.with_incognito(true)` は `attributes.context`
を無視して毎回 `WebContext::new_ephemeral()` を作る) を実装レベルで
そのまま追認する形になった。**プライベートモードはこの変更の影響を一切
受けず、変更前とプロセス構成・PSS 特性ともに同一である。**

### 9.5 データ分離の確認

自作の cookie テストページ (`document.cookie` を読み書きし、結果を
`document.title` に反映するだけの最小 HTML — 使い捨て、リポジトリには
含めていない) を使い、`VELOX_DEBUG=1` の `PageTitleResolved` ログで
確認した。

- **通常タブ同士の Cookie 共有 (意図通りか)**: 通常モードの 2 タブ間で
  Cookie が共有されることを確認した — 1 つ目のタブが
  `veloxmark=set-by-B` という Cookie を設定すると、2 つ目のタブは同じ
  セッション内でその Cookie を読み取れた。**これは `WebContext` を
  共有した意図通りの結果であり、通常タブが同一プロファイルの
  Cookie/storage を共有するのは (実際のブラウザと同じ) 正しい挙動で
  あって分離の破壊ではない。**
- **プライベートモードは通常モードのデータを一切見ない**: 上記のテスト
  で通常モードのタブが Cookie を書き込んだあとに、同じ `XDG_DATA_HOME`/
  `XDG_CACHE_HOME` を指した状態でプライベートモードを起動し同じページを
  開いたところ、2 タブとも Cookie が見えていなかった。プライベート
  モードのデータストアは常にエフェメラル (§9.4) であり、通常モードの
  永続データへ通じる経路自体が存在しない。
- **プライベートモードの各タブは互いにも共有しない**: 上記と同じ
  プライベートセッション内で 2 タブとも Cookie が見えていなかった —
  1 つ目のタブが設定した Cookie を 2 つ目のタブも見ていない。これは
  wry の `.with_incognito(true)` パスが webview ごとに独立した
  `WebContext::new_ephemeral()` を作るという **この変更以前からの**
  既存の挙動であり (§9.1 のとおり、この変更はプライベートパスに一切
  手を入れていない)、今回の変更による新しい制約や規模拡大ではない
  (むしろプライベート性としては保守的な方向であり、緩んでもいない)。
- **通常/プライベートの混在は起きない**: `BrowserWindow::context` は
  `config.private` に基づいて起動時に一度だけ決まり (D14: プライベート
  モードはプロセス全体で固定、タブ単位で切り替わらない)、共有
  `WebContext` はそもそも `config.private == true` の実行では生成すら
  されない。共有 `WebContext` がプライベート webview に渡る経路はコード
  上存在しない。

### 9.6 startup / page load への影響 (`velox-bench gate`)

`cold_startup` シナリオ (`minimal.html`、baseline 10 試行 + candidate
2×10 試行、同一セッション内) を `velox-bench gate` (既定閾値 warn 20% /
fail 60%、D46) で評価した。**総合判定: OK (全指標 OK、Warn/Fail なし)。**

| metric | baseline (中央値) | candidate 1 | candidate 2 | 判定 |
| --- | ---: | --- | --- | --- |
| page_load_ms | 19.50 | 20.35 (+4.4%) | 21.50 (+10.3%) | OK |
| pss_process_count | 5.00 | 4.00 (-20.0%) | 4.00 (-20.0%) | OK |
| pss_total_bytes | 143,933,952 | 140,063,232 (-2.7%) | 133,710,848 (-7.1%) | OK |
| rss_process_count | 5.00 | 4.00 (-20.0%) | 4.00 (-20.0%) | OK |
| rss_total_bytes | 266,754,048 | 234,227,712 (-12.2%) | 212,791,296 (-20.2%) | OK |
| startup_first_load_ms | 312.00 | 285.80 (-8.4%) | 299.75 (-3.9%) | OK |
| startup_rust_setup_done_ms | 135.45 | 135.35 (-0.1%) | 134.30 (-0.8%) | OK |
| startup_toolbar_ready_ms | 288.75 | 289.90 (+0.4%) | 286.25 (-0.9%) | OK |
| startup_window_created_ms | 135.35 | 135.15 (-0.1%) | 134.10 (-0.9%) | OK |

**起動系メトリクスは悪化しておらず (いずれもゲート内)、`pss_process_count`/
`pss_total_bytes` はむしろ改善している。**

### 9.7 T2 (Chromium 比 +10% 以内) は達成したか

**達成していない。** 変更後も Chromium との差は依然として大きい:

| タブ数 | before: VeloX vs Chromium | after: VeloX vs Chromium |
| ---: | ---: | ---: |
| 1  | +48.6%  | +45.5%  |
| 5  | +160.4% | +146.6% |
| 10 | +281.6% | +234.7% |
| 20 | +428.1% | +402.2% |

1 タブあたりの増分 (1→20 タブの平均) は before 約 107.2 MiB/タブ → after
約 100.7 MiB/タブへと、約 6% だけ縮んだ (§9.3 のとおり `NetworkProcess`
の重複だけが消え、支配的な `WebProcess` (§2.1 で全体の 74.4% を占めて
いた) はタブごとに増え続けるため)。

### 9.8 結論: 変更を残すか、revert するか

**残す。** 理由:

1. **効果はゼロではなく、実測で確認できる程度に有意である。** 1/5/10/20
   タブすべてで PSS が -2.5%〜-11.0% 減少し、プロセス数は 20%〜38%
   減少した。対照群の Chromium は同一測定で -0.6%〜+1.5% にとどまって
   おり、VeloX 側の変化がこの環境のノイズ (§7 で報告した 0.1〜8.6% の
   trial 間ばらつき) の範囲を超えていることを裏付ける。
2. **プロセス数の減少パターン (+2/タブ→+1/タブ) は決定的 (trial 間で
   分散ゼロ) であり、根本原因 (`NetworkProcess` の統合) がソースレベルで
   説明できる。** 偶然の測定ノイズでは説明できない。
3. **startup/page load を悪化させていない** (§9.6、`velox-bench gate`
   総合判定 OK)。
4. **データ分離を壊していない** (§9.5) — 通常タブ間の共有は意図通り、
   プライベートモードは変更の影響を受けていない。
5. **統合テスト・単体テストが全て通る** (`cargo test`、
   `xvfb-run ... dbus-run-session -- cargo test` とも 0 failed)。

一方で正直に記録すべき限界:

- **T2 (Chromium 比 +10% 以内) は達成できていない** (§9.7)。支配的な
  `WebProcess` の重複が残っているため、Chromium 比の超過分の大半は
  未解決のまま。
- **タブが増えるほど Chromium との差は絶対値でも相対値でも拡大し続ける**
  (§9.7 の表)。今回の変更はこの傾向そのものは変えていない — 増分の
  傾きをわずかに緩めた (107.2→100.7 MiB/タブ) だけである。
- したがって **この変更は「メモリ超過分の主要因を解決した」わけではなく、
  「効果が実測で確認できる部分的な改善」という位置付けが正確である。**
  #59/D43 (効果ゼロと判明し見送った) とも、当初期待された「T2 達成」とも
  異なる、第三の結果になった。

### 9.9 次に残された課題

`WebProcess` を webview 間で共有するには、wry 0.56.1 が
`WebViewBuilderExtUnix::with_related_view(webview: webkit2gtk::WebView)`
という別の (`WebContext` 共有とは独立した) API を公開していることを
ソース調査で確認した (「Creates a new webview sharing the same web
process with the provided webview.」— `wry-0.56.1/src/lib.rs`)。ただし
この API は `webkit2gtk::WebView` という wry の外側の型を要求しており、
VeloX が現状 `wry::WebView` しか保持していない設計 (D20 の「`browser::`
は `wry`/`gtk` 型を一切知らない」という層分離とも関わる) を崩さずに
使えるかは未検証。WebKitGTK の related-view process pool の一般的な挙動
(#61 §5.4 が未検証としていた点) も含め、次の Issue で検証することを
推奨する。

---

## 10. `WebKitWebProcess` 共有 (`with_related_view`) の実装・計測結果 (Issue #124)

§9.9 / `docs/decisions.md` D49 の Revisit condition が「次の Issue」として
推奨していた、wry 0.56.1 の `WebViewBuilderExtUnix::with_related_view` に
よる **タブ間の `WebKitWebProcess` 共有** を実装し、**同一セッション内で**
before/after を計測した。結論は `docs/decisions.md` D54 に記録した。
branch `claude/next-phase-issue-check-8xyj9u`。

測定環境は §1 と同一 (同じコンテナ、WebKitGTK 2.52.6、Chromium 141、GPU
なし)。before は `main` (`eeb609c`、D49 適用済み) のバイナリ、after は本
Issue の変更を適用したバイナリで、どちらも同じセッションで
`cargo build --release` した。

### 10.1 実装 (3 段階で確定した)

**前提の確認**: `webkit2gtk::WebView` という wry の外側の型は、wry 自身の
`WebViewExtUnix::webview(&self) -> webkit2gtk::WebView` アクセサで既存の
`wry::WebView` から取り出せるため、**新しい依存クレートは不要**で、
`browser::` は引き続き `wry`/`gtk` 型を一切知らない (D20 の層分離は維持)。
また wry は related view が指定されていると builder に `.web_context()` を
呼ばず (`wry-0.56.1/src/webkitgtk/mod.rs` の `create_webview`)、WebKitGTK が
related view の `WebContext` を継承するため、D49 の共有 `WebContext` と
整合する。

1. **案 A — 全タブを 1 つの `WebProcess` に乗せる**: `open_tab` で、生存中の
   content webview を 1 つ選んで related view として渡す。toolbar は対象
   外 (信頼境界: 特権 UI とページコンテンツを同一レンダラプロセスに
   置かない、D18/D23)。プライベートモードも対象外 (D15: wry の
   `.with_incognito(true)` は webview ごとに ephemeral context を作り、
   related view を指定すると WebKitGTK が related view 側の context を
   使ってしまうため、「private が何を分離するか」が変わってしまう)。
   → メモリは最大 -33% だが **burst オープン時のページロードが直列化**
   した (§10.4)。
2. **案 B — 1 プロセスあたりのタブ数に上限 (4) を設ける**: `ContentTab` に
   `process_group` を持たせ、空きのある最も埋まったグループに相乗り、
   全部埋まっていれば新しいグループ (= 新しい `WebProcess`)。
   → 上限 4 でも 4 ページの同時ロードは直列化され、`tab_switch` の
   `page_load_ms` は依然 +380% (§10.4)。
3. **案 C (採用) — B に加えて「読み込み中のタブがいるグループには相乗り
   しない」**: `app.rs` が `Tabs` の `is_loading` を probe クロージャで
   `open_tab`/`resume_tab` に渡し、グループ選択 (`pick_process_group`、
   純粋関数・単体テスト済み) がそのグループを除外する。burst オープン
   (前のタブがまだ読み込み中に次を開く) は従来どおり別プロセスに散って
   並列にロードされ、定常状態 (前のタブの読み込みが済んでから次を開く)
   だけ共有される。

### 10.2 変更前後の PSS (`scripts/bench/tab_scaling.py`, 各 3 試行の中央値)

`minimal.html`、`--settle-per-open-ms 300` (既定)。Chromium は対照群として
before と案 A の計測で同時に測定した (案 C の計測では時間短縮のため VeloX
のみ。Chromium は同セッション 2 回の計測で ±0.4% 以内だった)。

| タブ数 | before PSS (MiB) | 案 A: 全共有 (MiB) | **案 C: 採用 (MiB)** | 案 C の before 比 | Chromium (MiB) | 案 C の Chromium 比 (before 比) |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1  | 409.2  | 409.6  | **409.0**  | -0.1%  | 281.2 | +45.4% (+45.5%) |
| 5  | 787.9  | 625.1  | **655.7**  | -16.8% | 320.4 | +104.6% (+145.9%) |
| 10 | 1277.7 | 907.2  | **989.5**  | -22.6% | 367.3 | +169.4% (+247.9%) |
| 20 | 2165.8 | 1441.8 | **1612.3** | -25.6% | 465.8 | +246.2% (+365.0%) |

trial 間のばらつき (min–max) は before 409.0–422.7 / 769.2–791.0 /
1208.3–1278.8 / 2157.7–2199.6 MiB、案 C 408.2–409.1 / 640.3–657.1 /
973.1–990.6 / 1559.8–1635.6 MiB で、**5 タブ以上では before の最小値より
案 C の最大値のほうが小さい** (分布が重ならない)。

**1 タブあたりの増分 (1→20 タブの平均)**: before 92.4 MiB/タブ → 案 A
54.3 MiB/タブ → **案 C 63.3 MiB/タブ** (Chromium 9.7 MiB/タブ)。

**プロセス数 (中央値、3 trial とも一致・分散ゼロ)**: 1/5/10/20 タブで
before 4/8/13/23 → 案 A 4/4/4/4 → 案 C 4/5/6/8。案 C は
「`velox` + `NetworkProcess` + toolbar の `WebProcess` + ⌈タブ数/4⌉ 個の
content `WebProcess`」の計算どおり。

### 10.3 プロセス内訳の直接確認 (`scripts/profile/process_breakdown.py`)

`VELOX_AUTOMATION_SCRIPT` で toolbar+5 タブ (content webview 5 個) を開かせ、
settle 6 秒後にスナップショット (案 A):

| comm | before (§9.3、webview 6 個) | 案 A |
| --- | ---: | ---: |
| `WebKitWebProces` | 6 | **2** (toolbar 1 + 全 content タブ共有 1) |
| `WebKitNetworkPr` | 1 | 1 |
| `velox` | 1 | 1 |
| 合計 PSS | 約 791 MiB (§9.2 の 5 タブ) | **518.8 MiB** |

案 C では同じスクリプトで `WebKitWebProces` が **3** (toolbar 1 + content
グループ 2: 最初の `open` は初期タブの読み込み中に実行されるため別
グループになり、以降のタブがそこに相乗りする)。

追加で確認したこと (案 C):

- **クロスオリジン遷移で分裂しない**: `file://` のタブを
  `http://127.0.0.1:8731/text.html` へ `navigate` し、さらに
  `http://127.0.0.1:8731/dom_heavy.html` を `open` しても `WebProcess` は
  増えない (WebKitGTK 2.52.6 のこの構成では process swap on navigation は
  起きなかった)。
- **タブを全部閉じてから開き直しても共有が続く**: 初期タブを含む 3 タブを
  `close 0` ×3 で閉じ、新たに 2 タブ開いた状態で `WebProcess` は 2 個
  (toolbar + 共有 1)。生存 webview が無いときは新グループを作り、次の
  タブがそこに相乗りする、という設計どおり。
- **プライベートモードは無変更**: `VELOX_PRIVATE=1` で toolbar+4 タブ
  (webview 5 個) → `WebKitWebProcess` 5 個 + `WebKitNetworkProcess` 5 個。
  §9.4 と同じく完全に 1:1 のままで、本変更の影響を受けていない。

### 10.4 なぜ案 A/B を採用しなかったか: burst オープン時のページロード直列化

`velox-bench gate` (各 8 試行、baseline = before、candidate = after ×2、
既定閾値 warn 20% / fail 60%、D46) を `tab_switch` シナリオ (タブ 4 個を
**待ち時間なしで連続オープン**してから切替を繰り返す) で評価した:

| 案 | `page_load_ms` baseline → candidate 1 / 2 | 判定 |
| --- | --- | --- |
| A: 全共有 | 18.4 → 135.9 (+638.6%) / 133.1 (+623.4%) | **FAIL** |
| B: 上限 4 のみ | 18.4 → 88.5 (+381.0%) / 98.3 (+434.5%) | **FAIL** |
| **C: 上限 4 + 読み込み中を避ける** | 18.4 → 21.8 (+18.5%) / 21.1 (+14.7%) | **OK** |

案 A の min 値でさえ 39.9ms (before の min は 10.3ms) で、1 つの
`WebProcess` の単一メインスレッドに 5 ページの読み込みが乗ると**直列化**
される (4 コアの環境で 5 プロセスなら並列に進む) ことが原因。
Epic #57 のルール 4 (「メモリを過剰に解放して復帰時のページロードが遅く
なる場合は改善とみなさない」) に照らして A/B は採用できず、C に至った。
C の残り +15〜18% は多数決・絶対差フロアの範囲内 (D46) で、`tab_create`
シナリオ (300ms 間隔で逐次オープン) では逆に **`page_load_ms` が
-31.6〜-34.4%、`tab_create_ms` が -11.9%** 改善している (既存プロセスに
ページを追加するほうが新プロセスを起こすより速い)。

### 10.5 startup / page load への影響 (`velox-bench gate`、案 C)

| シナリオ | 総合判定 | 主な指標 (baseline → candidate 1 / 2) |
| --- | --- | --- |
| `cold_startup` | **OK** | `startup_first_load_ms` 280.8 → 290.8 (+3.5%) / 285.4 (+1.6%)、`startup_toolbar_ready_ms` 281.4 → 289.4 (+2.8%) / 281.0 (-0.1%)、`page_load_ms` 16.2 → 20.1 (+24.4%) / 18.2 (+12.3%) |
| `tab_create` | **OK** | `page_load_ms` 14.4 → 9.4 (-34.4%) / 9.8 (-31.6%)、`tab_create_ms` 2.95 → 2.6 (-11.9%) / 2.6 (-11.9%)、`startup_first_load_ms` 267.1 → 302.5 (+13.3%) / 283.3 (+6.1%) |
| `tab_switch` | **OK** | `page_load_ms` 18.4 → 21.8 (+18.5%) / 21.1 (+14.7%)、`tab_switch_ms` 0.5 → 0.5 / 0.6、`tab_create_ms` 6.1 → 5.3 (-12.3%) / 6.8 (+11.5%) |

1 タブ時 (cold_startup) は共有相手がいないため、構成・PSS ともに before と
同一 (`pss_process_count` 4 → 4)。`page_load_ms` の +12〜24% は絶対値で
2〜4ms、D46 の絶対差フロア未満で、trial 間ばらつき (§7) の範囲。

### 10.6 T2 (Chromium 比 +10% 以内) は達成したか

**達成していない。** ただし差は大幅に縮んだ: 20 タブで +365.0% → +246.2%、
10 タブで +247.9% → +169.4%。1 タブ (toolbar + content 1 個) は共有相手が
無いため +45% のまま — ここは D48 §5 の「toolbar 用 2 個目の webview 1 個
分」がそのまま残っている。

**残っている超過の内訳 (次の切り分けポイント)**: 案 A (全タブ 1 プロセス)
でも 1 タブあたり **54.3 MiB** 増える。プロセスの固定費 (before との差、
約 38 MiB/プロセス) は消えたのに、同一プロセス内のページ 1 枚あたり
Chromium の 5.6 倍のメモリを使っている。候補は (1) GPU 無し環境での
ページごとのソフトウェアレンダリング用バッキングストア (非表示タブ分も
保持されている可能性 — Chromium は非表示タブの描画リソースを破棄する)、
(2) 非表示タブの JS heap / DOM。**どちらも未検証**で、#63 (Adaptive Tab
Suspension: 非表示タブの webview を落とす) がこの残りに効く可能性が高い。

### 10.7 再現手順

```sh
S=/path/to/scratch
cp target/release/velox $S/velox-after        # after: 本 Issue 適用後
git stash && cargo build --release && cp target/release/velox $S/velox-before && git stash pop

xvfb-run -a --server-args="-screen 0 1280x900x24" dbus-run-session -- \
  python3 scripts/bench/tab_scaling.py --velox $S/velox-before \
    --chromium /opt/pw-browsers/chromium --page minimal.html \
    --tab-counts 1,5,10,20 --trials 3 --output $S/tab-scaling-before.json
# after も同様 (--velox $S/velox-after)

(cd scripts/bench/pages && python3 -m http.server 8731 &)
for sc in cold_startup tab_create tab_switch; do
  xvfb-run -a --server-args="-screen 0 1280x900x24" dbus-run-session -- \
    target/release/velox-bench run --scenario $sc --trials 8 \
      --velox-bin $S/velox-before --url http://127.0.0.1:8731/minimal.html \
      --output $S/$sc-baseline.json
  # candidate-1 / candidate-2 も同様 (--velox-bin $S/velox-after)
  target/release/velox-bench gate --baseline $S/$sc-baseline.json \
    --candidate $S/$sc-candidate-1.json --candidate $S/$sc-candidate-2.json
done

# プロセス内訳 (toolbar+5 タブ)
printf 'open file://%s\nwait 300\n' $PWD/scripts/bench/pages/minimal.html > $S/tabs5.txt  # ×5
printf 'wait 8000\nquit\n' >> $S/tabs5.txt
VELOX_AUTOMATION_SCRIPT=$S/tabs5.txt xvfb-run -a --server-args="-screen 0 1280x900x24" \
  dbus-run-session -- python3 scripts/profile/process_breakdown.py --settle-secs 6 \
    --launch -- $S/velox-after --homepage file://$PWD/scripts/bench/pages/minimal.html
```
