#!/usr/bin/env python3
"""PR のレビュー状態からマージしてよいかを判定する。

Issue #188: `auto-merge.yml` は head commit の check-runs / commit status
しか見ておらず、Pull Request のレビューを一切考慮せずにマージしていた。
Codex (`chatgpt-codex-connector[bot]`) は check-run を登録しないため、
指摘が出ていてもすり抜けていた (実例: PR #185 → Issue #186、PR #189/#190
でも有効な指摘。3 PR 中 3 件とも有効)。本モジュールはその判定ロジックを
GitHub API から切り離してテスト可能にしたもの
(`.github/scripts/extract_closing_issues.py` + D83 と同じ流儀。
docs/decisions.md D91 参照)。workflow の YAML にはこのロジックをベタ書き
しない。

判定する4条件 (いずれか1つでも該当すればマージを見送る = blocked):

1. 未解決のレビュースレッド (`reviewThreads[].isResolved == false`) が
   1件でもある。人間・ボットを問わず適用する。
2. レビュアーごとの最新レビュー状態 (GraphQL の `latestReviews`) に
   `CHANGES_REQUESTED` が残っている。
3. **Codex (`chatgpt-codex-connector[bot]`) が現在の head SHA をレビュー
   済みでない** (2026-09-07 の方針変更でマージの必須要件になった。
   `codex_bypass=True` で免除できる — `automerge-without-codex` ラベル用)。
   単に「Codex のレビューが存在する」では不十分で、**古いコミットへの
   レビューが残っているだけ**のケースを弾く必要がある。判定に使う
   シグナルは以下の OR (どれか1つでも現在の head を指せば「レビュー済み」
   とみなす):
     a. `latestReviews[].commit.oid` (GraphQL) が head SHA と完全一致する。
     b. レビュー本文の `Reviewed commit: <sha>` 行 (Codex の定型文言) から
        取り出した短縮 SHA が head SHA の接頭辞と一致する。実データで
        確認済み: PR #185/#189/#190/#191 いずれも head SHA の先頭10桁
        (16進数) と一致する。
     c. Codex による 👍 リアクション (PR 本体への `THUMBS_UP`) の
        `createdAt` が **head SHA の push 観測時刻 (`head_push_observed_at`、
        下記参照)** 以降である。PR #191 (0バイトのファイルのみ追加) で
        実測済み: 指摘ゼロのとき Codex はレビュー/コメントを一切残さず
        この 👍 のみを付ける。**注意**: リアクションはコミットに紐付か
        ない (1 ユーザ 1 個) ため、単独では「このリアクションがどの push
        に対するものか」を厳密には特定できない。あくまで「push 観測時刻
        より後に Codex が何らかの反応をした」ことの弱い代理指標として
        扱う。
   Codex ログインの照合は **完全一致** (許可リスト `_CODEX_LOGINS`) で行う
   — `chatgpt-codex-connector-review` のような別名アカウントが前方一致で
   すり抜けないようにするため (2026-09-07 の Codex レビュー指摘、PR #192
   で修正)。GraphQL で `__typename` が取得できている場合は追加で `Bot`
   であることも要求する (`_is_codex_author`)。
4. **head SHA の push 観測時刻 (`head_push_observed_at`)** から猶予期間
   (既定15分、呼び出し側の `gracePeriodMinutes`) が経過していない。
   レビューボットがまだ投稿していない場合の保険。

> ⚠️ **`head_push_observed_at` に git commit の committer date を使っては
> いけない。** committer date は「commit をローカルで作った時刻」であり
> 「GitHub に push された時刻」ではない。ローカルで数時間前に作った
> commit を今 push する、cherry-pick/rebase で古い commit を持ち込む、と
> いった普通の操作で committer date は容易に「past」になる。もし
> committer date を使うと、(a) 以前の head に付いた Codex の 👍 の
> `createdAt` が新しい head の committer date より後になり誤って
> 「現在の SHA をレビュー済み」と判定してしまう (シグナル c の誤検出)、
> (b) 猶予期間も同時に即座に満たされてしまう — **2つの防御が同じ
> 操作可能なタイムスタンプに依存し、同時に破られる**
> (2026-09-07 の Codex レビュー指摘、PR #192 で修正。詳細は
> docs/decisions.md D91)。呼び出し側 (`review_gate_decision.sh`) は
> GitHub がサーバ側で観測した時刻 (head SHA に対する check-suite の
> 作成時刻) を渡すこと。取得できなかった場合にこれへフォールバックする
> ことも同じ脆弱性を再現するため禁止。

**CodeRabbit は判定に含めない。** Free プランの制限 (1時間1レビュー、
行単位の指摘を出さず要約のみ) により実質機能しておらず、リポジトリへの
アクセスも除外済み (Issue #188 の方針変更)。CodeRabbit が過去に指摘を
出していた場合でも、それは 1. の未解決スレッド判定で拾われる。

GraphQL のページング (`pageInfo.hasNextPage == true`) で全件を確認できな
かった場合、および `head_push_observed_at` が取得できなかった場合は、
安全側 (マージしない = blocked) に倒す。「取得できなかったので指摘
ゼロとみなす」は絶対にしない。
"""

