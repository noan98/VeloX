# VeloX プロファイリング手順

Issue #70 (Epic #57 傘下、#58 依存)。**この文書は「性能問題を見つけたあと、
原因まで掘る手順」を扱う。** どこが遅い/重いかを検出するのは
[docs/benchmarking.md](benchmarking.md) (VeloX 自身の内部計測) と
[docs/performance-targets.md](performance-targets.md) (競合ブラウザとの比較)
の役目であり、この文書はその続き — 「遅いこと/重いことは分かった。次にどこの
コードが原因かをどう特定するか」に答える。

```
ベンチマークで「遅い/重い」を検出   docs/benchmarking.md
                                     scripts/bench/compare_browsers.py
              │
              ▼
プロファイリングで「どこが」を特定   この文書
                                     scripts/profile/
              │
              ▼
再現条件・環境・プロファイルを添えて Issue を起票   §7
```

この Issue の時点でこのコンテナには **GPU が無い**
(`docs/performance-targets.md` §1 に明記された環境の重大な制約)。メモリと
描画に関わる数値が実機と乖離することは既に分かっている。**この文書はそれに
加えて「実機で何を測り直すべきか」のチェックリストも引き受ける** (§8)。

以下の手順は**すべてこのコンテナで実際に実行して確認した** (§9「この環境での
検証状況」に生の実行結果を記録している)。未検証の部分は本文中にその旨を
明記する。

---

## 0. 前提: debug symbols 付きビルド

`cargo build --release` の成果物は `Cargo.toml` の `[profile.release]` が
`strip = true` を指定しているため、シンボルが一切残っていない
(`perf report` に関数名が出ず、`0x7f1b7cd94133` のような生アドレスしか
出ない)。プロファイリングには専用のビルドプロファイルを使う:

```sh
cargo build --profile profiling
```

`target/profiling/velox` が生成される。`Cargo.toml` の `[profile.profiling]`
は `[profile.release]` を継承しつつ (`inherits = "release"` — 最適化レベル・
`lto` は release と同じなので、プロファイル対象のコードパスは release と
同じ挙動になる) `debug = true` / `strip = false` だけを上書きしている。
**`target/release/` の成果物やビルド設定には一切影響しない**
(別プロファイル = 別ディレクトリ `target/profiling/` に出力されるため)。
実際に確認したサイズ差:

| ビルド | パス | サイズ | 備考 |
| --- | --- | ---: | --- |
| `cargo build --release` | `target/release/velox` | 1,984,704 B | strip 済み |
| `cargo build --profile profiling` | `target/profiling/velox` | 28,727,528 B | debug_info 付き、not stripped |

サイズが増えデバッグ情報が付くのは `target/profiling/` 側だけで、
`cargo build --release` を再実行しても `target/release/velox` は変わらない
(通常の `cargo build --release` の成果物や起動性能を悪化させない、という
Issue の制約はこの分離で満たしている)。判断根拠は
[docs/decisions.md](decisions.md) D45 を参照。

以下の手順の `<velox-profiling>` は `target/profiling/velox` (または
Windows なら `target\profiling\velox.exe`) を指す。

---

## 1. CPU プロファイリング (Linux, `perf`)

### 1.1 `perf` のインストールとバージョン不整合への対処

```sh
sudo apt install -y linux-tools-common linux-tools-generic
```

**このコンテナのカーネル (`6.18.44-fc-v22` のようなカスタムビルド) には
ちょうど一致する `linux-tools-<version>` パッケージが存在しない。**
`/usr/bin/perf` はカーネルバージョン別のラッパースクリプトで、一致する
パッケージが無いと次の警告を出して **exit code 2 で即座に失敗する**
(このコンテナで実際に発生した):

```
WARNING: perf not found for kernel 6.18.44-fc

  You may need to install the following packages for this specific kernel:
    linux-tools-6.18.44-fc-v22
    linux-cloud-tools-6.18.44-fc-v22
```

回避策: `linux-tools-generic` が入れる、動いているカーネルとは異なる
バージョンの `perf` バイナリを直接呼ぶ。

```sh
ls /usr/lib/linux-tools/*/perf
# 例: /usr/lib/linux-tools/6.8.0-138-generic/perf
```

`perf_event` の ABI はこの程度のバージョン差なら通常互換で、**このコンテナ
では実際にこのバイナリで CPU サンプリングと Rust シンボル解決の両方が動作
した** (§9)。`scripts/profile/run_perf.py` はこの探索 (`/usr/bin/perf` →
`/usr/lib/linux-tools/*/perf` の順) を自動でやる。

### 1.2 ハードウェアイベントが使えない (このコンテナ固有)

このコンテナは仮想化環境で、ハードウェア PMU (Performance Monitoring Unit)
がゲストに渡されていない。実際に確認した症状:

```sh
$ perf stat -e cycles,instructions -- sleep 0.2
 Performance counter stats for 'sleep 0.2':
   <not supported>      cycles
   <not supported>      instructions
```

`cycles` / `instructions` のようなハードウェアイベントは使えない。
代わりに **ソフトウェアイベント `cpu-clock`** (実際に CPU 上で実行されている
時間を一定間隔でサンプリングする、PMU 不要) を使う。**実機や PMU の使える
環境では `-e cycles` の方が (割り込みオーバーヘッドが小さく、命令ミックスの
影響を受けにくい分) 高精度なので、そちらを優先すること。**
`scripts/profile/run_perf.py --event cycles` で切り替えられる。

