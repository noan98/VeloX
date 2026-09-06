# コード署名と配布信頼性 (Issue #42)

Issue #42「macOS/Windowsコード署名と配布信頼性」に対する調査結果と、現状の実装
状況をまとめる。**このリポジトリには署名用の証明書 (Windows: コード署名証明書 /
macOS: Apple Developer Program の Developer ID) が無く、Secrets にも登録されて
いない。** そのため本ドキュメントの時点では「署名された成果物」は生成できない
— 以下は (1) 証明書が用意され次第すぐ有効化できる workflow 実装、(2) 証明書が
無くても今できる配布信頼性の改善、(3) 証明書取得までの調査結果、の 3 点をまとめ
たものである。CLAUDE.md の OS 優先度方針に従い **Windows を主対象**とし、macOS
は概要に留める。

設計判断の記録は [docs/decisions.md](decisions.md) の **D73** を参照。

## 出典について (重要)

この調査は公式ドキュメントの確認を優先したが、本セッションの実行環境では
`learn.microsoft.com` / `docs.github.com` / `azure.microsoft.com` /
`support.apple.com` への直接アクセスがネットワーク境界でブロックされていた。
そのため:

- **確認済み** と明記した項目は、各ベンダーの GitHub リポジトリ (README を
  直接取得できた `azure/artifact-signing-action`) や `developer.apple.com`
  など、実際に取得できたページの内容に基づく。
- それ以外の Microsoft Learn 上の一次情報 (SmartScreen レピュテーションの
  仕様変更、Trusted Signing の適格性要件、CA/Browser Forum の要件など) は、
  検索エンジン経由でそれらの公式ページの内容を要約した結果を根拠にしている
  (ページ本文を直接目視で確認できたわけではない)。複数の独立したソース
  (Microsoft Q&A、CA/認証局各社のブログ、技術者の実装記事) が同じ内容を
  裏付けている場合のみ「確認できた内容」として記載し、単一の非公式ソース
  にしか出てこない情報は本文中で推測である旨を明記した。
- macOS notarization の手順はコミュニティ製 GitHub Actions のドキュメント
  (`developer.apple.com` はページ本文がクライアント側レンダリングのため
  取得できなかった) を根拠にしており、Apple の一次ドキュメントの文言その
  ものは確認できていない。

## 1. Windows: Authenticode 署名

### 1.1 証明書の種類と、2023 年以降の重要な制約

Windows の実行ファイルへの署名には Authenticode 形式のコード署名証明書が必要
で、認証局 (DigiCert, Sectigo, GlobalSign, SSL.com など) が OV (Organization
Validation) と EV (Extended Validation) の 2 種類を発行している。

**確認できた内容**: CA/Browser Forum の Ballot CSC-17 により、**2023 年 6 月
1 日以降に新規発行されるコード署名証明書は EV/OV を問わず、秘密鍵を
FIPS 140-2 Level 2 (または Common Criteria EAL4+) 相当のハードウェア暗号
モジュールに生成・保管しなければならず、秘密鍵をエクスポートできない**よう
になった (複数の認証局・セキュリティベンダーの解説記事で一致)。つまり
「`.pfx` ファイルを認証局からダウンロードして GitHub Actions の Secrets に
そのまま入れる」という従来型のシンプルな運用は、2023 年 6 月以降に新規発行
された証明書ではそもそも成立しない。選択肢は次のいずれかになる。

1. 物理 USB トークン / ローカル HSM を自分で運用する
   → GitHub がホストするランナーには挿せないため、セルフホストランナーが
     前提になる (このリポジトリの CI は GitHub ホストランナーのみ使用して
     おり、対象外)。
2. 認証局やサードパーティが提供する **クラウド HSM 型のリモート署名サービス**
   (DigiCert KeyLocker, SSL.com eSigner, GlobalSign Managed HSM 等) を使う
   → 秘密鍵はベンダー側のクラウド HSM に留まり、CI からは API/CLI 経由で
     「署名リクエスト」を送るだけになる。
3. **Azure Trusted Signing (2024 年に Artifact Signing へ改称)** — Microsoft
   自身が提供するクラウド署名サービス。証明書の発行・更新・タイムスタンプ・
   HSM 管理を Microsoft 側が行う。

このリポジトリでは、GitHub Actions とネイティブに統合できる公式 Action が
存在し、証明書の調達自体も Microsoft のポータルで完結する **Azure Trusted
Signing (Artifact Signing)** を主軸として workflow を実装した (下記 1.4)。
選択肢 2 (他ベンダーのクラウド署名サービス) を使う場合も、`signing` ジョブ
の判定ロジックとステップ構成はほぼ流用できる。

### 1.2 Azure Trusted Signing (Artifact Signing) の適格性

**確認できた内容 (複数の Microsoft Q&A スレッドおよび公式ブログ記事で一致)**:

