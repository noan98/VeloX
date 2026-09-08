#!/usr/bin/env bash
# review_gate_decision.sh のローカル統合テスト。
#
# `gh` コマンドをこのスクリプト専用の一時ディレクトリに置いたスタブに
# 差し替え (PATH の先頭に挿す)、実際に review_gate_decision.sh を
# 起動して stdout/exit code を検証する。check_review_gate.py 側の判定
# ロジックは test_check_review_gate.py (純粋な Python 単体テスト) で
# 網羅しているため、ここでは shell ラッパー固有の挙動 — 特に
# CLAUDE_REVIEWER_LOGINS の CSV → JSON 変換が空文字列/空白/カンマのみ
# でクラッシュしないこと (2026-09-07、PR #192 の dry-run で実際に
# `jq: invalid JSON text passed to --argjson` で落ちたバグの回帰テスト。
# docs/decisions.md D91 参照) — を確認する。
#
# 実行方法:
#   bash .github/scripts/test_review_gate_decision.sh
#
# 終了コード: 0 = 全件 pass、1 = 1件以上 fail。
set -uo pipefail

scripts_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
target="${scripts_dir}/review_gate_decision.sh"

tmp_root="$(mktemp -d)"
trap 'rm -rf "$tmp_root"' EXIT

pass_count=0
fail_count=0

_fail() {
  echo "FAIL: $1"
  fail_count=$((fail_count + 1))
}

_pass() {
  echo "ok: $1"
  pass_count=$((pass_count + 1))
}

# 標準の GraphQL レスポンス (未解決スレッド無し・レビュー無し・上限
# メッセージ無し・push観測時刻は十分過去) を書き出す共通スタブ。
_write_stub_gh() {
  local dir="$1"
  cat > "${dir}/gh" <<'STUB'
#!/usr/bin/env bash
if [ "$1" = "api" ] && [ "$2" = "graphql" ]; then
  cat <<'JSON'
{
  "data": {
    "repository": {
      "pullRequest": {
        "title": "スタブPR",
        "reviewThreads": {"pageInfo": {"hasNextPage": false}, "nodes": []},
        "latestReviews": {"pageInfo": {"hasNextPage": false}, "nodes": []},
        "reactions": {"pageInfo": {"hasNextPage": false}, "nodes": []},
        "comments": {"nodes": []}
      }
    }
  }
}
JSON
  exit 0
elif [ "$1" = "api" ] && [[ "$2" == *check-suites* ]]; then
  date -u -d '2 hours ago' +"%Y-%m-%dT%H:%M:%SZ"
  exit 0
elif [ "$1" = "pr" ] && [ "$2" = "comment" ]; then
  cat >/dev/null
  exit 0
fi
echo "unhandled: $*" >&2
exit 1
STUB
  chmod +x "${dir}/gh"
}

_run_with_claude_logins() {
  # 引数1: CLAUDE_REVIEWER_LOGINS の値 (未設定を表すには特殊値 __UNSET__)
  local value="$1"
  local dir="${tmp_root}/$(mktemp -u XXXXXX)"
  mkdir -p "$dir"
  _write_stub_gh "$dir"
  local out rc
  # codex_bypass=true (第3引数) にして条件3 (Codex 必須判定) を無効化し、
  # CLAUDE_REVIEWER_LOGINS の CSV 解析バグそのものを条件1/2/4だけの単純
  # なケースで切り分ける (この統合テストの主眼はクラッシュしないことの
  # 確認であり、Codex 判定ロジック自体は test_check_review_gate.py が
  # 別途網羅している)。
  if [ "$value" = "__UNSET__" ]; then
    out=$(PATH="${dir}:${PATH}" GH_REPO="owner/repo" GH_TOKEN="dummy" \
          GRACE_PERIOD_MINUTES="15" \
          bash "$target" 1 aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa true false 2>&1)
    rc=$?
  else
    out=$(PATH="${dir}:${PATH}" GH_REPO="owner/repo" GH_TOKEN="dummy" \
          GRACE_PERIOD_MINUTES="15" CLAUDE_REVIEWER_LOGINS="$value" \
          bash "$target" 1 aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa true false 2>&1)
    rc=$?
  fi
  printf '%s\x1e%s' "$out" "$rc"
}

