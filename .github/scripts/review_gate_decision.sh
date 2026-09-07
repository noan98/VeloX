#!/usr/bin/env bash
# Issue #188: PR のレビュー状態 (未解決スレッド / CHANGES_REQUESTED /
# Codex の head SHA レビュー / head commit の猶予期間) からマージしてよいか
# を判定し、理由を標準出力に1行ずつ出す。auto-merge.yml の本番マージ判定
# ジョブと、workflow 自身を変更する PR で走る dry-run 検証ジョブの両方から
# 呼ぶ共通ロジック。判定ロジック本体は check_review_gate.py に切り出して
# あり単体テストがある (test_check_review_gate.py。docs/decisions.md D91
# 参照)。
#
# 使い方: review_gate_decision.sh <PR番号> <head_sha> [codex_bypass]
#   codex_bypass - "true" なら Codex レビュー必須判定だけを免除する
#                   (`automerge-without-codex` ラベル用)。省略時は "false"。
#                   未解決スレッド判定・CHANGES_REQUESTED 判定は免除しない。
#
# 前提の環境変数:
#   GH_REPO               - "owner/repo" (gh CLI が要求)
#   GH_TOKEN               - gh CLI の認証トークン
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
# 仕様 (未検証、docs/decisions.md D91 参照) の代替シグナル用。
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
          author { login }
          commit { oid }
        }
      }
      reactions(content: THUMBS_UP, first: 100) {
        pageInfo { hasNextPage }
        nodes {
          createdAt
          user { login }
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

# head commit の committer date (猶予期間の起点、および Codex の 👍
# リアクション判定の基準時刻)。取得に失敗しても致命的にはせず、
# check_review_gate.py 側の「committed_date が無ければ安全側でブロック」に
# 判定を委ねる (原因は stderr の警告で分かるようにする)。
committed_date=""
if ! committed_date=$(gh api "repos/${GH_REPO}/commits/${sha}" --jq '.commit.committer.date' 2>&1); then
  echo "::warning::PR #${number} の head commit (${sha}) の committer date 取得に失敗しました: ${committed_date}" >&2
  committed_date=""
fi

now=$(date -u +"%Y-%m-%dT%H:%M:%SZ")

payload=$(jq -n \
  --argjson pr "$pr_data" \
  --arg committed "$committed_date" \
  --arg now "$now" \
  --argjson grace "$grace" \
  --arg sha "$sha" \
  --arg codexBypass "$codex_bypass" \
  '{
    reviewThreads: $pr.reviewThreads,
    latestReviews: $pr.latestReviews,
    reactions: $pr.reactions,
    headSha: $sha,
    headCommittedDate: (if ($committed | length) > 0 then $committed else null end),
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
