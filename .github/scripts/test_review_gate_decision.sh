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

echo
echo "結果: ${pass_count} passed, ${fail_count} failed"
if [ "$fail_count" -gt 0 ]; then
  exit 1
fi
exit 0