### 1.3 記録する

手動での最小手順 (`<velox-profiling>` は debug symbols 付きビルド):

```sh
Xvfb :99 -screen 0 1280x900x24 -nolisten tcp &
export DISPLAY=:99
PERF=/usr/lib/linux-tools/6.8.0-138-generic/perf   # 実際のパスに合わせる

(cd scripts/bench/pages && python3 -m http.server 8731 &)

timeout -s INT 8 "$PERF" record -g -e cpu-clock -F 999 \
  -o /tmp/velox-cpu.data \
  -- ./target/profiling/velox --homepage http://127.0.0.1:8731/minimal.html
```

`timeout -s INT` で `perf record` に SIGINT を送るのが要点 — `perf` は
SIGINT を「Ctrl-C で普通に記録を止める」経路として扱い、記録済みデータを
壊さずに終了し、**自分が起動した子プロセス (VeloX 本体) も道連れに終了させる**。

`--` の後のコマンドを `perf record` が起動すると、既定 (`--inherit` あり) で
**そのプロセスが fork/exec する子プロセスも一緒にトレースする** —
実際に確認したところ、記録データには VeloX 本体だけでなく
`WebKitWebProcess` / `WebKitNetworkProcess` / `SkiaGPUWorker` などのサンプル
も含まれていた。`perf report --sort comm` でコマンド別の内訳が見られる:

```
    32.87%    32.87%  WebKitWebProces
    20.75%    20.75%  SkiaGPUWorker
    19.74%    19.74%  velox
    11.73%    11.73%  WebKitNetworkPr
     9.56%     9.56%  eadedCompositor
```

**VeloX 自身のコードだけを見たいときは `--comms=velox` で絞り込む**:

```sh
"$PERF" report -i /tmp/velox-cpu.data --stdio --comms=velox
```

これで `velox::app::run` や `tao::platform_impl::...` のような Rust の
シンボル名まで解決できることを確認済み (§9)。これは
`docs/decisions.md` D43 が `window_created → toolbar_ready` の内訳を
実測したときと同じ絞り込み方であり、実際に D43 の結論 (`tao`/GTK の
イベントループ初期化が支配的) と整合する結果が `perf report --comms=velox`
でも再現した。

### 1.4 ラッパースクリプトで一括実行する

```sh
python3 scripts/profile/run_perf.py \
  --duration-secs 8 \
  --output-prefix results/profile/cpu-startup-$(date +%Y%m%d)-$(git rev-parse --short HEAD) \
  -- ./target/profiling/velox --homepage http://127.0.0.1:8731/minimal.html
```

このスクリプトは以下を自動でやる (§1.1〜§1.3 の手作業をまとめたもの):

- `DISPLAY` が未設定なら自分で `Xvfb` を起動する (`--display` で既存の
  `DISPLAY` を明示指定することもできる)。
- `perf` バイナリを §1.1 の手順で探す (`--perf-bin` で明示指定も可)。
- `perf record -g -e cpu-clock -F 999` を `--duration-secs` 秒だけ実行し、
  SIGINT で正常終了させる。
- `perf script` でシンボル解決したテキストに変換 (`<prefix>.script`)。
- `scripts/profile/flamegraph.py` に通して折り畳みスタック
  (`<prefix>.folded`、`stackcollapse-perf.pl` 互換形式) と SVG フレーム
  グラフ (`<prefix>.svg`) を生成する。

出力ファイル名に日付と git commit を含めているのは、後から見て「どのコードの
プロファイルか」が分かるようにするため (§7 参照、`docs/benchmarking.md` の
baseline 保存方針と同じ考え方)。

### 1.5 フレームグラフを見る

`<prefix>.svg` を任意のブラウザで開く。矩形の幅がサンプル数 (≒ その関数に
費やされた時間) に比例する。ホバーで完全なスタックとサンプル数/割合が
`<title>` ツールチップとして出る (SVG 標準機能なので追加の JS ライブラリは
不要)。色分け:

- **青系** — シンボルが `velox::` で始まる、または `target/profiling/velox`
  バイナリ由来 (= **VeloX 自身のコード**)
- **緑系** — `WebKit`/`JavaScriptCore`/`WTF::` を含む (= **WebKitGTK/JSC 側**)
- **橙系** — それ以外 (`glib`/`gtk`/`libc` など、エンジンでも VeloX 自身でも
  ない下位レイヤ)

Epic #57 の「WebView をブラックボックスとして扱う」原則のもと、**最適化の
対象になり得るのは基本的に青系の矩形だけ**であることが多い (D43 のように、
橙系/緑系が支配的だと分かった場合はそこがボトルネックでも VeloX 側からは
手が出せない、という結論になりうる)。

なぜ自前の SVG 生成スクリプトなのかは
[scripts/profile/flamegraph.py](../scripts/profile/flamegraph.py) の
docstring を参照 — 要約すると、定番の `flamegraph.pl` はこの環境に
インストールできない (外部ネットワーク遮断) ため、Python 標準ライブラリだけ
で同等の役割を果たす最小実装を用意した。

