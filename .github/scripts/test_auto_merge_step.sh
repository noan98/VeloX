#!/usr/bin/env bash
# auto-merge.yml の「マージ判定ステップ」(inline シェルスクリプト) の
# ローカル統合テスト。
#
# **なぜ必要か**: このスクリプトは workflow の `run:` ブロックに直書き
# されており、**main にマージして実際に auto-merge が起動するまで一度も
# 実行されない。** 判定ロジック (check_review_gate.py) には 100 件を超える
# 単体テストがあるのに、それを呼ぶ側のループには 1 件も無かった。
# Issue #219 でこのループに「猶予期間だけを待っている PR があれば待ち直す」
# 制御を足したので、ここで固定する (docs/decisions.md D103)。
#
# 方法: auto-merge.yml から `run:` の中身を取り出し、`gh` / `sleep` /
# `review_gate_decision.sh` をスタブに差し替えたサンドボックスで実行して、
# 挙動 (sleep したか・何秒か・マージしたか) を検証する。
#
# 実行方法:
#   bash .github/scripts/test_auto_merge_step.sh
#
# 終了コード: 0 = 全件 pass、1 = 1件以上 fail。
set -uo pipefail

scripts_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${scripts_dir}/../.." && pwd)"
workflow="${repo_root}/.github/workflows/auto-merge.yml"

tmp_root="$(mktemp -d)"
trap 'rm -rf "$tmp_root"' EXIT

pass_count=0
fail_count=0
_fail() { echo "FAIL: $1"; echo "      $2"; fail_count=$((fail_count + 1)); }
_pass() { echo "ok: $1"; pass_count=$((pass_count + 1)); }

# --- workflow から run: の中身を取り出す --------------------------------
step_script="${tmp_root}/merge_step.sh"
if ! python3 - "$workflow" "$step_script" <<'PY'
import sys
import yaml

workflow = yaml.safe_load(open(sys.argv[1], encoding="utf-8"))
steps = workflow["jobs"]["auto-merge"]["steps"]
runs = [s["run"] for s in steps if "run" in s]
if len(runs) != 1:
    raise SystemExit(f"auto-merge ジョブの run: ステップが {len(runs)} 個ありました (1 個を想定)")
open(sys.argv[2], "w", encoding="utf-8").write(runs[0])
PY
then
  echo "FAIL: auto-merge.yml から run: スクリプトを取り出せませんでした"
  exit 1
fi
bash -n "$step_script" || { echo "FAIL: 取り出したスクリプトが bash 構文エラーです"; exit 1; }

