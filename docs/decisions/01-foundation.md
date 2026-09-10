# Foundation Decisions

基本アーキテクチャと、後続の設計判断が前提として参照する Decision 群です。

## 対象

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

- [D1–D7 全文](./archive.md#d1-web-engine--wry-system-webviews-not-servo)
- [設計判断アーカイブ](./archive.md)