# --- CLAUDE_REVIEWER_LOGINS: 未設定/空/空白のみ/カンマのみ ---------------
# いずれも「クラッシュせず (jq エラーが出ず)、安全側 (exit 0、指摘ゼロで
# 猶予期間も経過済みなのでブロック理由が無い) として扱われる」ことを
# 確認する。exit 1/2 になったり `jq: invalid JSON` が出力に含まれたりし
# たら fail。
for case_name_value in \
    "unset:__UNSET__" \
    "empty:" \
    "whitespace:   " \
    "commas_only:,,"; do
  case_name="${case_name_value%%:*}"
  value="${case_name_value#*:}"
  result=$(_run_with_claude_logins "$value")
  out="${result%$'\x1e'*}"
  rc="${result##*$'\x1e'}"

  if echo "$out" | grep -qi "invalid JSON"; then
    _fail "CLAUDE_REVIEWER_LOGINS=${case_name}: 出力に 'invalid JSON' が含まれる (jq クラッシュ再発)。出力: ${out}"
    continue
  fi
  if [ "$rc" != "0" ]; then
    _fail "CLAUDE_REVIEWER_LOGINS=${case_name}: 指摘ゼロ・猶予期間経過済みのはずが exit ${rc} (期待値 0)。出力: ${out}"
    continue
  fi
  _pass "CLAUDE_REVIEWER_LOGINS=${case_name}: クラッシュせず exit 0 (空配列として安全に扱われた)"
done

# --- 対照: 実際に値が入っている場合は壊れていないことも確認する ---------
result=$(_run_with_claude_logins "claude[bot], other-bot")
out="${result%$'\x1e'*}"
rc="${result##*$'\x1e'}"
if echo "$out" | grep -qi "invalid JSON"; then
  _fail "CLAUDE_REVIEWER_LOGINS に実値がある場合でも 'invalid JSON' が出た。出力: ${out}"
elif [ "$rc" != "0" ]; then
  _fail "CLAUDE_REVIEWER_LOGINS に実値がある正常系で exit ${rc} (期待値 0)。出力: ${out}"
else
  _pass "CLAUDE_REVIEWER_LOGINS に実値 (前後空白・複数件) がある場合も正常に動作する"
fi

# =========================================================================
# Issue #201: `@codex review` の自動投稿は `@claude` と同じく
# AUTO_MERGE_TOKEN を明示的に使い、未設定なら投稿しない (マーカーも
# 残さない) こと。設定時は実際に AUTO_MERGE_TOKEN で投稿されること。
# あわせて `@claude` 側の既存挙動が壊れていないことも回帰テストする。
# =========================================================================

# GraphQL の応答をそのまま返し、`gh pr comment` の呼び出しを
# `<dir>/pr_comment.log` に "GH_TOKEN=<値><TAB>本文1行目" として記録する
# スタブを書き出す。呼び出しが無ければログファイル自体が作られない
# (「投稿しなかったこと」を「ファイルが存在しない」で確認できるように
# するため)。
_write_stub_gh_logging() {
  local dir="$1"
  local graphql_json="$2"
  printf '%s' "$graphql_json" > "${dir}/graphql_response.json"
  cat > "${dir}/gh" <<'STUB'
#!/usr/bin/env bash
self_dir="$(cd "$(dirname "$0")" && pwd)"
log="${self_dir}/pr_comment.log"
if [ "$1" = "api" ] && [ "$2" = "graphql" ]; then
  cat "${self_dir}/graphql_response.json"
  exit 0
elif [ "$1" = "api" ] && [[ "$2" == *check-suites* ]]; then
  date -u -d '2 hours ago' +"%Y-%m-%dT%H:%M:%SZ"
  exit 0
elif [ "$1" = "pr" ] && [ "$2" = "comment" ]; then
  body="$(cat)"
  printf 'GH_TOKEN=%s\t%s\n' "${GH_TOKEN:-}" "$(printf '%s' "$body" | head -1)" >> "$log"
  exit 0
fi
echo "unhandled: $*" >&2
exit 1
STUB
  chmod +x "${dir}/gh"
}

# Codex がこの head SHA (40 桁の 'b') をまだ一度もレビューしていない状態
# (`codex_review_request_needed=true` になる) を再現する GraphQL 応答。
# `printf 'b%.0s' {1..40}` で確実に 40 文字ちょうどにする (マーカー正規表現
# `[0-9a-fA-F]{40}` は桁数が合わないと一致しないため、決め打ちの文字列
# リテラルで桁数を数え間違えないようにする)。
_codex_head_sha="$(printf 'b%.0s' {1..40})"
read -r -d '' _graphql_codex_pending <<JSON || true
{
  "data": {
    "repository": {
      "pullRequest": {
        "title": "スタブPR (Codex未レビュー)",
        "reviewThreads": {"pageInfo": {"hasNextPage": false}, "nodes": []},
        "latestReviews": {"pageInfo": {"hasNextPage": false}, "nodes": []},
        "reactions": {"pageInfo": {"hasNextPage": false}, "nodes": []},
        "comments": {"nodes": []}
      }
    }
  }
}
JSON