- 組織として Public Trust 証明書を取得するには、**3 年以上の検証可能な事業
  実績**が必要 (2025 年 4 月時点の Q&A で、3 年未満の米国 LLC が Public
  Trust の組織確認を通せなかった事例が確認できる)。
- 個人開発者向けの申請も用意されているが、**米国・カナダ在住者に限定**
  されており (2025 年 4 月時点)、政府発行の写真付き身分証明書 + 生体認証
  (セルフィー) による本人確認が必要。
- サービス名は 2024〜2025 年ごろに Trusted Signing → **Artifact Signing**
  へ改称されているが機能的には同一 (公式 GitHub リポジトリ
  `azure/artifact-signing-action` の README で確認済み)。

→ このリポジトリの開発者 (個人、日本在住) は現時点の適格性要件を満たさない
可能性が高く、**証明書の取得自体がすぐには行えない**。組織として法人化し
3 年以上の実績を積むか、対象国が拡大されるのを待つか、あるいは選択肢 2 の
他ベンダーのクラウド署名サービス (国・地域の制約が異なる場合がある) を検討
する必要がある。

### 1.3 SmartScreen のレピュテーション (評価) 蓄積

**確認できた内容 (複数の独立したソースで一致)**:

- 2024 年 3 月頃の Microsoft Trusted Root Program の変更により、**EV 証明書
  による SmartScreen の即時バイパス (以前の挙動) は廃止**された。現在は
  EV も OV も同じ土俵でレピュテーションを積む。
- レピュテーションは主に**ダウンロード数・インストール成功率・ユーザの
  操作・発行者の実績**などから Microsoft 側で内部的に蓄積される。新しい
  実行ファイル (特にリリース初期) は「未知の発行者」として SmartScreen の
  警告 (「Windows によって PC が保護されました」) が出る可能性が高い。
- **証明書を更新すると発行者 ID が変わり、レピュテーションはゼロから
  積み直しになる**。運用上、証明書のローテーションを頻繁に行うと逆効果
  になり得る。

→ つまり署名を導入しても「即座に警告が消える」わけではなく、**継続的な
配布実績が必要**という点を利用者への説明 (README やリリースノート) に
含めておくべきである。今回のドキュメントにその旨を明記した。

### 1.4 signtool.exe の基本構文 (参考: 従来型 CA ルートの場合)

Azure Trusted Signing / クラウド HSM 型サービスを使わず、選択肢 1 (自前の
ハードウェアトークンをセルフホストランナーに挿す) を選ぶ場合に使う
`signtool.exe` (Windows SDK 同梱) の基本形は次のとおり (複数の独立した
技術記事で一致する内容):

```powershell
signtool sign /fd SHA256 /tr http://timestamp.digicert.com /td SHA256 /a path\to\velox.exe
```

- `/fd SHA256` — ファイルのダイジェストアルゴリズム。
- `/tr <URL>` / `/td SHA256` — RFC 3161 準拠のタイムスタンプサーバーと、
  そのダイジェストアルゴリズム。タイムスタンプを付けないと、証明書の
  有効期限切れ後に署名自体が無効化されてしまうため必須。
- `/a` — 対象ファイルに適合する証明書をストアから自動選択。

このリポジトリの workflow では Azure Trusted Signing の公式 Action が内部で
署名処理を行うため `signtool.exe` を直接は呼んでいないが、選択肢 2/1 に
切り替える場合の参考として記録する。

### 1.5 GitHub Actions での秘密情報の扱い

**確認できた内容 (GitHub 上の複数の実装例で一致するパターン)**:

- 秘密鍵をエクスポートできない現在のコード署名証明書の性質上、
  「`.pfx` を base64 にして Secrets に入れる」パターンは**新規発行の証明書
  では使えない**(1.1 参照)。2023 年 6 月より前に発行された証明書がまだ
  有効な場合や、一部のベンダーがレガシー互換で発行するケースに限り成立する
  legacy な方法として言及するに留める。
- Azure Trusted Signing のような API 型サービスを使う場合、GitHub Actions
  側は **OIDC (OpenID Connect) によるフェデレーション認証**
  (`azure/login` + `permissions: id-token: write`) を使い、長期の
  クライアントシークレットや証明書そのものを Secrets に保存せずに済む
  設計が可能 (公式 README の推奨構成)。このリポジトリの実装もこの方式を
  採用した。
- いずれの方式でも、**証明書・秘密鍵そのものやそれに準ずる長期認証情報を
  リポジトリのコード/コミット履歴に置かない**のは大前提であり、Secrets /
  Variables はすべて GitHub リポジトリ設定側で管理する。

## 2. macOS: Developer ID 署名と notarization (概要のみ)

CLAUDE.md の OS 優先度方針により macOS は現時点で release workflow 自体が
存在しない (docs/decisions.md D70)。そのため署名の実装は行わず、将来
macOS の release workflow を追加する際に参照できるよう、概要のみ記録する。

