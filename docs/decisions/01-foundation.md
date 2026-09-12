# Foundation Decisions

基本アーキテクチャと、後続の設計判断が前提として参照する Decision 群です。

## 対象

主担当は **D1–D7** の 7 件 (網羅的な一覧は下の「詳細」節。ここは読み進める手がかりとしての要約)。

- **D1** — Web engine: wry / system webviews
- **D2** — Window / event loop: tao
- **D3** — Browser chrome: toolbar webview
- **D4** — Back / Forward: engine session history
- **D5** — State ownership: event-loop messages / single-threaded mutation
- **D6** — Dependency policy
- **D7** — Address-bar input handling

## 関係する後続判断

D8 以降のタブ管理、D14–D15 の private browsing、D17 の content blocking、D18/D23 の IPC trust boundary などは、ここで定めた webview 分離・依存関係・入力処理の方針を前提にしています。

### 詳細

**このカテゴリが主担当の Decision: D1–D7** (7 件)

`archive.md` の全 Decision は、いずれか 1 つのカテゴリが主担当として必ず
この一覧に載る。複数カテゴリにまたがるものは主担当だけに載せ、必要なら
本文から相互参照する (README の「重要なルール」3)。

⚠️ **この節は `generate_decision_index.py` が生成する。手で編集しない。**
`archive.md` との対応は `test_decision_index.py` が CI で検査するので、
追記漏れも、見出しを変えたことによるリンク切れも、そこで落ちる。

- [D1](./archive.md#d1-web-engine--wry-system-webviews-not-servo) — Web engine — wry (system webviews), not Servo
- [D2](./archive.md#d2-windowevent-loop--tao-not-winit) — Window/event loop — tao, not winit
- [D3](./archive.md#d3-browser-chrome-as-an-html-toolbar-in-a-second-webview) — Browser chrome as an HTML toolbar in a second webview
- [D4](./archive.md#d4-backforward-via-the-engines-session-history) — Back/Forward via the engine's session history
- [D5](./archive.md#d5-single-threaded-state-via-event-loop-messages) — Single-threaded state via event loop messages
- [D6](./archive.md#d6-dependency-policy) — Dependency policy
- [D7](./archive.md#d7-address-bar-input-handling) — Address-bar input handling

---

- [設計判断アーカイブ (全文)](./archive.md)
