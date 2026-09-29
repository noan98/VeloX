# リリース手順

タグを打ってから GitHub Release が公開されるまでの手順と、現時点で手作業・
未対応の部分をまとめます (Issue #94)。利用者向けの案内は
[user-guide.md](user-guide.md) です。

## 全体像

`v*` タグを push すると、次の 2 つの workflow が同じタグで動き、同じ
GitHub Release に成果物を添付します (どちらも `softprops/action-gh-release`)。

| workflow | 成果物 |
|---|---|
| `.github/workflows/release-windows.yml` | `velox-<version>-windows-x86_64-setup.exe` (Inno Setup。定義は `installer/windows/velox.iss`) |
| `.github/workflows/release-linux.yml` | `velox-<version>-linux-x86_64.tar.gz` と `.sha256` |

macOS 用の release workflow はありません (D70)。`workflow_dispatch` で
手動実行すると、Release は作らず成果物を Actions の artifact
(`velox-windows-x86_64-installer` / `velox-linux-x86_64`、保持 30 日) に
置くだけなので、タグを打つ前の試走に使えます。

同一タグに対する Release 作成は、両 workflow が同じ concurrency group
(`release-tag-<ref>`) を共有して直列化されます。

## 手順

1. **バージョンを決める。** `Cargo.toml` の `version` (現在 `0.1.0`) を
   更新し、`Cargo.lock` も更新して、通常の PR でマージします。
   両 workflow は、タグ `vX.Y.Z` が `Cargo.toml` の version と一致しないと
   ビルドを失敗させます (Release が中身と食い違うのを防ぐため)。
2. **リリース前の確認。** `main` の CI (fmt / clippy / test / build) が
   緑であること。Windows は優先 OS なので、Windows ジョブが緑であること。
   必要なら `release-windows.yml` / `release-linux.yml` を Actions から
   手動実行して、成果物を実機で確かめます。
3. **プルリクエストの分類を確認する。** リリースノートは、前回のリリース
   以降にマージされた PR を **ラベルで分類** して自動生成されます (次節)。
   `bug` / `enhancement` / `documentation` / `dependencies` が付いていない
   PR は「その他」に入ります。
4. **タグを打つ。**

   ```sh
   git tag v0.1.0
   git push origin v0.1.0
   ```

5. **Actions を確認する。** 2 つの release workflow が緑になり、Release に
   Windows のインストーラと Linux の tar.gz が並んでいること。
6. **Release ページを確認する。** 生成されたリリースノートを読み、
   必要なら Release の編集画面で補足します (下記「リリースノート」)。
7. **Windows のインストーラを 1 度実機で実行する** (署名が有効になるまでの
   暫定手順。SmartScreen の表示も確認します)。

### やり直し

タグ作成後にビルドが失敗した場合は、Release が作られていないことを確認
してから、修正を `main` にマージし、**タグを削除して打ち直す**
(`git push --delete origin vX.Y.Z` → 再度タグ)。すでに Release が公開されて
しまった場合は、Release を削除してから打ち直します。公開済みの成果物を
同じバージョン番号で差し替えることは避け、修正版として次のバージョンを
出す方が安全です。

## リリースノート

- 生成: 両 workflow が `generate_release_notes: true` を指定しており、
  GitHub が前回の Release からの差分 PR を自動でまとめます。
- 分類: `.github/release.yml` がラベルごとの見出しを定めます
  (不具合修正 = `bug`、新機能・改善 = `enhancement`、ドキュメント =
  `documentation`、依存関係の更新 = `dependencies`、その他 = ラベルなし)。
  Dependabot の PR は自動で `dependencies` が付きます。
- 本文の先頭には、Windows インストーラが署名済みか未署名かの一文が入ります
  (`release-windows.yml` の `body`)。
- **CHANGELOG.md は置きません。** 変更履歴の一次情報は GitHub Release の
  リリースノートです。PR タイトル・本文は CLAUDE.md により日本語で書く
  ため、そのままリリースノートの品質になります。**PR にはラベルを付ける
  こと** (マージ前にメンテナが確認します)。
- 設計判断の経緯は [decisions/README.md](decisions/README.md) にあります。

## 手作業・未対応の項目

| 項目 | 状態 |
|---|---|
| Windows のコード署名 | 仕組みは実装済みだが **証明書 (Azure Artifact Signing) が未設定**。6 つの Secrets / Variables が揃うと自動で有効化されます。必要なもの・調査結果は [windows-code-signing.md](windows-code-signing.md)。Issue #42 / #91 |
| macOS の配布物・署名・公証 | 未対応 (Apple Developer Program 前提。D70 / D73) |
| Windows インストーラの SHA-256 ファイル | Release には付かない (Linux の tar.gz だけ `.sha256` が付く)。署名導入までは、Release ページ由来であることの確認が頼り |
| 自動更新 / 移行・ロールバック | 未実装 (Epic #81 の #90 / #92) |
| リリースチャンネル (Stable / 開発) | 未定 (Epic #81 の #88)。現状は `v*` タグ 1 種類 |
| 配布形式 (winget / MSI / AppImage / deb) | 未対応。導入するかはオーナーの判断事項 |
| ロードマップの文言 | README の Roadmap 節は「Packaging, code signing and notarization」を残課題としたまま。公開前にオーナーが更新する |