_run_codex_pending() {
  # 引数1: AUTO_MERGE_TOKEN の値 (未設定を表すには特殊値 __UNSET__)
  local token="$1"
  local dir="${tmp_root}/$(mktemp -u XXXXXX)"
  mkdir -p "$dir"
  _write_stub_gh_logging "$dir" "$_graphql_codex_pending"
  local out rc
  # codex_bypass=false (第3引数、条件3を有効にする) / allow_codex_
  # request_post=true (第4引数、本番相当で実際に投稿を試みる)。
  if [ "$token" = "__UNSET__" ]; then
    out=$(PATH="${dir}:${PATH}" GH_REPO="owner/repo" GH_TOKEN="dummy" \
          GRACE_PERIOD_MINUTES="15" \
          bash "$target" 1 "$_codex_head_sha" false true 2>&1)
    rc=$?
  else
    out=$(PATH="${dir}:${PATH}" GH_REPO="owner/repo" GH_TOKEN="dummy" \
          GRACE_PERIOD_MINUTES="15" AUTO_MERGE_TOKEN="$token" \
          bash "$target" 1 "$_codex_head_sha" false true 2>&1)
    rc=$?
  fi
  local log_content=""
  if [ -f "${dir}/pr_comment.log" ]; then
    log_content="$(cat "${dir}/pr_comment.log")"
  fi
  printf '%s\x1e%s\x1e%s' "$out" "$log_content" "$rc"
}

# --- (a)/(b) AUTO_MERGE_TOKEN 未設定: @codex review を投稿しない・
#     マーカーも残らない -----------------------------------------------
result=$(_run_codex_pending "__UNSET__")
out="${result%%$'\x1e'*}"
rest="${result#*$'\x1e'}"
log_content="${rest%%$'\x1e'*}"
rc="${rest##*$'\x1e'}"

if [ -n "$log_content" ]; then
  _fail "AUTO_MERGE_TOKEN 未設定時に @codex review が投稿されてしまった (gh pr comment が呼ばれた): ${log_content}"
else
  _pass "AUTO_MERGE_TOKEN 未設定時は @codex review を投稿しない (gh pr comment が一度も呼ばれない = マーカーコメントも残らない)"
fi
if ! echo "$out" | grep -q "::warning::.*AUTO_MERGE_TOKEN.*@codex review"; then
  _fail "AUTO_MERGE_TOKEN 未設定時に期待した ::warning:: (AUTO_MERGE_TOKEN 未設定 / @codex review を投稿しない旨) が出力に無い。出力: ${out}"
else
  _pass "AUTO_MERGE_TOKEN 未設定時に @codex review を投稿しない旨の ::warning:: が出力される"
fi

# --- (c) AUTO_MERGE_TOKEN 設定時: AUTO_MERGE_TOKEN を使って実際に
#     投稿される -----------------------------------------------------
result=$(_run_codex_pending "codex-pat-token")
out="${result%%$'\x1e'*}"
rest="${result#*$'\x1e'}"
log_content="${rest%%$'\x1e'*}"
rc="${rest##*$'\x1e'}"

if [ -z "$log_content" ]; then
  _fail "AUTO_MERGE_TOKEN 設定時に @codex review が投稿されなかった (gh pr comment が呼ばれていない)。出力: ${out}"
elif ! echo "$log_content" | grep -q "^GH_TOKEN=codex-pat-token"; then
  _fail "AUTO_MERGE_TOKEN 設定時の投稿が AUTO_MERGE_TOKEN の値で行われていない (GH_TOKEN フォールバックのままの疑い)。ログ: ${log_content}"
elif ! echo "$log_content" | grep -q "@codex review"; then
  _fail "投稿されたコメント本文に '@codex review' が含まれていない。ログ: ${log_content}"
else
  _pass "AUTO_MERGE_TOKEN 設定時は AUTO_MERGE_TOKEN (PAT) を使って @codex review が投稿される"
fi
if ! echo "$out" | grep -q "info:.*@codex review.*自動投稿しました"; then
  _fail "AUTO_MERGE_TOKEN 設定時に投稿成功のログ (info: ... @codex review を自動投稿しました) が出ていない。出力: ${out}"
else
  _pass "AUTO_MERGE_TOKEN 設定時に投稿成功のログが出力される"
fi