# --- サンドボックスを組み立てる ------------------------------------------
# 引数1: gate プラン (1 行 = 1 回の呼び出し。"<rc>:<only_grace>:<残り秒>")
# 引数2: gh pr list が返す PR の JSON 配列
_setup() {
  local plan="$1" prs_json="$2"
  work="${tmp_root}/case$((pass_count + fail_count))"
  mkdir -p "${work}/bin" "${work}/.github/scripts"

  printf '%s\n' "$plan" > "${work}/gate_plan"
  echo 0 > "${work}/gate_calls"
  printf '%s' "$prs_json" > "${work}/prs.json"
  : > "${work}/sleeps"
  : > "${work}/merged"

  # review_gate_decision.sh のスタブ。呼ばれるたびにプランを 1 行進める。
  cat > "${work}/.github/scripts/review_gate_decision.sh" <<'STUB'
#!/usr/bin/env bash
n=$(cat "${WORK}/gate_calls"); n=$((n + 1)); echo "$n" > "${WORK}/gate_calls"
line=$(sed -n "${n}p" "${WORK}/gate_plan")
[ -z "$line" ] && line=$(tail -n 1 "${WORK}/gate_plan")
rc="${line%%:*}"; rest="${line#*:}"
only="${rest%%:*}"; secs="${rest#*:}"
[ -z "$only" ] && only=false
if [ -n "${GATE_RESULT_FILE:-}" ]; then
  if [ -n "$secs" ]; then
    printf '{"blocked":true,"blocked_only_by_grace":%s,"grace_remaining_seconds":%s}' "$only" "$secs" > "$GATE_RESULT_FILE"
  else
    printf '{"blocked":%s,"blocked_only_by_grace":false,"grace_remaining_seconds":null}' \
      "$([ "$rc" = "0" ] && echo false || echo true)" > "$GATE_RESULT_FILE"
  fi
fi
exit "$rc"
STUB
  chmod +x "${work}/.github/scripts/review_gate_decision.sh"
  cp "${scripts_dir}/extract_closing_issues.py" "${work}/.github/scripts/"

  cat > "${work}/bin/gh" <<'STUB'
#!/usr/bin/env bash
if [ "$1" = "pr" ] && [ "$2" = "list" ]; then
  cat "${WORK}/prs.json"; exit 0
elif [ "$1" = "api" ] && [[ "$*" == *check-runs* ]]; then
  # 呼び出し側は `--jq '.check_runs[]'` を付けるので、gh は**要素**を
  # 1 行ずつ出す (ラッパーオブジェクトではない)。ここを間違えると
  # `.name` が null になり「未完了のチェックがある」と誤判定される。
  echo '{"name":"CI","status":"completed","conclusion":"success"}'; exit 0
elif [ "$1" = "api" ] && [[ "$*" == *"/status"* ]]; then
  # `--jq '.statuses'` 付きなので配列そのものを返す
  echo '[]'; exit 0
elif [ "$1" = "pr" ] && [ "$2" = "merge" ]; then
  echo "$3" >> "${WORK}/merged"; exit 0
elif [ "$1" = "pr" ] && [ "$2" = "view" ]; then
  echo ""; exit 0
fi
echo "unhandled gh: $*" >&2; exit 1
STUB
  chmod +x "${work}/bin/gh"

  # 実際には眠らず、秒数だけ記録する
  cat > "${work}/bin/sleep" <<'STUB'
#!/usr/bin/env bash
echo "$1" >> "${WORK}/sleeps"
STUB
  chmod +x "${work}/bin/sleep"
}

_run() {
  ( cd "$work" && WORK="$work" PATH="${work}/bin:$PATH" \
      GH_REPO="o/r" GH_TOKEN="t" AUTO_MERGE_TOKEN="t" \
      BASE_BRANCH="main" SKIP_LABEL="no-automerge" \
      CODEX_BYPASS_LABEL="automerge-without-codex" MERGE_METHOD="merge" \
      SELF_WORKFLOW="Auto Merge" SELF_JOB="Auto merge PRs whose checks all passed" \
      GRACE_PERIOD_MINUTES="15" \
      bash "$step_script" ) > "${work}/out" 2>&1
  echo $?
}

_one_pr='[{"number":1,"title":"t","isDraft":false,"labels":[],"headRefOid":"deadbeef","headRefName":"b"}]'
_two_prs='[{"number":1,"title":"t1","isDraft":false,"labels":[],"headRefOid":"aaa","headRefName":"b1"},
           {"number":2,"title":"t2","isDraft":false,"labels":[],"headRefOid":"bbb","headRefName":"b2"}]'

# --- 1. 猶予期間だけがブロック要因 → 待ち直してマージする ---------------
_setup "1:true:300
0::" "$_one_pr"
rc=$(_run)
sleeps=$(tr '\n' ' ' < "${work}/sleeps")
if [ "$rc" = "0" ] && [ "$(wc -l < "${work}/sleeps")" = "1" ] \
   && [ "$(head -1 "${work}/sleeps")" = "315" ] && [ -s "${work}/merged" ] \
   && [ "$(cat "${work}/gate_calls")" = "2" ]; then
  _pass "猶予期間だけが残っているとき、残り+15秒 待って再評価しマージする"
else
  _fail "猶予期間だけが残っているとき、残り+15秒 待って再評価しマージする" \
        "rc=${rc} sleeps=[${sleeps}] gate_calls=$(cat "${work}/gate_calls") merged=[$(tr '\n' ' ' < "${work}/merged")]"
fi

