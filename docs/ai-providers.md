# AI プロバイダ抽象

Issue #76 (Epic #73)。#77 (AI Omnibox) と #78 (AI Page Actions) が乗る土台の
設計。実装は `src/browser/ai/` (純粋ロジックのみ・**app には未配線**)。
判断の記録は docs/decisions/archive.md の D162。

## 目標 / 非目標

目標:

- プロバイダを差し替えられる (レジストリ + trait)。
- UI (toolbar / app) は特定プロバイダの API・型に依存しない。
- タイムアウト・キャンセル・エラーを共通の形で扱う。
- 資格情報がログ・`Debug`・エラー文言・設定ファイルに出ない。
- AI を使わない限りコストゼロ (Issue #76 コメントのトリアージ): プロバイダは
  選択されるまで生成せず、スレッドも channel も持たない。

非目標:

- **標準で有効なプロバイダは同梱しない。** レジストリは空で始まり、ユーザが
  明示的に設定するまで**端末外へデータは出ない**。VeloX のプライバシー方針
  (既定でテレメトリなし) と同じ立場。
- **ページ内容をプロバイダへ送ってよいか**は #78 の決定事項で、ここでは決めない。
  この層は `Remote` プロバイダに何が渡るかを知らない (だから
  `ProviderKind::Remote` を UI が表示できる形で公開している)。
- 実プロバイダ (HTTP クライアント) の実装。依存を増やさない (D6) ため、
  これは別 Issue で HTTP 依存の是非と併せて決める。
- OS キーチェーン連携 (後述)。

## 構成

```
UI (toolbar IPC) ──UserEvent──> app.rs ──dispatch()──> worker thread
      ^                            |                      AiProvider::complete
      └──── UserEvent::AiEvent ────┘<── supervisor thread <── mpsc ─┘
```

### 同期 trait + スレッド + channel を選んだ理由

`app.rs` はメインスレッドの `EventLoop<UserEvent>` に全状態変更を集約し、
ロックを使わない。したがってプロバイダ呼び出しはメインスレッドで
ブロックできず、かつ非同期ランタイムは依存追加になる (D6 に反する)。

- `AiProvider::complete` は**同期**で、ストリームは `on_chunk` コールバックで返す。
  実装は素直なブロッキング I/O で書ける。
- 実行は [`dispatch`] が専用ワーカースレッドで行う。結果は `std::sync::mpsc` で
  監督スレッドへ、そこから外部 channel (`AiHandle::events`) へ流れる。
- 外部 channel に入れるたびに `notify` を呼ぶ。app 側はここで
  `EventLoopProxy::send_event(UserEvent::AiEvent(..))` を送り、メインスレッドが
  `events.try_recv()` で取り出す (既存の他の UserEvent と同じ流儀)。状態は
  メインスレッドだけが持つ。
- AI 未使用時はスレッドが 1 本も立たない。

### 要求・応答モデル

- `AiRequest { messages: Vec<Message>, config: ModelConfig }`。
  `Message { role: System|User|Assistant, content }`。
- `ModelConfig { model, max_tokens, temperature: Option<f32> }`。`model` は
  プロバイダ固有の名前を運ぶ不透明な文字列で、UI は解釈しない。
- 応答は `AiEvent { request_id, kind }`。`kind` は
  `Chunk(String)` (0 回以上) → 終端 `Finished(FinishReason)` または
  `Failed(AiError)` (**ちょうど 1 回**、その後は何も来ない)。`request_id` は
  古い要求のイベントを UI が捨てるための印。

### キャンセル

`CancelToken` (`Arc<AtomicBool>`) をプロバイダに渡し、長い処理の合間に確認させる。
`AiHandle::cancel()` はトークンを立て、監督スレッドを内部 channel への合図で
即座に起こす (ポーリングなし)。UI には `Failed(Cancelled)` が 1 回届き、
以降のチャンクは捨てられる。

### タイムアウト

期限は **dispatcher が強制する** (`recv_timeout`)。プロバイダが期限を守る、
あるいはキャンセルに応じることには頼らない。期限が来たら `Failed(Timeout)` を
流し、トークンも立てる。トークンを無視するプロバイダのスレッドは止められない
(Rust に安全なスレッド強制終了は無い) が、UI は待たされず、結果は捨てられる。
実プロバイダは HTTP のタイムアウトも自前で設定してスレッド滞留を減らすこと。

### エラー分類 (`AiError`)

`Cancelled` / `Timeout` / `Auth` / `RateLimited` / `Network(String)` /
`InvalidRequest(String)` / `Unavailable(String)` / `Provider(String)`。
UI はこの分類だけで表示・再試行可否を決める。プロバイダの panic は
`catch_unwind` で `Provider` に変換する。`Display` に資格情報や要求本文を
入れてはならない。

### レジストリと選択

`ProviderRegistry` が ID → `Arc<dyn AiProvider>` を持ち、`select(id)` で 1 つを
選ぶ。未選択・未知の ID は `Unavailable`。呼び出し側は `active()` を
`dispatch` に渡すだけで、差し替えで変わるのは登録内容だけ。
`ProviderKind::{Local, Remote}` で「データが端末外へ出るか」を UI に示せる。
ローカル (例: 端末上のモデルサーバ) もリモートも同じ trait で扱う。

## 資格情報

- `Secret` 型は `Debug` / `Display` が常に `[REDACTED]`。`Serialize` /
  `Deserialize` を**実装しない**ので、`settings.json` に平文で書く経路が型で塞がる。
  平文は `Secret::expose()` を呼んだ箇所 (プロバイダが認証ヘッダを組む所) のみ。
- 保管方法: **当面は環境変数 (`Secret::from_env`) のみ、または資格情報なし**
  (ローカルプロバイダ)。設定ファイルに保存しない。
- OS キーチェーン (Windows 資格情報マネージャ等) は将来課題。OS ごとの API と
  依存の追加が要り、Windows 優先の方針でも実プロバイダが無い今は費用に見合わない。
  実プロバイダの Issue で改めて決める。それまでは「環境変数のみ」が最も
  漏れ面が小さく、ディスクに何も残らない。
- 設定に載せるのは環境変数の**名前**であって値ではない。ログ・ベンチ結果・
  メトリクスにも値を出さない。

## UI との接続 (将来の配線)

- toolbar → app: 既存の IPC と同様に、`{ "type": "ai_request", ... }` を
  `UserEvent` に変換。プロバイダ ID とモデル名は文字列で、固有型は持たない。
- app → toolbar: `UserEvent::AiEvent` を受けて `AiEventKind` を JS へ渡す。
  toolbar が知るのは `Chunk` / `Finished` / `Failed(分類)` だけ。
- プロバイダの生成は初回利用時に遅延させる。

## テスト

`src/browser/ai/` の単体テストが、差し替え (レジストリ)、ストリーム、
mid-stream キャンセル、期限 (キャンセル無視のプロバイダを含む)、エラー伝搬、
panic、資格情報の伏せ字を検証する。`EchoProvider` (決定的・ローカル)、
`FailingProvider`、`StallProvider` はテスト/デモ用で、外部通信をしない。