from __future__ import annotations

import json
import re
import sys
from datetime import datetime, timezone
from typing import Any

# GraphQL で最新レビューが CHANGES_REQUESTED として残っていることを示す状態。
_CHANGES_REQUESTED = "CHANGES_REQUESTED"

# Codex アプリの既知のログイン名 (完全一致で照合する — 前方一致にすると
# `chatgpt-codex-connector-review` のような別名アカウントが通ってしまう。
# 2026-09-07 の Codex レビュー指摘、PR #192)。REST/GraphQL いずれの API
# でも実データは "chatgpt-codex-connector[bot]" だった (末尾 "[bot]" 付き)。
_CODEX_LOGINS = frozenset({"chatgpt-codex-connector[bot]"})

# Codex のレビュー本文に含まれる定型行 (例: "**Reviewed commit:** `80246e861a`")
# から短縮 SHA を取り出す。太字マークアップ (**...**) の有無・コロンの位置
# ゆらぎを許容する。
_REVIEWED_COMMIT_RE = re.compile(
    r"Reviewed commit:?\**\s*`?([0-9a-fA-F]{7,40})`?", re.IGNORECASE
)


def _parse_iso8601(value: str) -> datetime:
    # GitHub の日時は `Z` 終端の UTC ISO8601。Python 3.10 以前の
    # `datetime.fromisoformat` は `Z` をそのまま扱えないため置換する。
    if value.endswith("Z"):
        value = value[:-1] + "+00:00"
    dt = datetime.fromisoformat(value)
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return dt


def _is_codex_author(
    author: dict[str, Any] | None, codex_logins: frozenset[str]
) -> bool:
    """`author`/`user` フィールド (GraphQL Actor) が Codex 本人か判定する。

    ログイン名は完全一致 (大小無視) でのみ許可する — 前方一致・部分一致は
    別名アカウントによるなりすましを許してしまうため使わない。`__typename`
    が取得できている場合は追加で `Bot` であることも要求する (取得できて
    いない場合は互換性のためスキップ — GraphQL クエリが `__typename` を
    要求していれば常に埋まる)。
    """
    if not author:
        return False
    login = (author.get("login") or "").lower()
    if login not in {l.lower() for l in codex_logins}:
        return False
    typename = author.get("__typename")
    if typename is not None and typename != "Bot":
        return False
    return True


def _codex_reviewed_head_sha(
    latest_reviews_nodes: list[dict[str, Any]],
    head_sha: str,
    codex_logins: frozenset[str],
) -> tuple[bool, str | None]:
    """Codex のレビューが現在の head SHA を指しているか判定する。

    Returns: (matched, last_seen_hint) — `last_seen_hint` は一致しなかった
    場合に「直近の Codex レビューはどのコミットに対するものだったか」を
    ログに出すための短縮 SHA (無ければ None)。
    """
    codex_reviews = [
        r for r in latest_reviews_nodes if _is_codex_author(r.get("author"), codex_logins)
    ]
    if not codex_reviews:
        return False, None

    matched = False
    hint: str | None = None
    for r in codex_reviews:
        commit_oid = (r.get("commit") or {}).get("oid")
        if commit_oid and head_sha and commit_oid == head_sha:
            matched = True

        body = r.get("body") or ""
        m = _REVIEWED_COMMIT_RE.search(body)
        if m:
            short_sha = m.group(1)
            hint = short_sha
            if head_sha and head_sha.lower().startswith(short_sha.lower()):
                matched = True

    return matched, hint


def _codex_reacted_after(
    reaction_nodes: list[dict[str, Any]],
    head_push_observed_at: str | None,
    codex_logins: frozenset[str],
) -> bool:
    """Codex による 👍 リアクションが head SHA の push 観測時刻より後か判定する。

    `head_push_observed_at` には GitHub がサーバ側で観測した時刻 (head SHA
    に対する check-suite の作成時刻など) を渡すこと。git の committer
    date を渡してはいけない (モジュール docstring の警告を参照)。
    """
    if not head_push_observed_at:
        return False
    observed_at = _parse_iso8601(head_push_observed_at)
    for r in reaction_nodes:
        if not _is_codex_author(r.get("user"), codex_logins):
            continue
        created_at = r.get("createdAt")
        if not created_at:
            continue
        if _parse_iso8601(created_at) >= observed_at:
            return True
    return False