- **証明書**: Apple Developer Program (年会費 $99) に登録し、"Developer ID
  Application" 証明書を発行してもらう。この証明書で `.app`/バイナリに
  `codesign` コマンドで署名する。
- **notarization (公証)**: 署名した成果物を Apple の公証サービスに送り
  (`xcrun notarytool submit`)、マルウェアスキャンを通過させる。認証には
  App Store Connect API キー、または Apple ID + アプリ用パスワードを使う。
- **stapling**: 公証チケットを成果物に添付する (`xcrun stapler staple`)。
  これによりオフライン環境でも Gatekeeper が公証済みと判定できる。
- **GitHub Actions での実装**: Apple 公式の GitHub Action は無く、
  コミュニティ製 (`indygreg/apple-code-sign-action` など、OSS の
  `rcodesign` を使うものを含む) を使うのが一般的なパターンとして複数の
  リポジトリで確認できた。証明書は `.p12` を base64 化して Secrets に
  格納する運用例が多い (macOS 側は Windows のような 2023 年の
  ハードウェア必須化の対象ではない)。

macOS 対応は、README ロードマップと D70 の方針どおり「3 OS の品質が
担保された段階」で release workflow 自体の新設と合わせて着手する。

## 3. 証明書が無くても今回実施した改善

1. **release-windows.yml に署名ステップを追加**(実装内容は
   `.github/workflows/release-windows.yml` のコメント参照)。以下の 6 つの
   Secrets/Variables が全て設定されている場合のみ動作し、1 つでも欠けて
   いれば従来どおり無署名でビルドを継続する。
   - `secrets.WINDOWS_CODESIGN_AZURE_CLIENT_ID`
   - `secrets.WINDOWS_CODESIGN_AZURE_TENANT_ID`
   - `secrets.WINDOWS_CODESIGN_AZURE_SUBSCRIPTION_ID`
   - `vars.WINDOWS_CODESIGN_ENDPOINT`
   - `vars.WINDOWS_CODESIGN_ACCOUNT_NAME`
   - `vars.WINDOWS_CODESIGN_CERT_PROFILE_NAME`
2. **GitHub Release 本文に署名状態を明記** — 署名済みなら
   「Authenticode-signed」、未署名なら「verify the SHA-256 checksum before
   running it」という注記を自動で本文に追加する。
3. **README にチェックサム検証手順を追記** (証明書の有無に関わらず今すぐ
   使える改善)。

## 4. 証明書を取得した後に行う作業

1. Azure Portal で **Trusted Signing (Artifact Signing) アカウント**と
   **証明書プロファイル**を作成し (1.2 の適格性要件を満たすことが前提)、
   エンドポイント URL・アカウント名・証明書プロファイル名を控える。
2. Microsoft Entra ID (旧 Azure AD) にアプリ登録を作成し、GitHub Actions
   からの OIDC フェデレーション資格情報 (発行者
   `https://token.actions.githubusercontent.com`、対象はこのリポジトリの
   `release-windows.yml` の実行コンテキスト) を設定する。そのアプリの
   署名担当ロール (Trusted Signing Certificate Profile Signer 相当) を
   付与する。
3. GitHub リポジトリの Settings → Secrets and variables → Actions で、
   上記 3.1 節の 3 つの Secrets (client-id / tenant-id /
   subscription-id) と 3 つの Variables (endpoint / account name /
   certificate profile name) を設定する。
4. `workflow_dispatch` で `release-windows.yml` を手動実行し、Actions の
   ログで「Windows code signing is configured」と署名ステップの成功、
   および `Verify Authenticode signature` ステップが `Valid` と表示する
   ことを確認する。
5. 実際に `v*` タグを push して GitHub Release を作成し、生成された
   `velox.exe` を Windows 実機にダウンロードして SmartScreen の挙動
   (署名は付くが、レピュテーションが無い間は依然として警告が出得る点は
   1.3 を参照) を確認する。
6. 問題なければ docs/decisions.md に Revisit として記録し、README の
   説明 (「現状は無署名」の記述) を更新する。

## 5. 未検証・今後の課題

- Azure Trusted Signing (Artifact Signing) の実際の申請・適格性審査は
  行っていない (組織の 3 年要件、個人の国・地域要件のいずれも現時点では
  満たせないため)。
- `azure/login` + `azure/artifact-signing-action` の組み合わせは、証明書
  が無い現状では `enabled=false` のため一度も実行されておらず、**実際に
  Azure 側の設定と繋げた動作確認はできていない**。
- macOS の署名・notarization は概要調査のみで、実装 (workflow・
  `codesign`/`notarytool` コマンドの実行) は行っていない。
- SmartScreen のレピュテーション蓄積が実際にどの程度の期間・ダウンロード数
  で効いてくるかは Microsoft 側で明文化された定量的基準が無く (検索した
  範囲では確認できなかった)、実運用で様子を見るほかない。