### 1.6 CPU プロファイリング: macOS / Windows (未検証)

**このコンテナには macOS/Windows 環境が無いため、以下は実行して確認できて
いない。** 標準的な手順として記録するに留める。

- **macOS**: Xcode 付属の Instruments (`Time Profiler` テンプレート) で
  `velox` プロセスにアタッチする。CLI からは `xcrun xctrace record
  --template 'Time Profiler' --launch -- ./target/profiling/velox
  --homepage ...` で記録できるはず (Xcode Command Line Tools が必要)。
  WKWebView のレンダラ/ネットワークプロセスは VeloX 本体とは別プロセスとして
  Instruments 上でも別行に分かれて見えるはずなので、Linux の
  `perf --comms=velox` と同じ考え方で VeloX 自身のプロセスの行だけを見ればよい。
- **Windows**: Windows Performance Recorder (WPR) でシステム全体を記録し、
  Windows Performance Analyzer (WPA) で `velox.exe` のプロセスだけに絞って
  CPU 使用量スタックを見る。WebView2 のレンダラは `msedgewebview2.exe` という
  別プロセス名で表示されるはずなので、同様に `velox.exe` の行だけを見る。
  Visual Studio の「パフォーマンス プロファイラー」(CPU 使用率ツール) でも
  同様のことができるはず。

いずれも「VeloX 本体プロセスと WebView のレンダラ/ネットワークプロセスは
OS プロセスとして分離している」という、この文書全体を貫く前提
(§2 のメモリ分離もこれと同じ理由による) が成り立つので、**プロファイラで
見るべきは常に「VeloX 本体の実行ファイル名のプロセスだけ」**という指針は
OS が変わっても同じはずである。実機での検証は §8 のチェックリストに含める。

---

## 2. メモリプロファイリング (Rust heap)

### 2.1 VeloX 自身の Rust heap と WebKitGTK 側を切り分ける方法

**これが Issue #61 の受け入れ条件 (「Rust 側と WebView/renderer 側を可能な
範囲で分離して分析」) に対するこの Issue の回答である。**

`heaptrack <コマンド>` は `LD_PRELOAD` でメモリ確保関数をフックし、
**`heaptrack` が直接起動したプロセス 1 つのプロファイルだけを 1 ファイルに
書き出す。** WebKitGTK が `fork`+`exec` する `WebKitWebProcess` /
`WebKitNetworkProcess` 等の子プロセスは、実際にこのコンテナで確認したところ
**記録されなかった** (子プロセス用の `.gz` ファイルが増えない — `heaptrack
-o /tmp/x ./target/profiling/velox ...` を実行しても `/tmp/x.gz` が 1 個だけ
生成され、`WebKitWebProcess` 用のファイルは生成されなかった)。WebKitGTK が
子プロセスを起動する際に環境をサニタイズしており、`LD_PRELOAD` が
子プロセスに引き継がれていないと考えられる。

**つまり、何も特別なオプションを付けずに `heaptrack ./target/profiling/velox`
を実行するだけで、得られるプロファイルは自然に「VeloX 本体プロセスの Rust
heap だけ」になる。** これは D42/`docs/performance-targets.md` §3.1 が扱う
「RSS/PSS はプロセスツリー全体を合算する」話とは**別の軸**であることに注意:
RSS/PSS は「メモリ使用量の全体像 (WebView 込み)」を見るための指標
(§3 参照)、`heaptrack` はその中で「VeloX 自身の Rust コードがどこで malloc
しているか」を掘るための指標。両方が必要になる場面が多い。

WebKitGTK 側の heap も掘りたい場合は `heaptrack -p <WebKitWebProcess の PID>`
でアタッチする方法があるが、`heaptrack --help` 自身が警告するとおり
**実行中プロセスへのアタッチは不安定でクラッシュしうる**。今回は試していない
(検証はしていない — 必要になったら別 Issue で扱う)。

### 2.2 `heaptrack` を使う

```sh
sudo apt install -y heaptrack   # このコンテナでは apt でそのまま入った
```

```sh
python3 scripts/profile/run_heaptrack.py \
  --duration-secs 15 \
  --output results/profile/heap-startup-$(date +%Y%m%d)-$(git rev-parse --short HEAD) \
  -- ./target/profiling/velox --homepage http://127.0.0.1:8731/minimal.html
```

`--duration-secs` 経過すると VeloX に SIGTERM を送って終了させる (heaptrack
はプロセス終了時に記録をフラッシュする)。集計を見る:

```sh
heaptrack_print results/profile/heap-startup-....gz | less
```

「MOST CALLS TO ALLOCATION FUNCTIONS」のセクションにコールスタック付きで
アロケーション回数の多い箇所が並ぶ。実際に確認した例 (`minimal.html` 起動
直後、`--duration-secs 6`):

```
99748 calls to allocation functions with 346.34K peak consumption from g_malloc
  ...
    gio::auto::application::ApplicationExt::register::hb51c2b7bdf06e9b4
      at .../gio-0.18.4/src/auto/application.rs:294
    tao::platform_impl::platform::event_loop::EventLoop$LT$T$GT$::new_gtk::...
      at .../tao-0.37.0/src/platform_impl/linux/event_loop.rs:229
    velox::app::run::h0f70876477fa4105
      at /home/user/VeloX-issue-70/src/app.rs:171
```