def evaluate_review_gate(
    review_threads: dict[str, Any] | None,
    latest_reviews: dict[str, Any] | None,
    head_push_observed_at: str | None,
    now: str,
    grace_period_minutes: int,
    reactions: dict[str, Any] | None = None,
    head_sha: str | None = None,
    codex_bypass: bool = False,
    codex_logins: frozenset[str] = _CODEX_LOGINS,
) -> dict[str, Any]:
    """マージしてよいかを判定する。

    Args:
        review_threads: GraphQL `pullRequest.reviewThreads` connection
            (`{"pageInfo": {"hasNextPage": bool}, "nodes": [{"isResolved":
            bool, "comments": {"nodes": [{"path": str, "author":
            {"login": str}}]}}, ...]}`)。`None` は「取得できなかった」を
            表し、安全側 (blocked) の理由になる。
        latest_reviews: GraphQL `pullRequest.latestReviews` connection
            (`{"pageInfo": {...}, "nodes": [{"state": str, "body": str,
            "author": {"login": str, "__typename": str}, "commit":
            {"oid": str}}, ...]}`)。
        head_push_observed_at: **head SHA が GitHub にサーバ側で観測された
            時刻** (ISO8601)。head SHA に対する check-suite の作成時刻など、
            attacker が操作できないサーバ側のタイムスタンプを渡すこと。
            **git commit の committer date を渡してはいけない**
            (モジュール docstring の警告を参照 — committer date は
            ローカルで自由に設定できるため、猶予期間とシグナル c の両方の
            防御が同時に破られる)。`None`/空文字列は「取得できなかった」
            を表し、安全側でブロックする (committer date へのフォール
            バックは禁止)。
        now: 判定時刻 (ISO8601)。呼び出し側から渡す (テスト容易性のため
            `datetime.now()` をこの関数の中で呼ばない)。
        grace_period_minutes: 猶予期間 (分)。
        reactions: GraphQL `pullRequest.reactions(content: THUMBS_UP)`
            connection (`{"pageInfo": {...}, "nodes": [{"createdAt": str,
            "user": {"login": str, "__typename": str}}, ...]}`)。Codex の
            👍 リアクション判定に使う。省略時は「リアクションによる代替
            シグナルなし」扱い。
        head_sha: 現在の PR head commit のフル SHA。Codex レビュー必須
            判定に使う。
        codex_bypass: `True` なら Codex レビュー必須判定 (条件3) を免除
            する (`automerge-without-codex` ラベル用)。未解決スレッド判定・
            CHANGES_REQUESTED 判定は免除しない。
        codex_logins: Codex の既知ログイン名の完全一致許可リスト (テスト用
            に差し替え可能)。前方一致・部分一致は使わない。

    Returns:
        `{"blocked": bool, "reasons": [str, ...]}`。`reasons` はログ出力
        用の日本語メッセージ (先頭が "wait: ")。`blocked` は
        `len(reasons) > 0` と等価。
    """
    reasons: list[str] = []

    # --- 1. 未解決のレビュースレッド -----------------------------------
    threads_page_info = (review_threads or {}).get("pageInfo") or {}
    if review_threads is None:
        reasons.append(
            "wait: レビュースレッドの取得に失敗したため、安全側でスキップします"
        )
    elif threads_page_info.get("hasNextPage"):
        reasons.append(
            "wait: レビュースレッドの件数が多く GraphQL の1ページ (100件) に"
            "収まらなかったため全件を確認できません (安全側でスキップ)"
        )
    else:
        threads = review_threads.get("nodes") or []
        unresolved = [t for t in threads if not t.get("isResolved", True)]
        if unresolved:
            details = []
            for t in unresolved:
                comment_nodes = ((t.get("comments") or {}).get("nodes")) or []
                first = comment_nodes[0] if comment_nodes else {}
                path = first.get("path") or "?"
                author = (first.get("author") or {}).get("login") or "?"
                details.append(f"{path} ({author})")
            reasons.append(
                f"wait: 未解決のレビュースレッドが {len(unresolved)} 件あります: "
                + ", ".join(details)
            )

    # --- 2. CHANGES_REQUESTED ------------------------------------------
    reviews_page_info = (latest_reviews or {}).get("pageInfo") or {}
    reviews_ok = True
    latest_reviews_nodes: list[dict[str, Any]] = []
    if latest_reviews is None:
        reasons.append(
            "wait: レビュー状態の取得に失敗したため、安全側でスキップします"
        )
        reviews_ok = False
    elif reviews_page_info.get("hasNextPage"):
        reasons.append(
            "wait: レビューの件数が多く GraphQL の1ページ (100件) に"
            "収まらなかったため全件を確認できません (安全側でスキップ)"
        )
        reviews_ok = False
    else:
        latest_reviews_nodes = latest_reviews.get("nodes") or []
        changes_requested = [
            r for r in latest_reviews_nodes if r.get("state") == _CHANGES_REQUESTED
        ]
        if changes_requested:
            authors = [
                (r.get("author") or {}).get("login") or "?"
                for r in changes_requested
            ]
            reasons.append(
                "wait: CHANGES_REQUESTED のレビューが残っています (レビュアー: "
                + ", ".join(authors)
                + ")"
            )

    # --- 3. Codex が現在の head SHA をレビュー済みか ---------------------
    # latestReviews が取得できている (安全に判定できる) 場合のみチェック
    # する。取得失敗時は上の reasons で既に安全側ブロックされているため、
    # ここで重ねて紛らわしい「Codex 待ち」メッセージは出さない。
    if not codex_bypass and reviews_ok:
        # reactions は代替シグナル (シグナル c) のための補助データであり、
        # 取得できなかった/全件確認できなかった場合でも単独ではブロック
        # 理由にしない (シグナル a/b によるレビュー本体での判定を優先し、
        # それでも一致しなければ「Codex 待ち」としてブロックされる)。
        reaction_nodes: list[dict[str, Any]] = (reactions or {}).get("nodes") or []

        if not head_sha:
            reasons.append(
                "wait: head SHA を取得できなかったため Codex のレビューを"
                "確認できません (安全側でスキップ)"
            )
        else:
            matched, hint = _codex_reviewed_head_sha(
                latest_reviews_nodes, head_sha, codex_logins
            )
            if not matched:
                matched = _codex_reacted_after(
                    reaction_nodes, head_push_observed_at, codex_logins
                )
            if not matched:
                head_short = head_sha[:7]
                detail = f"head SHA `{head_short}` に対するレビュー/👍リアクションが見つかりません"
                if hint:
                    detail += f" (直近の Codex レビューは commit `{hint}` に対するものでした)"
                reasons.append(f"wait: Codex のレビュー待ち ({detail})")

    # --- 4. 猶予期間 -----------------------------------------------------
    # head_push_observed_at は「GitHub がサーバ側で head SHA の push を
    # 観測した時刻」でなければならない。committer date を渡す/フォール
    # バックすることは禁止 (モジュール docstring の警告を参照)。
    if not head_push_observed_at:
        reasons.append(
            "wait: head SHA の push 観測時刻を取得できなかったため、"
            "安全側でスキップします"
        )
    else:
        observed_at = _parse_iso8601(head_push_observed_at)
        now_at = _parse_iso8601(now)
        elapsed_minutes = (now_at - observed_at).total_seconds() / 60
        if elapsed_minutes < grace_period_minutes:
            remaining = grace_period_minutes - elapsed_minutes
            reasons.append(
                f"wait: head SHA の push 観測からまだ {elapsed_minutes:.1f}分 "
                f"しか経過していません (猶予期間 {grace_period_minutes}分、"
                f"あと {remaining:.1f}分)"
            )

    return {"blocked": len(reasons) > 0, "reasons": reasons}