# --- 2. 猶予期間 **と** それ以外が同時に残っている → 待たない -----------
# 未解決スレッドが残ったまま push した直後がこの形になる。残り秒数は
# 取得できるが `blocked_only_by_grace` は false — **待っても解決しない**
# ので待ってはいけない (待つのは誰も何もしなくても解決する場合だけ)。
_setup "1:false:300" "$_one_pr"
rc=$(_run)
if [ "$rc" = "0" ] && [ ! -s "${work}/sleeps" ] && [ ! -s "${work}/merged" ] \
   && [ "$(cat "${work}/gate_calls")" = "1" ]; then
  _pass "猶予期間**以外**の理由も残っているときは、残り秒数が分かっても待たない"
else
  _fail "猶予期間**以外**の理由も残っているときは、残り秒数が分かっても待たない" \
        "rc=${rc} sleeps=[$(tr '\n' ' ' < "${work}/sleeps")] gate_calls=$(cat "${work}/gate_calls")"
fi

# --- 3. 最初から条件を満たす → 待たずに 1 パスでマージ ------------------
_setup "0::" "$_one_pr"
rc=$(_run)
if [ "$rc" = "0" ] && [ ! -s "${work}/sleeps" ] && [ "$(cat "${work}/gate_calls")" = "1" ] \
   && [ -s "${work}/merged" ]; then
  _pass "ブロック要因が無ければ待たず、ゲート呼び出しは 1 回で済む"
else
  _fail "ブロック要因が無ければ待たず、ゲート呼び出しは 1 回で済む" \
        "rc=${rc} gate_calls=$(cat "${work}/gate_calls") sleeps=[$(tr '\n' ' ' < "${work}/sleeps")]"
fi

# --- 4. 待ち時間の上限 (猶予期間 + 1分) ---------------------------------
_setup "1:true:999999
0::" "$_one_pr"
rc=$(_run)
if [ "$(head -1 "${work}/sleeps")" = "960" ]; then
  _pass "待ち時間は猶予期間+1分 (960秒) で頭打ちになる"
else
  _fail "待ち時間は猶予期間+1分 (960秒) で頭打ちになる" \
        "sleeps=[$(tr '\n' ' ' < "${work}/sleeps")]"
fi

# --- 5. 待ち直しは 1 回まで ---------------------------------------------
_setup "1:true:60" "$_one_pr"   # 何度呼ばれても「猶予期間だけ」を返し続ける
rc=$(_run)
if [ "$rc" = "0" ] && [ "$(wc -l < "${work}/sleeps")" = "1" ] && [ ! -s "${work}/merged" ] \
   && [ "$(cat "${work}/gate_calls")" = "2" ]; then
  _pass "待ち直しは 1 回まで — 満了しなくても無限には待たない (ゲート呼び出しは 2 回)"
else
  _fail "待ち直しは 1 回まで — 満了しなくても無限には待たない (ゲート呼び出しは 2 回)" \
        "rc=${rc} sleeps=[$(tr '\n' ' ' < "${work}/sleeps")] gate_calls=$(cat "${work}/gate_calls")"
fi

# --- 6. 複数 PR: 最短の残り時間で起きる ---------------------------------
_setup "1:true:600
1:true:120
0::
0::" "$_two_prs"
rc=$(_run)
if [ "$(head -1 "${work}/sleeps")" = "135" ] && [ "$(cat "${work}/gate_calls")" = "4" ] \
   && [ "$(wc -l < "${work}/merged")" = "2" ]; then
  _pass "複数 PR が待っているときは最短の残り時間に合わせて起き、全 PR を再評価する"
else
  _fail "複数 PR が待っているときは最短の残り時間に合わせて起き、全 PR を再評価する" \
        "sleeps=[$(tr '\n' ' ' < "${work}/sleeps")] (期待 135) gate_calls=$(cat "${work}/gate_calls") (期待 4) merged=$(wc -l < "${work}/merged") (期待 2)"
fi

echo
echo "結果: ${pass_count} passed, ${fail_count} failed"
[ "$fail_count" -eq 0 ]