# --- (d) @claude 側の既存挙動が壊れていないことの回帰テスト -------------
# Codex が利用上限に達しており (usage limit メッセージ)、この PR は一度も
# Codex にレビューされておらず (`latestReviews` が空)、
# CLAUDE_REVIEWER_LOGINS が設定されている状態を再現する。この場合
# `claude_review_request_needed=true` になり、`@claude` へのレビュー
# 依頼コメントの投稿が試みられる。
_claude_head_sha="$(printf 'c%.0s' {1..40})"
read -r -d '' _graphql_claude_fallback <<JSON || true
{
  "data": {
    "repository": {
      "pullRequest": {
        "title": "スタブPR (Codex利用上限到達)",
        "reviewThreads": {"pageInfo": {"hasNextPage": false}, "nodes": []},
        "latestReviews": {"pageInfo": {"hasNextPage": false}, "nodes": []},
        "reactions": {"pageInfo": {"hasNextPage": false}, "nodes": []},
        "comments": {"nodes": [
          {
            "body": "@codex review\n\n<!-- auto-merge:codex-review-request:${_claude_head_sha} -->",
            "createdAt": "2026-01-01T00:00:00Z",
            "author": {"login": "github-actions[bot]", "__typename": "Bot"}
          },
          {
            "body": "You have reached your Codex usage limits for code reviews.",
            "createdAt": "2026-01-01T00:05:00Z",
            "author": {"login": "chatgpt-codex-connector[bot]", "__typename": "Bot"}
          }
        ]}
      }
    }
  }
}
JSON

_run_claude_fallback() {
  # 引数1: AUTO_MERGE_TOKEN の値 (未設定を表すには特殊値 __UNSET__)
  local token="$1"
  local dir="${tmp_root}/$(mktemp -u XXXXXX)"
  mkdir -p "$dir"
  _write_stub_gh_logging "$dir" "$_graphql_claude_fallback"
  local out rc
  if [ "$token" = "__UNSET__" ]; then
    out=$(PATH="${dir}:${PATH}" GH_REPO="owner/repo" GH_TOKEN="dummy" \
          GRACE_PERIOD_MINUTES="15" CLAUDE_REVIEWER_LOGINS="claude[bot]" \
          bash "$target" 2 "$_claude_head_sha" false true 2>&1)
    rc=$?
  else
    out=$(PATH="${dir}:${PATH}" GH_REPO="owner/repo" GH_TOKEN="dummy" \
          GRACE_PERIOD_MINUTES="15" CLAUDE_REVIEWER_LOGINS="claude[bot]" \
          AUTO_MERGE_TOKEN="$token" \
          bash "$target" 2 "$_claude_head_sha" false true 2>&1)
    rc=$?
  fi
  local log_content=""
  if [ -f "${dir}/pr_comment.log" ]; then
    log_content="$(cat "${dir}/pr_comment.log")"
  fi
  printf '%s\x1e%s\x1e%s' "$out" "$log_content" "$rc"
}

result=$(_run_claude_fallback "__UNSET__")
out="${result%%$'\x1e'*}"
rest="${result#*$'\x1e'}"
log_content="${rest%%$'\x1e'*}"
rc="${rest##*$'\x1e'}"
if [ -n "$log_content" ]; then
  _fail "(回帰) AUTO_MERGE_TOKEN 未設定時に @claude が投稿されてしまった: ${log_content}"
elif ! echo "$out" | grep -q "::warning::.*AUTO_MERGE_TOKEN.*@claude"; then
  _fail "(回帰) AUTO_MERGE_TOKEN 未設定時に @claude を投稿しない旨の ::warning:: が出力されない。出力: ${out}"
else
  _pass "(回帰) @claude は従来どおり AUTO_MERGE_TOKEN 未設定時に投稿しない"
fi

result=$(_run_claude_fallback "claude-pat-token")
out="${result%%$'\x1e'*}"
rest="${result#*$'\x1e'}"
log_content="${rest%%$'\x1e'*}"
rc="${rest##*$'\x1e'}"
if [ -z "$log_content" ]; then
  _fail "(回帰) AUTO_MERGE_TOKEN 設定時に @claude が投稿されなかった。出力: ${out}"
elif ! echo "$log_content" | grep -q "^GH_TOKEN=claude-pat-token"; then
  _fail "(回帰) AUTO_MERGE_TOKEN 設定時の @claude 投稿が AUTO_MERGE_TOKEN の値で行われていない。ログ: ${log_content}"
else
  _pass "(回帰) @claude は従来どおり AUTO_MERGE_TOKEN 設定時に PAT を使って投稿される"
fi

echo
echo "結果: ${pass_count} passed, ${fail_count} failed"
if [ "$fail_count" -gt 0 ]; then
  exit 1
fi
exit 0
