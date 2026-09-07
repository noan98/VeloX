#!/usr/bin/env bash
# Issue #188: PR のレビュー状態 (未解決スレッド / CHANGES_REQUESTED /
# Codex の head SHA レビュー / 猶予期間) からマージしてよいかを判定し、
# 理由を標準出力に1行ずつ出す。auto-merge.yml の本番マージ判定ジョブと、
# workflow 自身を変更する PR で走る dry-run 検証ジョブの両方から呼ぶ
# 共通ロジック。判定ロジック本体は check_review_gate.py に切り出してあり
# 単体テストがある (test_check_review_gate.py。docs/decisions.md D91 参照)。
#
# 使い方: review_gate_decision.sh <PR番号> <head_sha> [codex_bypass]
#   codex_bypass - "true" なら Codex レビュー必須判定だけを免除する
#                   (`automerge-without-codex` ラベル用)。省略時は "false"。
#                   未解決スレッド判定・CHANGES_REQUESTED 判定は免除しない。
#
# 前提の環境変数:
#   GH_REPO               - "owner/repo" (gh CLI が要求)
#   GH_TOKEN               - gh CLI の認証トークン (check-suites 取得に
#                             `checks: read` 権限が要る)
#   GRACE_PERIOD_MINUTES   - 猶予期間 (分)。未設定なら 15。
#
# 標準出力: 判定理由を1行ずつ (問題が無ければ何も出さない)。「今このPRは
#   何を待っているのか」が一目で分かる文言にしてある (Issue #168 の教訓)。
# 標準エラー出力: API 呼び出し自体が失敗した場合の生エラー (握りつぶさない)。
# 終了コード:
#   0 - ブロックする理由なし (マージしてよい)
#   1 - ブロックする理由がある (未解決スレッド/CHANGES_REQUESTED/
#       Codex レビュー待ち/猶予期間)
#   2 - API 呼び出し自体に失敗した (安全側としてブロック扱いだが原因が別)
set -euo pipefail

if [ "$#" -lt 2 ] || [ "$#" -gt 3 ]; then
  echo "usage: review_gate_decision.sh <PR番号> <head_sha> [codex_bypass]" >&2
  exit 2
fi

number="$1"
sha="$2"
codex_bypass="${3:-false}"
scripts_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

owner="${GH_REPO%%/*}"
repo="${GH_REPO##*/}"
grace="${GRACE_PERIOD_MINUTES:-15}"

# reviewThreads: 未解決かどうかと、代表コメント (先頭1件) のファイル/投稿者。
# latestReviews: レビュアーごとの最新 (submitted) レビュー状態。PENDING の
# ドラフトレビューはここには含まれない。commit.oid と body は Issue #188
# の Codex 必須判定 (現在の head SHA をレビュー済みか) に使う。
# reactions(content: THUMBS_UP): Codex が指摘ゼロのとき 👍 のみを付ける
# 仕様 (PR #191 で実測済み、docs/decisions.md D91 参照) の代替シグナル用。
# author/user の __typename: ログイン名の完全一致に加えて Bot であることも
# 確認するための追加シグナル (2026-09-07 の Codex レビュー指摘、PR #192)。
read -r -d '' query <<'GRAPHQL' || true
query($owner: String!, $repo: String!, $number: Int!) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $number) {
      reviewThreads(first: 100) {
        pageInfo { hasNextPage }
        nodes {
          isResolved
          comments(first: 1) {
            nodes { path author { login } }
          }
        }
      }
      latestReviews(first: 100) {
        pageInfo { hasNextPage }
        nodes {
          state
          body
          author { login __typename }
          commit { oid }
        }
      }
      reactions(content: THUMBS_UP, first: 100) {
        pageInfo { hasNextPage }
        nodes {
          createdAt
          user { login __typename }
        }
      }
    }
  }
}
GRAPHQL