依存クレート (`tao`/`gio`) 経由のコールスタックでもファイル名・行番号まで
解決できている (debug symbols 付きビルドかつ `cargo` のレジストリソースが
ローカルに残っているため)。

**`heaptrack_print` の "leaked allocations" 件数を実際のメモリリーク件数だと
早合点しないこと。** SIGTERM で強制終了させた場合、その時点でまだ生きている
(=正常に使われ続けている) メモリもすべて「leaked」として数えられる。上記の
6 秒間の記録でも 30,101 件の "leaked allocations" が出たが、これはプロセスを
正常終了させずに打ち切ったからそう見えるだけで、実際のリークとは限らない。
**本当のリークを疑うときは、同じ操作 (例: タブを開いて閉じる) を複数回繰り返し、
繰り返すたびに heap 使用量が単調に増えていくかどうかを見る** — 1 回打ち切った
時点の絶対件数ではなく、繰り返しに対する傾向を見ること (§2.4 のタブ
ライフサイクル/長時間セッションの節も参照)。

`heaptrack-gui` はこのコンテナには入れていない (`heaptrack_print` のテキスト
出力のみで検証した)。GUI がある環境では `heaptrack --analyze <file>.gz` で
開くとフレームグラフ表示や差分表示ができる。

### 2.3 `heaptrack` が使えないとき: `valgrind --tool=massif` で代替する

`heaptrack` が `apt` で入らない環境向けの代替手段。`valgrind` はこのコンテナ
に最初から入っていた (`valgrind-3.22.0`)。

```sh
Xvfb :99 -screen 0 1280x900x24 -nolisten tcp &
export DISPLAY=:99

timeout 40 valgrind --tool=massif \
  --massif-out-file=/tmp/massif.out.velox \
  --pages-as-heap=no \
  ./target/profiling/velox --homepage http://127.0.0.1:8731/minimal.html &
VG_PID=$!
sleep 25
pkill -f "target/profiling/velox"
wait $VG_PID
```

(SIGTERM で終了させると Valgrind がその時点のスナップショットを保存する。
`--pages-as-heap=no` は `malloc`/`free` ベースの heap だけを見るモード —
既定のこの設定でよい。)

```sh
ms_print /tmp/massif.out.velox | less
```

時間軸に沿った heap サイズのグラフ (ASCII アート) と、ピーク時のコール
スタック内訳が出る。実際に確認済み: 96 スナップショット、ピーク約 31MB、
`velox::app::run` 経由のコールスタックまで解決できた。**`valgrind` は
`heaptrack` よりオーバーヘッドが大きく実行がかなり遅くなる**
(起動だけで数十秒かかることがある) ので、起動シーケンスを丸ごと追うより、
特定の操作の前後で heap サイズを比べる用途に向く。massif も **`heaptrack`
と同じ理由でプロセス境界による分離が効く** (`valgrind` は対象プロセスの
下で直接動くエミュレータであり、`fork`+`exec` された子プロセスは既定では
別の `valgrind` インスタンスの対象にはならない)。

### 2.4 タブライフサイクル / 長時間セッションのプロファイリング

- **タブライフサイクル** (タブを開く/切り替える/閉じるときのメモリ挙動):
  `scripts/profile/pss_sampler.py` (§3 参照) で高頻度 (`--interval-ms 200`
  程度) にサンプリングしながら、手動でタブを開閉する。`heaptrack`/`massif`
  はこの用途には向かない (対象がタブ操作を起動するもの — VeloX 本体 — で
  はなく、タブ操作そのものだから)。CPU 側でタブ生成/切り替えのコストを
  見たいなら、`docs/architecture.md`「Performance extension points」の
  `tab_create`/`tab_switch` イベント (`velox-bench aggregate` で集計、
  `docs/benchmarking.md` 参照) を使う方が直接的 — こちらは VeloX 自身が
  計測した正確な区間なので、`perf`/`heaptrack` で外側から探るより先に使う
  べき手段である。
- **長時間セッション**: `pss_sampler.py --duration-secs 0` (0 以下を渡すと
  対象プロセスが終了するまで無期限にサンプルし続ける) で長時間放置しながら
  CSV を取り、`pss_bytes` が単調に増え続けていないかを見る。`heaptrack` を
  長時間張り付けたままにする方法もあるが、記録ファイルが際限なく肥大化する
  ため (§2.2 の "leaked allocations" の注意と合わせて)、**傾向を見るのは
  軽量な `pss_sampler.py`、疑わしい箇所が絞れてから `heaptrack` で詳細を掘る**
  という順番を推奨する。

### 2.5 メモリプロファイリング: macOS / Windows (未検証)

**このコンテナには macOS/Windows 環境が無いため、以下は実行して確認できて
いない。**

- **macOS**: Instruments の `Allocations` / `Leaks` テンプレートで `velox`
  プロセスにアタッチする。WKWebView のネットワーク/レンダラプロセスは別
  プロセスとして Instruments のプロセス選択に出るはずなので、`velox` (VeloX
  本体) だけを選んで記録すれば §2.1 と同じ理由でプロセス境界による分離が
  効くはず。
