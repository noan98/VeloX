# Design Decisions

VeloX の設計判断は、実装の経緯・代替案・トレードオフまで含めて記録しています。

## 読み方

- **現在の設計を知りたい** → 下のカテゴリ別インデックスから該当する Decision を開く
- **実装時の詳細な経緯・検証結果を確認したい** → [`archive.md`](./archive.md) の該当 `Dxx` を参照
- **新しい設計判断を追加する** → まずカテゴリを選び、既存 Decision と重複しないか確認してから archive に追記する

`archive.md` は元の `docs/decisions.md` を内容変更なしで移動したものです。情報を失わないことを最優先し、詳細な一次記録を一か所に保持します。

## カテゴリ

| ファイル | 対象 | Decision |
|---|---|---|
| [01-foundation.md](./01-foundation.md) | エンジン、UI、依存関係、基本アーキテクチャ | D1–D7 |
| [02-tabs-session.md](./02-tabs-session.md) | タブ、履歴、ブックマーク、プライバシー、omnibox | D8–D15, D20, D22–D40, D74 |
| [03-performance.md](./03-performance.md) | メトリクス、ベンチマーク、メモリ、プロファイリング、回帰検知 | D16, D19, D21, D41–D49, D56–D58, D79–D82, D84–D90, D92–D97, D99, D101, D104–D106, D109–D115, D117 |
| [04-browser-features.md](./04-browser-features.md) | コンテンツブロック、DevTools、ダウンロード、権限、サイトデータ、検索 | D17–D18, D59–D60, D64, D66, D69, D71–D72, D75–D78 |
| [05-platform-release.md](./05-platform-release.md) | Windows、アイコン、CI、リリース、マルチウィンドウ | D50–D55, D68, D70, D73, D83, D91, D98, D100, D103, D107–D108 |
| [06-security-maintenance.md](./06-security-maintenance.md) | 入力値堅牢性、依存監査、セッション復元、設定 | D61–D63, D65, D67, D102, D116 |

## 重要なルール

0. **カテゴリ表と各ファイルの「詳細」一覧は手で書かない。**
   割り当ては `.github/scripts/decision_index.py` に持ち、`archive.md` の
   見出しから機械的に生成する。アンカーは GitHub の `github-slugger` 規則で
   作る必要があり (`、` `・` `「」` `→` はハイフンではなく**除去**される)、
   目分量で書くとリンク切れになる — Issue #227 の起票時点では、既存リンク
   22 本のうち 14 本が実際にそうなっていた。
   `.github/scripts/test_decision_index.py` が CI で検査する。
1. **archive の内容を削除・要約置換しない**。詳細な根拠は一次記録として残す。
2. カテゴリファイルはナビゲーション用。判断の全文・検証値・revisit condition は archive を参照する。
3. 複数カテゴリにまたがる Decision は主担当カテゴリを一つ決め、他カテゴリから相互参照する。
4. 新しい判断を既存判断の修正として記録する場合は、既存 `Dxx` を書き換えず、新しい Decision を追加して履歴を残す。
5. 実装・Issue・PR との対応関係は Decision 内の `Issue #` / ファイル参照を優先する。