# 権限不足 (pull-requests: read が無い等) の場合ここが失敗する。Issue #168
# では権限不足による 404 が「Issue が見つからない」という誤解を招くメッセージ
# になり原因がログから読めなかった。同じ失敗を繰り返さないよう、gh の生の
# エラー出力を stderr の警告に含める (握りつぶさない)。
if ! gql=$(gh api graphql -f query="$query" -F owner="$owner" -F repo="$repo" -F number="$number" 2>&1); then
  echo "::warning::PR #${number} のレビュー情報取得 (GraphQL) に失敗しました。権限不足 (pull-requests: read) の可能性があります。安全側でマージを見送ります: ${gql}" >&2
  echo "wait: レビュー情報の取得 (GraphQL) に失敗したため安全側でスキップします"
  exit 2
fi

pr_data=$(echo "$gql" | jq '.data.repository.pullRequest')
if [ "$pr_data" = "null" ] || [ -z "$pr_data" ]; then
  echo "::warning::PR #${number} の GraphQL 応答に pullRequest が含まれていません: ${gql}" >&2
  echo "wait: レビュー情報の取得に失敗したため安全側でスキップします"
  exit 2
fi

# --- head SHA の push 観測時刻 (猶予期間の起点、および Codex の 👍
# リアクション判定の基準時刻) --------------------------------------------
# ⚠️ git commit の committer date は使わない。committer date は「commit を
# ローカルで作った時刻」であり、ローカルで数時間前に作った commit を今
# push する・cherry-pick/rebase で古い commit を持ち込む、といった普通の
# 操作で容易に過去の日時になる。これを使うと (a) 以前の head に付いた
# Codex の 👍 の createdAt が新しい (実は古い) committer date より後に
# なり誤って「レビュー済み」と判定されてしまう、(b) 猶予期間も同時に
# 即座に満たされてしまう — という2つの防御が同一の操作可能なタイムスタンプ
# に依存して同時に破られる欠陥があった (2026-09-07 の Codex レビュー指摘、
# PR #192。docs/decisions.md D91 に詳細)。
#
# 代わりに、GitHub がサーバ側で観測した時刻として、head SHA に対する
# check-suite の作成時刻の最小値を使う (push を受けて GitHub 自身が
# 作成するものなので attacker が直接操作できない)。取得できなかった場合
# (check-suite が1件も無い等) は committer date へのフォールバックはせず
# 安全側でブロックする (check_review_gate.py 側の
# 「head_push_observed_at が無ければ安全側でブロック」に判定を委ねる)。
push_observed_at=""
if ! check_suites_raw=$(gh api "repos/${GH_REPO}/commits/${sha}/check-suites" \
       --jq '[.check_suites[].created_at] | sort | .[0] // empty' 2>&1); then
  echo "::warning::PR #${number} の head commit (${sha}) の check-suites (push観測時刻の代替) 取得に失敗しました: ${check_suites_raw}" >&2
  push_observed_at=""
else
  push_observed_at="$check_suites_raw"
  if [ -z "$push_observed_at" ]; then
    echo "::warning::PR #${number} の head commit (${sha}) に check-suite が1件も見つかりませんでした (push観測時刻を決定できません)" >&2
  fi
fi

now=$(date -u +"%Y-%m-%dT%H:%M:%SZ")

payload=$(jq -n \
  --argjson pr "$pr_data" \
  --arg pushObservedAt "$push_observed_at" \
  --arg now "$now" \
  --argjson grace "$grace" \
  --arg sha "$sha" \
  --arg codexBypass "$codex_bypass" \
  '{
    reviewThreads: $pr.reviewThreads,
    latestReviews: $pr.latestReviews,
    reactions: $pr.reactions,
    headSha: $sha,
    headPushObservedAt: (if ($pushObservedAt | length) > 0 then $pushObservedAt else null end),
    now: $now,
    gracePeriodMinutes: $grace,
    codexBypass: ($codexBypass == "true")
  }')

result=$(echo "$payload" | python3 "${scripts_dir}/check_review_gate.py")
blocked=$(echo "$result" | jq -r '.blocked')
echo "$result" | jq -r '.reasons[]'

if [ "$blocked" = "true" ]; then
  exit 1
fi
exit 0