- **Windows**: Visual Studio の「メモリ使用量」診断ツールか、Windows
  Performance Recorder の heap プロファイリング機能を `velox.exe` に対して
  使う。WebView2 のプロセス (`msedgewebview2.exe`) とは別プロセスとして
  記録されるはずなので、同様に `velox.exe` だけを対象にすればよい。

実機での検証は §8 のチェックリストに含める。

---

## 3. プロセスツリー単位の観測 (RSS/PSS) — 既存の `browser::metrics` との使い分け

**PSS を使うべき理由そのものはここでは繰り返さない。**
`docs/performance-targets.md` §3.1 (「⚠️ メモリ比較には PSS を使うこと」) と
`docs/decisions.md` D41/D42 を参照すること。この節は「どのツールで測るか」
の使い分けだけを扱う。

| 目的 | 使うもの |
| --- | --- |
| VeloX のベンチマーク結果 (`velox-bench`) にメモリを 1 値として残したい、既存の起動/ページロードの計測と同じログに混ぜたい | `browser::metrics` の定期 RSS/PSS サンプリング (`VELOX_PERF_RSS_INTERVAL_MS`)。`docs/benchmarking.md` の `rss_total_bytes`/`pss_total_bytes` を参照 |
| VeloX を通常のビルド/起動方法のまま (計測用の環境変数無しで)、外から時系列で観察したい。他ブラウザや、計測フラグを立てていない既存プロセスも対象にしたい | `scripts/profile/pss_sampler.py` (この文書、下記) |
| ある一瞬のスナップショットだけでよい、他ブラウザとの比較もしたい | `scripts/bench/compare_browsers.py` (`docs/performance-targets.md` §3.1) |

3 つとも `/proc/<pid>/smaps_rollup` の `Pss:` 行 (kB) を読む、同じ実装方針
(D42 が明記するとおり、独立した実装がずれて数値が食い違うことを避けるため
意図的に揃えている)。`/proc/<pid>/smaps_rollup` の読み方自体は
`scripts/profile/pss_sampler.py::_pss_bytes` /
`scripts/bench/compare_browsers.py::_pss_bytes` /
`browser::metrics::sample_process_tree_rss` (Rust 側) を参照 —
カーネル (Linux ≥ 4.14) が計算済みの合計を返すので、`/proc/<pid>/smaps` の
マッピングごとの詳細をパースするより軽い。読めない場合 (権限・古いカーネル・
非 Linux) は `None`/欠損として扱い、`0` を装わない (D42)。

### `pss_sampler.py` の使い方

```sh
# 既に動いている VeloX (や他プロセス) にアタッチ
python3 scripts/profile/pss_sampler.py --pid $(pgrep -f target/release/velox | head -1) \
  --interval-ms 1000 --duration-secs 60 \
  --output results/profile/mem-timeline-$(date +%Y%m%d).csv

# 自分で起動してから追跡 (起動直後からサンプルしたいとき)
python3 scripts/profile/pss_sampler.py \
  --interval-ms 500 --duration-secs 30 \
  --output results/profile/mem-timeline-$(date +%Y%m%d).csv \
  --launch -- ./target/release/velox --homepage http://127.0.0.1:8731/minimal.html
```

実際にこのコンテナで確認した出力 (`--launch`、6 秒、500ms 間隔):

```
elapsed_ms,timestamp_iso,rss_bytes,pss_bytes,process_count,pss_process_count
0.0,2026-09-02T16:51:10.305022+00:00,48926720,19184640,1,1
581.4,2026-09-02T16:51:10.970911+00:00,753389568,320181248,5,5
1242.9,2026-09-02T16:51:11.547404+00:00,816291840,359517184,5,5
...
```

`process_count` が 1→5 に増えている行 (`WebKitWebProcess` 等の起動) が
そのままタイムスタンプ付きで見えるので、「メモリがいつ・どのプロセスの
起動に伴って増えたか」を粗く追うのに使える。より細かくどのプロセスが
何バイトかを見たい場合は `/proc/<pid>/smaps_rollup` を個別に読むか、
`ps --ppid <root pid> -o pid,comm,rss` を併用する。

---

## 3.5 CPU 使用率の観測 (Issue #64)

メモリと同じく、CPU も**外から測る手段**と**VeloX 自身が記録する手段**の
2 つがある。使い分けを間違えると数字がずれる。

| 手段 | 何が出るか | いつ使うか |
| --- | --- | --- |
| `scripts/profile/cpu_usage.py` | プロセスツリー全体の CPU 使用率 (1 コア = 100%)、プロセス別内訳 | **絶対値**が要るとき。VeloX の外から `/proc/<pid>/stat` を 2 点だけ読むので、測定コストが被測定側に乗らない |
| `velox-bench run --scenario background_cpu` の `cpu_percent` | 同上を VeloX 自身のサンプラが記録したもの | **回帰検知**。サンプラが /proc を歩くコストが乗るが、同一シナリオの before/after では打ち消し合う |
| `perf` (§1) | どの関数が CPU を使っているか | 使用率が高い原因を特定するとき |

`cpu_usage.py` は `velox` を自動操作スクリプト付きで起動し、`--settle-secs`
待ってから `--window-secs` の窓で CPU 時間の差を取る。起動とページ読み込みの
コストは窓の外に出る。

