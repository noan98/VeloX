# VeloX ユーザーガイド

VeloX を入手し、更新し、削除し、困ったときに問い合わせるための手順です。
開発者向けの情報 (ビルド・テスト・設計) は [../README.md](../README.md) と
[architecture.md](architecture.md) にあります。

> **対応 OS の現状**: Windows (WebView2) が最優先で、日常利用を想定した
> 配布物 (インストーラ) があるのは Windows だけです。Linux は tar.gz のみ、
> macOS は配布物がありません (ソースからビルドします)。理由と経緯は
> [decisions/archive.md](decisions/archive.md) の D70 を参照してください。
> 現時点では自動更新もありません (Epic #81 の #90 で扱う予定の項目です)。

## インストール

配布物は [GitHub Releases](https://github.com/noan98/VeloX/releases) にあります。

### Windows

1. 最新の Release から `velox-<バージョン>-windows-x86_64-setup.exe` をダウンロードします。
2. 実行します。**管理者権限を要求します** (インストーラの設定が
   `PrivilegesRequired=admin`、既定のインストール先は `Program Files` 配下の
   `VeloX`)。
3. ウィザードでデスクトップショートカットの作成を選べます。最後の画面の
   「Launch VeloX」で、そのまま起動できます。

動作要件は WebView2 Runtime です。Windows 11 と最近の Windows 10 には
最初から入っています。

**署名について**: 署名用の証明書がまだ無いため、Release のインストーラは
現状 **未署名** です (docs は [windows-code-signing.md](windows-code-signing.md))。
初回実行時に Windows SmartScreen が警告を出すことがあります。署名が有効に
なるまでは、次の「ダウンロードの検証」を行わずに実行しないでください。

### Linux

1. `sudo apt install libwebkit2gtk-4.1-dev` (Debian/Ubuntu。実行時に WebKitGTK が必要です)。
2. Release から `velox-<バージョン>-linux-x86_64.tar.gz` と `.sha256` をダウンロードします。
3. 検証して展開します。

   ```sh
   sha256sum -c velox-<バージョン>-linux-x86_64.tar.gz.sha256
   tar xzf velox-<バージョン>-linux-x86_64.tar.gz
   ./velox-<バージョン>-linux-x86_64/velox
   ```

パッケージ形式 (AppImage / deb) はありません。tar.gz には `velox`・
`velox-bench` (計測用。通常は不要)・README・LICENSE が入っています。

### macOS

配布物はありません。ソースからビルドします (`cargo build`、
[../README.md](../README.md#macos))。

### ダウンロードの検証

Linux の tar.gz には `.sha256` が付きます (上記の `sha256sum -c`)。
Windows のインストーラについては、Release の資産にハッシュファイルは
現状付いていません。Windows では次のコマンドで SHA-256 を計算できますが、
比較する公式の値が無いため、**ダウンロード元が本リポジトリの Release
ページであること** を確認してください。署名の導入 (#42 / #91) が完了すれば、
署名の検証がこの代わりになります。

```powershell
Get-FileHash .\velox-<バージョン>-windows-x86_64-setup.exe -Algorithm SHA256
```

## 更新

自動更新はありません。手動で更新します。

- **Windows**: 新しいバージョンの `*-setup.exe` をそのまま実行します。
  インストーラは同じアプリ ID を使うので、既存のインストールに上書きされ
  ます。ブラウザは終了してから実行してください。
- **Linux**: 新しい tar.gz を展開して置き換えます。
- **ソースから**: `git pull` して `cargo build --release`。

履歴・ブックマーク・設定はユーザーデータの保存先 (下記) に別置きなので、
更新しても保たれます。何が変わったかは Release ページのリリースノートで確認
できます (作り方は [releasing.md](releasing.md))。**バージョンを戻す (ロール
バック) 場合** は、古いバージョンのインストーラ / tar.gz を入れ直します。
ただし新しいバージョンが保存形式を変えている場合の互換性は保証されません
(移行・ロールバックの仕組みは Epic #81 の #92 の範囲です)。

## アンインストール

- **Windows**: 「設定 → アプリ」(または「プログラムの追加と削除」) から
  VeloX を削除します。インストーラが配置するのは、アプリのフォルダ・スタート
  メニュー / デスクトップのショートカットです。
- **Linux**: 展開したフォルダを削除します。

**ユーザーデータは自動では消えません。** インストーラは履歴やブックマーク
などの保存先を削除しません。完全に消したい場合は、VeloX を終了してから次の
フォルダを削除してください (`VELOX_DATA_DIR` を設定していた場合はそこ)。

| OS | ユーザーデータの保存先 |
|---|---|
| Windows | `%APPDATA%\VeloX` |
| macOS | `~/Library/Application Support/VeloX` |
| Linux | `$XDG_DATA_HOME/velox` (未設定なら `~/.local/share/velox`) |

中身は `history.json` / `bookmarks.json` / `input_history.json` /
`session.json` / `site_permissions.json` / `settings.json` です。
バックアップしたいときはこのフォルダをコピーします。

同じフォルダに `*.v<数字>.bak` (更新・ダウングレード時に自動で取られる控え) や
`*.corrupt-<数字>` (壊れていたため退避されたファイル) ができることがあります。
旧版へ戻すときの使い方は [migration.md](migration.md) の「ロールバック」を
参照してください。

## トラブルシューティング

| 症状 | 確認すること |
|---|---|
| Windows で SmartScreen の警告が出る | インストーラが未署名のためです (上記)。Release ページ由来の配布物であることを確認したうえで実行してください |
| 起動しない / ウィンドウが出ない (Windows) | WebView2 Runtime が入っているか確認します (Windows 11 には標準で入っています) |
| 起動しない (Linux) | `libwebkit2gtk-4.1` が入っているか、ターミナルから起動してエラー出力を確認します。WebKitGTK の Web プロセスは D-Bus セッションバスを必要とします |
| 履歴・ブックマーク・設定が保存されない | 保存先が決められない環境 (環境変数 `APPDATA` / `HOME` が無い) では、その実行では保存されません。`VELOX_DATA_DIR` で保存先を明示できます。保存ファイルが壊れていても、その項目は空として起動します (起動は止まりません) |
| 前回のタブが復元されない | 復元は既定では無効です。環境変数 `VELOX_RESTORE_SESSION` を設定して起動したときだけ、前回のタブを開きます。クラッシュの検知はできません (README の Session restore を参照) |
| 詳しいログが欲しい | 環境変数 `VELOX_DEBUG` を設定してターミナルから起動すると、診断用の詳細ログが標準エラー出力に出ます |
| メモリ使用量が多い | タブの休止 (suspend) が、既定でメモリ予算に応じて働きます。`VELOX_MEMORY_BUDGET_MB` や設定画面の Performance タブで調整できます (詳細は [../README.md](../README.md) の Performance) |
| プライベートに見せたくない | プライベートウィンドウは、それぞれ独立した保存領域を持ちます (`--private` で全体をプライベートで起動) |

## 問い合わせ・報告

| 内容 | 窓口 |
|---|---|
| 不具合 | [Issues](https://github.com/noan98/VeloX/issues/new/choose) の「不具合の報告」 |
| 機能要望 | 同じく「機能要望」 |
| **セキュリティ脆弱性** | **公開 Issue に書かず**、[非公開の脆弱性報告](https://github.com/noan98/VeloX/security/advisories/new) を使います。手順・対象範囲は [../SECURITY.md](../SECURITY.md) |

報告には VeloX のバージョン、OS、再現手順を添えてください。パスワードや
個人情報が含まれないよう、ログは貼る前に確認してください。
本プロジェクトの個別サポートの応答時間は保証されません (OSS として
ベストエフォートで対応します)。
