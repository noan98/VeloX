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
| [02-tabs-session.md](./02-tabs-session.md) | タブ、履歴、ブックマーク、プライバシー、omnibox | D8–D15, D20, D22–D40 |
| [03-performance.md](./03-performance.md) | メトリクス、ベンチマーク、メモリ、プロファイリング、回帰検知 | D16, D19, D21, D41–D49, D56–D58 |
| [04-browser-features.md](./04-browser-features.md) | コンテンツブロック、DevTools、ダウンロード、権限、サイトデータ、検索 | D17–D18, D23–D28, D59–D60, D64, D66, D69 |
| [05-platform-release.md](./05-platform-release.md) | Windows、アイコン、CI、リリース、マルチウィンドウ | D50–D55, D68, D70 |
| [06-security-maintenance.md](./06-security-maintenance.md) | 入力値堅牢性、依存監査、セッション復元、設定 | D61–D67 |

## 重要なルール

1. **archive の内容を削除・要約置換しない**。詳細な根拠は一次記録として残す。
2. カテゴリファイルはナビゲーション用。判断の全文・検証値・revisit condition は archive を参照する。
3. 複数カテゴリにまたがる Decision は主担当カテゴリを一つ決め、他カテゴリから相互参照する。
4. 新しい判断を既存判断の修正として記録する場合は、既存 `Dxx` を書き換えず、新しい Decision を追加して履歴を残す。
5. 実装・Issue・PR との対応関係は Decision 内の `Issue #` / ファイル参照を優先する。