```sh
P=$PWD/scripts/bench/pages
printf 'open file://%s/busy.html\nwait 1500\nopen file://%s/minimal.html\nwait 60000\nquit\n' \
  $P $P > /tmp/bg.txt
VELOX_MAX_TABS_PER_PROCESS=1 xvfb-run -a --server-args="-screen 0 1280x900x24" \
  dbus-run-session -- python3 scripts/profile/cpu_usage.py \
    --velox target/release/velox --script /tmp/bg.txt \
    --homepage file://$P/minimal.html --label background --window-secs 16
```

`VELOX_MAX_TABS_PER_PROCESS=1` を付けるとタブごとに web プロセスが分かれる
ので、内訳からどのタブが使っているかを読み取れる (D54/D57)。

**負荷源**: `scripts/bench/pages/busy.html` が `requestAnimationFrame`
ループ・10ms タイマー・CSS アニメーションで実際に CPU を焼く。`?idle=1` で
全ループを止めた静的版、`?beacon=1` で発火ごとに同一オリジンへ 1 本投げる版に
なる。ビーコンは「CPU が少ない」と「完全に止まっている」を区別するために
使う — 詳細は `docs/decisions.md` D58。

## 3.6 バックグラウンドタブのネットワーク活動の観測 (Issue #65)

CPU (§3.5) と同じ「外から測る」考え方をネットワーク要求に広げたもの。
ただし CPU と違い、VeloX 自身が記録できる相当物が無い —
`docs/decisions.md` D17/D59 のとおり、wry 0.56 がリクエスト単位の横取り
フックを公開しているのは Windows (WebView2) だけで、この開発環境
(Linux/WebKitGTK) では VeloX の中からリクエストを一切観測できない。その
ため `scripts/profile/network_activity.py` は VeloX の外に立てたローカル
HTTP/WebSocket サーバのアクセスログだけで数える。

```sh
printf 'wait 1500\nopen about:blank\nwait 16000\nquit\n' > /tmp/bg.txt
xvfb-run -a --server-args="-screen 0 1280x900x24" dbus-run-session -- \
  python3 scripts/profile/network_activity.py \
    --velox target/release/velox --script /tmp/bg.txt \
    --label background --settle-secs 6 --window-secs 16
```