def main(argv: list[str]) -> int:
    """CLI: 標準入力 (または引数で渡されたファイル) から JSON payload を読み、
    判定結果を JSON で標準出力に書く。

    payload の形は `{"reviewThreads": ..., "latestReviews": ...,
    "reactions": ..., "headSha": ..., "headPushObservedAt": ..., "now": ...,
    "gracePeriodMinutes": ..., "codexBypass": ...}`。`now`/
    `gracePeriodMinutes` を省略した場合はそれぞれ現在時刻/15分を使う。
    `headPushObservedAt` には GitHub がサーバ側で観測した時刻を渡すこと
    (git committer date ではない — モジュール docstring の警告を参照)。
    """
    if len(argv) > 1:
        with open(argv[1], "r", encoding="utf-8") as f:
            payload = json.load(f)
    else:
        payload = json.load(sys.stdin)

    result = evaluate_review_gate(
        review_threads=payload.get("reviewThreads"),
        latest_reviews=payload.get("latestReviews"),
        head_push_observed_at=payload.get("headPushObservedAt") or None,
        now=payload.get("now") or datetime.now(timezone.utc).isoformat(),
        grace_period_minutes=int(payload.get("gracePeriodMinutes", 15)),
        reactions=payload.get("reactions"),
        head_sha=payload.get("headSha") or None,
        codex_bypass=bool(payload.get("codexBypass", False)),
    )
    json.dump(result, sys.stdout, ensure_ascii=False)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
