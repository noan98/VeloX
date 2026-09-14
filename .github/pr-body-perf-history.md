<!--
  perf-windows.yml の `ingest-history` ジョブが `gh pr create --body-file` で
  使う定型文 (Issue #211 項目4 / docs/decisions.md D106 Revisit condition (1))。

  ⚠️ このファイルにクロージングキーワード (Closes / Fixes / Resolves) と
  Issue 番号を隣接させて書かないこと。自動生成の PR が毎週 Issue を閉じて
  しまう (CLAUDE.md / D128 / D131)。
-->
## 概要

`perf-windows.yml` の週次計測 (Issue #211 項目4) の結果を
`results/history/windows/` に追記した**自動生成の PR** です。
コードの変更は含まれません。

## 何のために貯めるのか

Issue #211 項目3 —「**機種を揃えたときの run 間分散を実測する**」ための
データです。D96 Revisit condition (2) のとおり「同一機種なら run を跨いで
比較してよい」はまだ言えておらず、ゲート化の閾値はこの分散が見えてから
決めます。

## 系列の分け方

`report.py` は **`session_id` と `machine_key` の両方が一致する隣接エントリ
だけ**を線でつなぎます (D106)。

| 軸 | 担保するもの |
| --- | --- |
| 機種 (CPU / コア数 / RAM / OS) | `machine_key` |
| 計測条件 (A 腕 / B 腕 / 環境変数) | `session_id` |
| シナリオ | 保存先ファイルの分割 |

条件が変われば `session_id` が変わるので、**別の条件で測った数値が同じ系列に
入ることはありません。**

## レビュー観点

追記されている行が `results/history/` の下だけであること。それ以外の差分が
あれば生成ジョブ側の不具合です (ジョブは範囲外の差分を検出したら落ちる
ようになっています)。