**負荷源**: `scripts/bench/pages/network_activity.html` が 4 種類の
バックグラウンド通信パターン (Issue #65 の分類に対応) を別々のパスに
向けて発生させる — 2 秒おきの `fetch`/`XHR` ポーリング、3 秒おきの
`<img>` 差し替え (background resource loading)、1 回だけの
`<link rel=prefetch>`、2 秒おきの WebSocket 心拍 (保護対象)。
`?poll_ms=` でポーリング間隔を上書きできる。結果と考察は
`docs/performance-targets.md` §15、設計判断は `docs/decisions.md` D80。

## 4. profiling プロファイルとベンチマーク baseline の紐付け

`docs/benchmarking.md` の「baseline の保存形式」と同じ方針を踏襲する:
**プロファイルのファイル名に日付と git commit を含める。** 対応するコード
が分からないプロファイルは後から見て意味を持たない。

```
results/profile/<種別>-<シナリオ>-<YYYYMMDD>-<git short sha>.<拡張子>
```

例:

```
results/profile/cpu-startup-20260902-a60b710.svg
results/profile/cpu-startup-20260902-a60b710.data
results/profile/heap-startup-20260902-a60b710.gz
results/profile/mem-timeline-20260902-a60b710.csv
```

`scripts/profile/run_perf.py`/`run_heaptrack.py` の `--output-prefix`/
`--output` にこの命名で渡せば、`.data`/`.script`/`.folded`/`.svg` (perf) や
`.gz` (heaptrack) が揃って同じ接頭辞で並ぶ。`results/` はこのリポジトリでは
コミットしない作業ディレクトリ (`docs/benchmarking.md` の `results/` と同じ
扱い) — チームで共有する場合は Issue や PR にファイルを添付するか、外部の
アーティファクトストレージに置き、Issue 側にはリンクとコマンド/コミット/
環境の記録を残す (§5 参照)。

---

## 5. プロファイルから Issue を起票する運用

プロファイリングで具体的なボトルネック (関数・アロケーション箇所) を
特定できたら、次のフォーマットで Issue を起票する。**再現できない
プロファイル報告は後から検証できず、Epic #57 の「ベンチマークなしの
最適化をしない」の逆 — 「プロファイルなしの Issue を起票しない」ことに
つながる。**

Issue 本文に含めるもの:

1. **症状**: どのベンチマーク/操作が、どの目標 (`docs/performance-
   targets.md` の T1〜T4 など) に対してどれだけ悪いか。
2. **プロファイルの取り方**: 使ったツール (`perf`/`heaptrack`/`massif`) と
   正確なコマンド (`scripts/profile/run_perf.py ...` の実際の引数)。
3. **環境**: `docs/performance-targets.md` §1 の環境情報一式、**および GPU
   の有無**。この環境の数値 (GPU 無し) と実機の数値を混同しないこと —
   §8 のチェックリストを実施済みかどうかを明記する。
4. **測定対象の git commit**: `git rev-parse HEAD`。
5. **プロファイル本体**: `.svg` フレームグラフ、`heaptrack_print`/`ms_print`
   の該当箇所の抜粋、または `pss_sampler.py` の CSV から作ったグラフ。
   ファイル自体を添付するか、外部ストレージへのリンクを貼る。
6. **疑わしい箇所**: プロファイルから読み取れる、青色 (VeloX 自身のコード —
   §1.5 参照) の中で最も太い/深いフレーム、または `heaptrack_print` で
   コール回数の多いスタック。
7. **コスト/メリットラベル**: `CLAUDE.md` の「Issue のラベリングポリシー」
   に従い `cost:Low|Mid|High` と `benefit:1`〜`5` を付ける。

このフォーマットに従えば、担当者を変えても同じ手順 (§1〜§4) でプロファイル
を再取得でき、「直った/直っていない」を同じ条件で確認できる。

---

## 6. コマンド早見表

```sh
# debug symbols 付きビルド (一度だけ)
cargo build --profile profiling

# 固定ページを配信 (再現性のため、外部ネットワークは使わない)
(cd scripts/bench/pages && python3 -m http.server 8731 &)

# CPU: perf record → flamegraph.svg まで一括
python3 scripts/profile/run_perf.py \
  --duration-secs 8 --output-prefix results/profile/cpu-startup \
  -- ./target/profiling/velox --homepage http://127.0.0.1:8731/minimal.html

# メモリ: heaptrack (VeloX 自身の Rust heap だけが対象になる)
python3 scripts/profile/run_heaptrack.py \
  --duration-secs 15 --output results/profile/heap-startup \
  -- ./target/profiling/velox --homepage http://127.0.0.1:8731/minimal.html

# プロセスツリー全体の RSS/PSS を時系列で
python3 scripts/profile/pss_sampler.py \
  --interval-ms 500 --duration-secs 30 \
  --output results/profile/mem-timeline.csv \
  --launch -- ./target/release/velox --homepage http://127.0.0.1:8731/minimal.html
```

---

## 7. この環境の限界

`docs/performance-targets.md` §1 が明記するとおり、**このコンテナには GPU
が無く、ソフトウェアレンダリングにフォールバックする。** これがプロファイル
のどの部分に影響するかを、指標ごとに切り分けて整理する。

### 実機と乖離しやすい指標

- **メモリ (RSS/PSS) の絶対値**: ソフトウェアレンダラは GPU 版と異なる
  バッファ管理・キャッシュ戦略を使うため、`docs/performance-targets.md` §1
  が既に警告しているとおり実機の値とは一致しない。§3 の `pss_sampler.py`/
  `heaptrack` で得た**絶対バイト数**を実機の基準値として扱わないこと。
- **描画/コンポジット関連の CPU 時間**: §1.3 のフレームグラフに出てくる
  `SkiaGPUWorker`/`llvmpipe-*` (実際にこのコンテナのプロファイルに出現した
  — `llvmpipe` は Mesa のソフトウェアラスタライザ) はこの環境固有の
  ソフトウェアレンダリング経路であり、実機では GPU 側にオフロードされて
  ほぼ消えるか、全く別のプロファイルになる。
- **`libEGL warning: DRI3 error` に関連する初期化コスト**: `docs/decisions.md`
  D43 が記録したとおり、`LIBGL_ALWAYS_SOFTWARE=1
  WEBKIT_DISABLE_COMPOSITING_MODE=1` を設定すると `rust_setup_done →
  toolbar_script_started` の区間がおよそ半分になる実験結果が出ている。
  **これは失敗する DRI3/EGL ネゴシエーションのリトライをスキップしている
  だけの、この環境固有のアーティファクト**であり、実 GPU がある環境では
  再現しない (むしろハードウェアアクセラレーションを無効化することになり
  実機では悪化しうる)。

### 実機でも比較的信頼できる指標

- **アロケーション回数・コールスタック (`heaptrack` の "MOST CALLS TO
  ALLOCATION FUNCTIONS")**: どの関数が何回 malloc を呼んでいるかという
  *相対的な* 内訳は、レンダリング方式に強く依存しない Rust/GTK 側の
  ロジック (history/bookmarks の読み込み、`AppState` 構築、IPC メッセージの
  組み立てなど) については実機でも近い傾向になりやすい。ただし絶対バイト数
  は上記の理由でずれうる。
- **`velox::` 名前空間の CPU プロファイル (青色のフレーム)**: VeloX 自身の
  Rust コードの実行時間は、GPU の有無に直接は依存しない (D43 が実測した
  とおり、`rust_setup_done` までの Rust 側処理は無視できるコストで、GPU
  云々より前の話)。ただし全体に対する相対的な太さ (%) は、分母である
  レンダリング関連のコストが実機で縮むぶん変わりうる。
- **プロセス数・プロセス構成** (`process_count`): レンダリング方式ではなく
  WebKitGTK のプロセスモデルで決まるので、GPU の有無にほぼ影響されない。

### 実機再測定チェックリスト

実機 (GPU あり) で再測定する際は、少なくとも以下を確認・記録する:

- [ ] `lspci`/`glxinfo` 等で使用している GPU とドライバを記録する
      (`docs/performance-targets.md` §1 の環境情報一式に追記する形で)。
- [ ] `libEGL warning: DRI3 error` が **出ないこと**を確認する (出ていたら
      実機でもソフトウェアレンダリングにフォールバックしており、この文書の
      「実機と乖離しやすい指標」の警告がそのまま当てはまる)。
- [ ] §1 の CPU プロファイルを取り直し、`SkiaGPUWorker`/`llvmpipe-*` の
      比率がこの環境の値 (§9 実測: 合計で全体の 2 割強) からどう変わるかを
      比較する。
- [ ] §2/§3 のメモリプロファイルを取り直し、`pss_total_bytes` の絶対値を
      この文書の値と比較する — **改善したかどうかではなく、この環境の値と
      構造的に近いかどうか (同じ関数が上位に来るか) を先に見る。**
- [ ] `perf stat -e cycles,instructions` がハードウェアイベントを実際に
      返すことを確認する (このコンテナでは `<not supported>` だった —
      実機でこれが解消されているかどうかで、`--event cycles` に切り替える
      べきかが決まる)。
- [ ] `docs/performance-targets.md` §4 の競合比較 (`compare_browsers.py`)
      も実機で取り直し、T1/T2 の閾値判定をこの環境の値ではなく実機の値で
      やり直す (同 §9 が既に指摘しているとおり、この環境はセッション間の
      変動も大きい)。
- [ ] macOS/Windows で計測する場合は §1.6/§2.5 の手順を実際に検証し、
      「未検証」の注記をこの文書から外す (PR で更新する)。

---

## 8. この環境での検証状況 (正直な記録)

`docs/benchmarking.md` の「この環境での検証状況」に倣い、実際にこのコンテナ
で確認できたことと、できなかったことを明確に分ける。

**実際に動かして確認できたこと** (すべて本文中に実行結果を記載済み):

- `cargo build --profile profiling` でシンボル付きバイナリが作れること、
  `target/release/velox` のサイズ・BuildID が変わらないこと (§0)。
- `/usr/bin/perf` がこのコンテナのカーネルでは動かないこと、
  `/usr/lib/linux-tools/6.8.0-138-generic/perf` へのフォールバックが
  実際に動作すること (§1.1)。
- `perf stat -e cycles,instructions` がこの環境で `<not supported>` になる
  こと、`-e cpu-clock` (ソフトウェアイベント) なら動作すること (§1.2)。
- `perf record -g -e cpu-clock -F 999` で VeloX 起動シーケンスを記録し、
  `perf report --comms=velox` で `velox::app::run` 等の Rust シンボルまで
  解決できること (§1.3)。
- `scripts/profile/run_perf.py` を `DISPLAY` 未設定の状態から実行し、
  自前で Xvfb を起動 → `perf record` → `perf script` →
  `scripts/profile/flamegraph.py` の一連が最後まで通り、有効な SVG (XML
  として妥当、1000 以上の矩形を含む) が生成されること (§1.4)。
- `heaptrack ./target/profiling/velox ...` の実行で `.gz` ファイルが
  **1 個だけ**生成され (`WebKitWebProcess`/`WebKitNetworkProcess` 用の
  ファイルは生成されない)、`heaptrack_print` で `velox::app::run` を含む
  Rust のファイル名・行番号付きコールスタックが読めること (§2.1〜§2.2)。
- `scripts/profile/run_heaptrack.py` が `run_perf.py` と同じ Xvfb 自動起動
  ロジックで最後まで通ること (§2.2)。
- `valgrind --tool=massif --pages-as-heap=no` で VeloX を実行し、
  `ms_print` で ASCII グラフとコールスタック付きのピーク内訳が読めること
  (§2.3)。
- `scripts/profile/pss_sampler.py --launch` で VeloX を起動しつつ CSV
  タイムラインを取得し、`process_count` が起動シーケンス中に 1→5 に増える
  様子が時系列で見えること (§3)。
- `python3 -m py_compile` で `scripts/profile/*.py` すべての構文確認。

**確認できなかったこと (正直な記録)**:

- **macOS/Windows での CPU/メモリプロファイリング手順** (§1.6/§2.5):
  このコンテナには macOS/Windows 環境が無いため、Instruments/WPR/WPA を
  実際に動かして確認していない。標準的な手順として記載したが、実機での
  検証が必要 (§7 チェックリストの最後の項目)。
- **`heaptrack-gui` によるグラフィカルな分析**: このコンテナには
  `heaptrack-gui` を入れていない (`heaptrack_print` のテキスト出力のみで
  検証)。`apt install heaptrack-gui` 自体は候補として見えていたので、
  入れれば動く可能性は高いが未検証。
- **実行中プロセスへの `heaptrack -p <PID>` アタッチ**によるレンダラ
  プロセス側の heap 分析: `heaptrack --help` 自身が「不安定でクラッシュ
  しうる」と警告しているため、意図的に試していない。
- **ハードウェアイベント (`-e cycles` 等) でのプロファイリング**: この環境
  では PMU が使えないため、動作を確認できていない。実機で確認すること
  (§7 チェックリスト)。
- **実機 (GPU あり) での数値**: 前提から実行できない。§7 のチェックリストが
  その代わりとなる手順書である。
