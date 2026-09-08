#!/usr/bin/env python3
"""check_review_gate.py のユニットテスト。

実行方法:
    python3 -m unittest .github/scripts/test_check_review_gate.py -v
または (このディレクトリから):
    python3 -m unittest test_check_review_gate -v

Issue #188 の完了条件・PR #185 / #189 / #191 / #192 の実例をそのまま網羅する。

テスト方針: 「未解決スレッド」「CHANGES_REQUESTED」「猶予期間」の3条件を
検証するテストクラスは `codex_bypass=True` を既定にして Codex 必須判定を
分離する (この3条件は Codex 要件と無関係のため)。Codex 必須判定
(条件3、`automerge-without-codex` ラベルでの免除を含む) は専用の
`CodexReviewRequiredTest` で個別に検証する。`CodexLoginExactMatchTest` /
`PushObservedAtSecurityRegressionTest` は 2026-09-07 の Codex レビュー
指摘 (PR #192、docs/decisions.md D91) を受けて追加した回帰テスト:
Codex ログイン判定の前方一致 (別名アカウントによるなりすまし) と、猶予期間
/シグナルcの基準時刻に committer date を使う設計 (attacker が操作可能な
タイムスタンプ) の2件の脆弱性を防ぐ。`CodexUsageLimitAndAutoRequestTest`
は同じく PR #192 の運用で判明した2つの前提 (Codex は push では再レビュー
しない/Codex には利用上限がある) への対応 (`@codex review` の自動リクエスト
と、利用上限到達時の緩和) を検証する — この機能自体が新規追加のため、
旧実装 (この機能を持たない `evaluate_review_gate`) に対しては
`codex_review_request_needed`/`codex_relaxed` キーが無く `KeyError` で
必ず失敗する、という意味で「修正前に失敗し修正後に通る」テスト群になる。
"""

from __future__ import annotations

import unittest

from check_review_gate import evaluate_review_gate

# 猶予期間を確実に満たす基準時刻 (head SHA の push 観測時刻から1時間後)。
_NOW = "2026-09-07T16:31:14Z"
_HEAD_PUSH_OBSERVED_1H_AGO = "2026-09-07T15:31:14Z"
_GRACE = 15
_HEAD_SHA = "80246e861a57bb526dea041b86bd07d51b6d7957"
_CODEX_LOGIN = "chatgpt-codex-connector[bot]"

_EMPTY_THREADS = {"pageInfo": {"hasNextPage": False}, "nodes": []}
_EMPTY_REVIEWS = {"pageInfo": {"hasNextPage": False}, "nodes": []}
_EMPTY_REACTIONS = {"pageInfo": {"hasNextPage": False}, "nodes": []}
_EMPTY_PR_COMMENTS = {"nodes": []}


def _evaluate(
    threads=_EMPTY_THREADS,
    reviews=_EMPTY_REVIEWS,
    push_observed_at=_HEAD_PUSH_OBSERVED_1H_AGO,
    now=_NOW,
    grace=_GRACE,
    reactions=_EMPTY_REACTIONS,
    head_sha=_HEAD_SHA,
    codex_bypass=True,
    pr_comments=_EMPTY_PR_COMMENTS,
    claude_logins=frozenset(),
):
    """条件1/2/4 (Codex 非依存の条件) を検証するための既定ヘルパー。

    `codex_bypass=True` を既定にしているため、Codex レビューの有無に
    関わらず条件3ではブロックされない。`claude_logins` は既定で空集合
    (Claude フォールバック無効)。
    """
    return evaluate_review_gate(
        threads,
        reviews,
        push_observed_at,
        now,
        grace,
        reactions=reactions,
        head_sha=head_sha,
        codex_bypass=codex_bypass,
        pr_comments=pr_comments,
        claude_logins=claude_logins,
    )


class NoIssuesMergesTest(unittest.TestCase):
    """完了条件: 指摘ゼロの PR は正常にマージされる (停滞しない)。"""

    def test_no_threads_no_reviews_past_grace_period_not_blocked(self) -> None:
        result = _evaluate()
        self.assertFalse(result["blocked"])
        self.assertEqual(result["reasons"], [])

    def test_all_resolved_threads_not_blocked(self) -> None:
        threads = {
            "pageInfo": {"hasNextPage": False},
            "nodes": [
                {"isResolved": True, "comments": {"nodes": []}},
                {"isResolved": True, "comments": {"nodes": []}},
            ],
        }
        result = _evaluate(threads=threads)
        self.assertFalse(result["blocked"])

    def test_approved_review_not_blocked(self) -> None:
        reviews = {
            "pageInfo": {"hasNextPage": False},
            "nodes": [{"state": "APPROVED", "author": {"login": "octocat"}}],
        }
        result = _evaluate(reviews=reviews)
        self.assertFalse(result["blocked"])

    def test_commented_review_not_blocked(self) -> None:
        reviews = {
            "pageInfo": {"hasNextPage": False},
            "nodes": [{"state": "COMMENTED", "author": {"login": "octocat"}}],
        }
        result = _evaluate(reviews=reviews)
        self.assertFalse(result["blocked"])

    def test_dismissed_changes_requested_not_blocked(self) -> None:
        # DISMISSED は「最新状態」として latestReviews に残らない想定だが、
        # 仮に混入していても CHANGES_REQUESTED 以外は無視する。
        reviews = {
            "pageInfo": {"hasNextPage": False},
            "nodes": [{"state": "DISMISSED", "author": {"login": "octocat"}}],
        }
        result = _evaluate(reviews=reviews)
        self.assertFalse(result["blocked"])

    def test_pending_review_not_blocked(self) -> None:
        # PENDING (未提出のドラフトレビュー) は判定対象にしない。
        reviews = {
            "pageInfo": {"hasNextPage": False},
            "nodes": [{"state": "PENDING", "author": {"login": "octocat"}}],
        }
        result = _evaluate(reviews=reviews)
        self.assertFalse(result["blocked"])


class UnresolvedThreadBlocksTest(unittest.TestCase):
    """条件1: 未解決のレビュースレッド。"""

    def test_single_unresolved_thread_blocked(self) -> None:
        threads = {
            "pageInfo": {"hasNextPage": False},
            "nodes": [
                {
                    "isResolved": False,
                    "comments": {
                        "nodes": [
                            {
                                "path": "src/browser/suspension.rs",
                                "author": {"login": "chatgpt-codex-connector"},
                            }
                        ]
                    },
                }
            ],
        }
        result = _evaluate(threads=threads)
        self.assertTrue(result["blocked"])
        self.assertEqual(len(result["reasons"]), 1)
        reason = result["reasons"][0]
        self.assertIn("未解決のレビュースレッドが 1 件", reason)
        self.assertIn("src/browser/suspension.rs", reason)
        self.assertIn("chatgpt-codex-connector", reason)

    def test_unresolved_thread_from_human_reviewer_blocked(self) -> None:
        # 人間・ボットを問わず適用される。
        threads = {
            "pageInfo": {"hasNextPage": False},
            "nodes": [
                {
                    "isResolved": False,
                    "comments": {
                        "nodes": [
                            {"path": "README.md", "author": {"login": "octocat"}}
                        ]
                    },
                }
            ],
        }
        result = _evaluate(threads=threads)
        self.assertTrue(result["blocked"])
        self.assertIn("octocat", result["reasons"][0])

    def test_multiple_unresolved_threads_counted(self) -> None:
        threads = {
            "pageInfo": {"hasNextPage": False},
            "nodes": [
                {
                    "isResolved": False,
                    "comments": {
                        "nodes": [{"path": "a.rs", "author": {"login": "u1"}}]
                    },
                },
                {
                    "isResolved": False,
                    "comments": {
                        "nodes": [{"path": "b.rs", "author": {"login": "u2"}}]
                    },
                },
                {"isResolved": True, "comments": {"nodes": []}},
            ],
        }
        result = _evaluate(threads=threads)
        self.assertTrue(result["blocked"])
        self.assertIn("2 件", result["reasons"][0])
        self.assertIn("a.rs", result["reasons"][0])
        self.assertIn("b.rs", result["reasons"][0])

    def test_thread_missing_comments_uses_placeholder(self) -> None:
        threads = {
            "pageInfo": {"hasNextPage": False},
            "nodes": [{"isResolved": False, "comments": {"nodes": []}}],
        }
        result = _evaluate(threads=threads)
        self.assertTrue(result["blocked"])
        self.assertIn("?", result["reasons"][0])

    def test_threads_pagination_incomplete_blocks_safely(self) -> None:
        threads = {"pageInfo": {"hasNextPage": True}, "nodes": []}
        result = _evaluate(threads=threads)
        self.assertTrue(result["blocked"])
        self.assertIn("安全側でスキップ", result["reasons"][0])

    def test_threads_fetch_failure_blocks_safely(self) -> None:
        result = _evaluate(threads=None)
        self.assertTrue(result["blocked"])
        self.assertIn("取得に失敗", result["reasons"][0])


class ChangesRequestedBlocksTest(unittest.TestCase):
    """条件2: CHANGES_REQUESTED。"""

    def test_changes_requested_blocked(self) -> None:
        reviews = {
            "pageInfo": {"hasNextPage": False},
            "nodes": [
                {"state": "CHANGES_REQUESTED", "author": {"login": "octocat"}}
            ],
        }
        result = _evaluate(reviews=reviews)
        self.assertTrue(result["blocked"])
        self.assertIn("CHANGES_REQUESTED", result["reasons"][0])
        self.assertIn("octocat", result["reasons"][0])

    def test_multiple_changes_requested_authors_listed(self) -> None:
        reviews = {
            "pageInfo": {"hasNextPage": False},
            "nodes": [
                {"state": "CHANGES_REQUESTED", "author": {"login": "u1"}},
                {"state": "CHANGES_REQUESTED", "author": {"login": "u2"}},
                {"state": "APPROVED", "author": {"login": "u3"}},
            ],
        }
        result = _evaluate(reviews=reviews)
        self.assertTrue(result["blocked"])
        self.assertIn("u1", result["reasons"][0])
        self.assertIn("u2", result["reasons"][0])

    def test_reviews_pagination_incomplete_blocks_safely(self) -> None:
        reviews = {"pageInfo": {"hasNextPage": True}, "nodes": []}
        result = _evaluate(reviews=reviews)
        self.assertTrue(result["blocked"])
        self.assertIn("安全側でスキップ", result["reasons"][0])

    def test_reviews_fetch_failure_blocks_safely(self) -> None:
        result = _evaluate(reviews=None)
        self.assertTrue(result["blocked"])
        self.assertIn("取得に失敗", result["reasons"][0])


class GracePeriodBlocksTest(unittest.TestCase):
    """条件4: 猶予期間 (head SHA の push 観測時刻からの経過)。"""

    def test_fresh_push_blocked(self) -> None:
        result = _evaluate(push_observed_at="2026-09-07T16:20:00Z", now=_NOW)  # 11分
        self.assertTrue(result["blocked"])
        self.assertIn("猶予期間", result["reasons"][0])

    def test_push_exactly_at_grace_period_not_blocked(self) -> None:
        # 15分ちょうど経過 -> 猶予期間は満了 (blocked にならない)。
        result = _evaluate(
            push_observed_at="2026-09-07T16:16:14Z", now=_NOW, grace=15
        )
        self.assertFalse(result["blocked"])

    def test_push_one_second_before_grace_period_blocked(self) -> None:
        result = _evaluate(
            push_observed_at="2026-09-07T16:16:15Z", now=_NOW, grace=15
        )
        self.assertTrue(result["blocked"])

    def test_missing_push_observed_at_blocks_safely(self) -> None:
        result = _evaluate(push_observed_at=None)
        self.assertTrue(result["blocked"])
        self.assertIn("取得できなかった", result["reasons"][0])

    def test_empty_push_observed_at_blocks_safely(self) -> None:
        result = _evaluate(push_observed_at="")
        self.assertTrue(result["blocked"])

    def test_custom_grace_period_respected(self) -> None:
        # 猶予期間を60分に設定すると、1時間経過ちょうどでも境界を割る。
        result = _evaluate(
            push_observed_at=_HEAD_PUSH_OBSERVED_1H_AGO, now=_NOW, grace=60
        )
        self.assertFalse(result["blocked"])
        result = _evaluate(
            push_observed_at=_HEAD_PUSH_OBSERVED_1H_AGO, now=_NOW, grace=61
        )
        self.assertTrue(result["blocked"])


class CodexReviewRequiredTest(unittest.TestCase):
    """条件3: Codex が現在の head SHA をレビュー済みであること (必須要件)。

    2026-09-07 の方針変更 (Issue #188 本文更新) により、Codex のレビューが
    マージの必須要件になった。`automerge-without-codex` ラベルでのみ免除
    できる。
    """

    def _reviews_with_codex(self, **codex_review_fields):
        node = {
            "state": "COMMENTED",
            "author": {"login": _CODEX_LOGIN, "__typename": "Bot"},
        }
        node.update(codex_review_fields)
        return {"pageInfo": {"hasNextPage": False}, "nodes": [node]}

    def test_no_codex_review_at_all_blocked(self) -> None:
        # pr_comments が空 (まだ @codex review をリクエストしていない) ため
        # 「自動リクエストする」判定になる (docs/decisions.md D91)。
        result = _evaluate(reviews=_EMPTY_REVIEWS, codex_bypass=False)
        self.assertTrue(result["blocked"])
        self.assertTrue(result["codex_review_request_needed"])
        self.assertIn(_HEAD_SHA[:7], result["reasons"][0])

    def test_codex_review_matches_head_sha_via_commit_oid_not_blocked(self) -> None:
        reviews = self._reviews_with_codex(commit={"oid": _HEAD_SHA})
        result = _evaluate(reviews=reviews, codex_bypass=False)
        self.assertFalse(result["blocked"])

    def test_codex_review_on_stale_commit_blocked(self) -> None:
        # PR #185 のケース: Codex がレビューしたのは古いコミットで、
        # その後 push された新しい head SHA には未対応。まだ自動リクエスト
        # していないため、リクエストする判定になる。
        stale_sha = "0" * 40
        reviews = self._reviews_with_codex(commit={"oid": stale_sha})
        result = _evaluate(reviews=reviews, codex_bypass=False)
        self.assertTrue(result["blocked"])
        self.assertTrue(result["codex_review_request_needed"])

    def test_codex_review_matches_via_reviewed_commit_body_text(self) -> None:
        # commit.oid が (何らかの理由で) 欠けていても、本文の
        # "Reviewed commit: <短縮SHA>" から突き合わせられる。
        reviews = self._reviews_with_codex(
            body="\n### 💡 Codex Review\n\n**Reviewed commit:** `80246e861a`\n",
        )
        result = _evaluate(reviews=reviews, codex_bypass=False)
        self.assertFalse(result["blocked"])

    def test_codex_review_body_text_stale_sha_blocked_with_hint(self) -> None:
        reviews = self._reviews_with_codex(
            body="**Reviewed commit:** `deadbeef00`\n",
        )
        result = _evaluate(reviews=reviews, codex_bypass=False)
        self.assertTrue(result["blocked"])
        self.assertIn("deadbeef00", result["reasons"][0])

    def test_codex_thumbs_up_reaction_after_push_not_blocked(self) -> None:
        # PR #191 (0バイトのファイルのみ追加) で実測済み: 指摘ゼロのとき
        # Codex はレビュー・コメントを一切残さず、PR本体への👍のみを付ける。
        reactions = {
            "pageInfo": {"hasNextPage": False},
            "nodes": [
                {
                    "createdAt": "2026-09-07T15:32:00Z",  # push (15:31:14) の後
                    "user": {"login": _CODEX_LOGIN, "__typename": "Bot"},
                }
            ],
        }
        result = _evaluate(
            reviews=_EMPTY_REVIEWS, reactions=reactions, codex_bypass=False
        )
        self.assertFalse(result["blocked"])

    def test_codex_thumbs_up_reaction_before_push_blocked(self) -> None:
        # 古い push に対する 👍 が残っているだけ (リアクションは SHA に
        # 紐付かないため createdAt で判定する)。
        reactions = {
            "pageInfo": {"hasNextPage": False},
            "nodes": [
                {
                    "createdAt": "2026-09-07T15:30:00Z",  # push (15:31:14) より前
                    "user": {"login": _CODEX_LOGIN, "__typename": "Bot"},
                }
            ],
        }
        result = _evaluate(
            reviews=_EMPTY_REVIEWS, reactions=reactions, codex_bypass=False
        )
        self.assertTrue(result["blocked"])

    def test_thumbs_up_from_non_codex_user_ignored(self) -> None:
        reactions = {
            "pageInfo": {"hasNextPage": False},
            "nodes": [
                {"createdAt": "2026-09-07T15:40:00Z", "user": {"login": "octocat"}}
            ],
        }
        result = _evaluate(
            reviews=_EMPTY_REVIEWS, reactions=reactions, codex_bypass=False
        )
        self.assertTrue(result["blocked"])

    def test_missing_reactions_data_falls_back_to_review_signal_only(self) -> None:
        reviews = self._reviews_with_codex(commit={"oid": _HEAD_SHA})
        result = _evaluate(reviews=reviews, reactions=None, codex_bypass=False)
        self.assertFalse(result["blocked"])

    def test_missing_reactions_data_without_review_still_blocked(self) -> None:
        result = _evaluate(
            reviews=_EMPTY_REVIEWS, reactions=None, codex_bypass=False
        )
        self.assertTrue(result["blocked"])

    def test_missing_head_sha_blocks_safely(self) -> None:
        reviews = self._reviews_with_codex(commit={"oid": _HEAD_SHA})
        result = _evaluate(reviews=reviews, head_sha=None, codex_bypass=False)
        self.assertTrue(result["blocked"])
        self.assertIn("head SHA を取得できなかった", result["reasons"][0])

    def test_codex_bypass_label_skips_requirement_even_without_review(self) -> None:
        result = _evaluate(reviews=_EMPTY_REVIEWS, codex_bypass=True)
        self.assertFalse(result["blocked"])

    def test_codex_bypass_label_does_not_skip_unresolved_threads(self) -> None:
        threads = {
            "pageInfo": {"hasNextPage": False},
            "nodes": [
                {
                    "isResolved": False,
                    "comments": {
                        "nodes": [{"path": "a.rs", "author": {"login": "octocat"}}]
                    },
                }
            ],
        }
        result = _evaluate(threads=threads, reviews=_EMPTY_REVIEWS, codex_bypass=True)
        self.assertTrue(result["blocked"])
        self.assertIn("未解決のレビュースレッド", result["reasons"][0])

    def test_codex_bypass_label_does_not_skip_changes_requested(self) -> None:
        reviews = {
            "pageInfo": {"hasNextPage": False},
            "nodes": [
                {"state": "CHANGES_REQUESTED", "author": {"login": "octocat"}}
            ],
        }
        result = _evaluate(reviews=reviews, codex_bypass=True)
        self.assertTrue(result["blocked"])
        self.assertIn("CHANGES_REQUESTED", result["reasons"][0])

    def test_reviews_fetch_failure_suppresses_duplicate_codex_reason(self) -> None:
        # latestReviews 自体が取得できなかった場合、条件2の理由 (安全側
        # ブロック) だけを出し、紛らわしい「Codex 待ち」は重ねて出さない。
        result = _evaluate(reviews=None, codex_bypass=False)
        self.assertTrue(result["blocked"])
        self.assertEqual(len(result["reasons"]), 1)
        self.assertIn("取得に失敗", result["reasons"][0])

    def test_multiple_codex_reviews_latest_entry_only(self) -> None:
        # latestReviews はレビュアーごとに最新の1件しか含まないため、
        # 複数ノードは通常発生しないが、念のため複数ノードでも1件でも
        # 一致すれば通ることを確認する。
        reviews = {
            "pageInfo": {"hasNextPage": False},
            "nodes": [
                {
                    "state": "COMMENTED",
                    "author": {"login": _CODEX_LOGIN, "__typename": "Bot"},
                    "commit": {"oid": "1" * 40},
                },
                {
                    "state": "APPROVED",
                    "author": {"login": "octocat"},
                },
            ],
        }
        result = _evaluate(
            reviews=reviews, head_sha="1" * 40, codex_bypass=False
        )
        self.assertFalse(result["blocked"])


class CodexUsageLimitAndAutoRequestTest(unittest.TestCase):
    """PR #192 の運用で判明した2つの前提への対応 (docs/decisions.md D91):

    1. Codex は push では再レビューしない (open/ready/`@codex review` の
       コメントでしか起動しない) — 未レビューの head SHA を検知したら
       `@codex review` を自動リクエストする (`codex_review_request_needed`)。
       同じ head SHA には1回だけリクエストする (マーカーコメントで重複
       防止)。
    2. Codex には利用上限がある — 上限到達メッセージを検知した場合、
       「この PR のいずれかの commit を Codex がレビュー済み」であれば
       条件3を緩和する (`codex_relaxed`。このクラスで検証)。それも無理な
       場合は Claude フォールバック (`ClaudeFallbackTest` で検証、
       2026-09-07 ユーザ決定) を試す。いずれも満たさず一度もレビューされて
       いない PR は緩和しない。未解決スレッド判定・CHANGES_REQUESTED 判定
       は独立した条件のため、緩和の影響を受けない。
    """

    def _request_marker_comment(self, head_sha: str, created_at: str) -> dict:
        return {
            "author": {"login": "github-actions[bot]", "__typename": "Bot"},
            "body": (
                "@codex review\n\n"
                f"<!-- auto-merge:codex-review-request:{head_sha} -->\n"
            ),
            "createdAt": created_at,
        }

    def _usage_limit_comment(
        self, created_at: str, login: str = _CODEX_LOGIN, typename: str = "Bot"
    ) -> dict:
        return {
            "author": {"login": login, "__typename": typename},
            "body": (
                "You have reached your Codex usage limits for code "
                "reviews. You can see your limits in the Codex usage "
                "dashboard."
            ),
            "createdAt": created_at,
        }

    def test_no_codex_review_yet_requests_review(self) -> None:
        # head SHA に Codex レビューが無く、まだリクエストもしていない
        # -> @codex review を投稿する判定になる。
        result = _evaluate(
            reviews=_EMPTY_REVIEWS,
            pr_comments=_EMPTY_PR_COMMENTS,
            codex_bypass=False,
        )
        self.assertTrue(result["blocked"])
        self.assertTrue(result["codex_review_request_needed"])
        self.assertFalse(result["codex_relaxed"])

    def test_already_requested_for_this_head_sha_does_not_repost(self) -> None:
        # 同じ head SHA に対して既にリクエスト済み (マーカーあり、上限
        # メッセージはまだ無い) -> 再投稿しない (request_needed=False) が、
        # 応答待ちとしてブロックは継続する。
        pr_comments = {
            "nodes": [
                self._request_marker_comment(_HEAD_SHA, "2026-09-07T16:00:00Z")
            ]
        }
        result = _evaluate(
            reviews=_EMPTY_REVIEWS, pr_comments=pr_comments, codex_bypass=False
        )
        self.assertTrue(result["blocked"])
        self.assertFalse(result["codex_review_request_needed"])
        self.assertFalse(result["codex_relaxed"])
        self.assertTrue(
            any("自動リクエスト済み" in r for r in result["reasons"])
        )

    def test_usage_limit_with_other_commit_reviewed_and_no_unresolved_threads_relaxes(
        self,
    ) -> None:
        # 上限メッセージあり + この PR の別 commit に Codex レビューあり +
        # 未解決スレッド無し -> 緩和してマージ可。
        pr_comments = {
            "nodes": [
                self._request_marker_comment(_HEAD_SHA, "2026-09-07T16:00:00Z"),
                self._usage_limit_comment("2026-09-07T16:01:00Z"),
            ]
        }
        reviews = {
            "pageInfo": {"hasNextPage": False},
            "nodes": [
                {
                    "state": "COMMENTED",
                    "author": {"login": _CODEX_LOGIN, "__typename": "Bot"},
                    "commit": {"oid": "9" * 40},  # 現在の head SHA とは別
                }
            ],
        }
        result = _evaluate(
            threads=_EMPTY_THREADS,
            reviews=reviews,
            pr_comments=pr_comments,
            codex_bypass=False,
        )
        self.assertFalse(result["blocked"])
        self.assertTrue(result["codex_relaxed"])
        self.assertIsNotNone(result["codex_relaxed_detail"])

    def test_usage_limit_but_unresolved_thread_still_blocks(self) -> None:
        # 上限メッセージあり + 未解決スレッドあり -> ブロック (条件1は
        # 緩和の対象外、独立して評価される)。
        pr_comments = {
            "nodes": [
                self._request_marker_comment(_HEAD_SHA, "2026-09-07T16:00:00Z"),
                self._usage_limit_comment("2026-09-07T16:01:00Z"),
            ]
        }
        reviews = {
            "pageInfo": {"hasNextPage": False},
            "nodes": [
                {
                    "state": "COMMENTED",
                    "author": {"login": _CODEX_LOGIN, "__typename": "Bot"},
                    "commit": {"oid": "9" * 40},
                }
            ],
        }
        threads = {
            "pageInfo": {"hasNextPage": False},
            "nodes": [
                {
                    "isResolved": False,
                    "comments": {
                        "nodes": [{"path": "a.rs", "author": {"login": "octocat"}}]
                    },
                }
            ],
        }
        result = _evaluate(
            threads=threads,
            reviews=reviews,
            pr_comments=pr_comments,
            codex_bypass=False,
        )
        self.assertTrue(result["blocked"])
        self.assertTrue(
            any("未解決のレビュースレッド" in r for r in result["reasons"])
        )

    def test_usage_limit_but_pr_never_reviewed_by_codex_blocks(self) -> None:
        # 上限メッセージあり + この PR に Codex レビューが1件も無い
        # -> 緩和せずブロック (一度も見ていない PR を通さない)。
        pr_comments = {
            "nodes": [
                self._request_marker_comment(_HEAD_SHA, "2026-09-07T16:00:00Z"),
                self._usage_limit_comment("2026-09-07T16:01:00Z"),
            ]
        }
        result = _evaluate(
            reviews=_EMPTY_REVIEWS, pr_comments=pr_comments, codex_bypass=False
        )
        self.assertTrue(result["blocked"])
        self.assertFalse(result["codex_relaxed"])
        self.assertTrue(
            any("一度も Codex にレビューされて" in r for r in result["reasons"])
        )

    def test_usage_limit_message_from_non_codex_author_ignored(self) -> None:
        # 上限メッセージの投稿者が Codex 以外 (なりすまし/第三者の悪戯) の
        # 場合は緩和しない — 応答待ちのまま。
        pr_comments = {
            "nodes": [
                self._request_marker_comment(_HEAD_SHA, "2026-09-07T16:00:00Z"),
                self._usage_limit_comment(
                    "2026-09-07T16:01:00Z", login="octocat", typename="User"
                ),
            ]
        }
        reviews = {
            "pageInfo": {"hasNextPage": False},
            "nodes": [
                {
                    "state": "COMMENTED",
                    "author": {"login": _CODEX_LOGIN, "__typename": "Bot"},
                    "commit": {"oid": "9" * 40},
                }
            ],
        }
        result = _evaluate(
            reviews=reviews, pr_comments=pr_comments, codex_bypass=False
        )
        self.assertTrue(result["blocked"])
        self.assertFalse(result["codex_relaxed"])
        self.assertFalse(result["codex_review_request_needed"])

    def test_usage_limit_before_request_comment_ignored(self) -> None:
        # 上限メッセージが「現在の head への @codex review リクエスト」より
        # 前に投稿されたものだと緩和条件を満たさない (過去の別のリクエスト
        # に対する上限メッセージを使い回さない)。
        pr_comments = {
            "nodes": [
                self._usage_limit_comment("2026-09-07T15:00:00Z"),  # リクエスト前
                self._request_marker_comment(_HEAD_SHA, "2026-09-07T16:00:00Z"),
            ]
        }
        reviews = {
            "pageInfo": {"hasNextPage": False},
            "nodes": [
                {
                    "state": "COMMENTED",
                    "author": {"login": _CODEX_LOGIN, "__typename": "Bot"},
                    "commit": {"oid": "9" * 40},
                }
            ],
        }
        result = _evaluate(
            reviews=reviews, pr_comments=pr_comments, codex_bypass=False
        )
        self.assertTrue(result["blocked"])
        self.assertFalse(result["codex_relaxed"])

    def test_pr_comments_fetch_failure_blocks_safely_and_does_not_request(
        self,
    ) -> None:
        result = _evaluate(
            reviews=_EMPTY_REVIEWS, pr_comments=None, codex_bypass=False
        )
        self.assertTrue(result["blocked"])
        self.assertFalse(result["codex_review_request_needed"])
        self.assertFalse(result["codex_relaxed"])

    def test_different_head_sha_requires_new_request(self) -> None:
        # head SHA が変わった (新しい push) 場合、古い head SHA へのマーカー
        # は一致しないため、新しい head SHA に対して改めてリクエストが
        # 必要になる (1 push = 1 リクエスト、永続的な緩和にはならない)。
        old_sha = "1" * 40
        pr_comments = {
            "nodes": [
                self._request_marker_comment(old_sha, "2026-09-07T16:00:00Z"),
                self._usage_limit_comment("2026-09-07T16:01:00Z"),
            ]
        }
        result = _evaluate(
            reviews=_EMPTY_REVIEWS,
            pr_comments=pr_comments,
            head_sha=_HEAD_SHA,  # old_sha とは異なる新しい head
            codex_bypass=False,
        )
        self.assertTrue(result["blocked"])
        self.assertTrue(result["codex_review_request_needed"])

    def test_request_comment_body_contains_trigger_and_marker(self) -> None:
        from check_review_gate import codex_review_request_comment_body

        body = codex_review_request_comment_body(_HEAD_SHA)
        self.assertIn("@codex review", body)
        self.assertIn(
            f"<!-- auto-merge:codex-review-request:{_HEAD_SHA} -->", body
        )


class ClaudeFallbackTest(unittest.TestCase):
    """Codex の利用上限到達時、この PR が一度も Codex にレビューされて
    いない場合の Claude フォールバック (2026-09-07 ユーザ決定、
    docs/decisions.md D91)。

    Claude のレビュアーを何と識別するかが最大の設計課題だったため、
    `_CLAUDE_LOGINS` のようなハードコードは行わず、呼び出し側
    (`claude_logins` 引数、workflow の `env.CLAUDE_REVIEWER_LOGINS`) で
    明示的に設定されたログインだけを許可する。**未設定 (既定の空集合) の
    場合は Claude 経路が常に不成立になる**ことをこのクラスの複数のテストで
    確認する — 「未設定なのに何となく通る」実装になっていないことの
    直接的な検証。
    """

    _CLAUDE_LOGIN = "claude[bot]"

    def _codex_request_marker_comment(self, head_sha: str, created_at: str) -> dict:
        return {
            "author": {"login": "github-actions[bot]", "__typename": "Bot"},
            "body": (
                "@codex review\n\n"
                f"<!-- auto-merge:codex-review-request:{head_sha} -->\n"
            ),
            "createdAt": created_at,
        }

    def _codex_usage_limit_comment(self, created_at: str) -> dict:
        return {
            "author": {"login": _CODEX_LOGIN, "__typename": "Bot"},
            "body": "You have reached your Codex usage limits for code reviews.",
            "createdAt": created_at,
        }

    def _claude_request_marker_comment(self, head_sha: str, created_at: str) -> dict:
        return {
            "author": {"login": "github-actions[bot]", "__typename": "Bot"},
            "body": (
                "@claude この PR のレビューをお願いします。\n\n"
                f"<!-- auto-merge:claude-review-request:{head_sha} -->\n"
            ),
            "createdAt": created_at,
        }

    def _usage_limit_state_comments(self) -> list[dict]:
        """「上限検知済み・この PR は一度も Codex にレビューされていない」
        状態を再現する共通のコメント列 (Codex 依頼 + 上限メッセージのみ)。
        """
        return [
            self._codex_request_marker_comment(
                _HEAD_SHA, "2026-09-07T16:00:00Z"
            ),
            self._codex_usage_limit_comment("2026-09-07T16:01:00Z"),
        ]

    def test_claude_configured_and_reviewed_head_sha_merges(self) -> None:
        # 上限検知 + Claude 許可リスト設定済み + head SHA への Claude
        # レビューあり -> マージ可。
        pr_comments = {"nodes": self._usage_limit_state_comments()}
        reviews = {
            "pageInfo": {"hasNextPage": False},
            "nodes": [
                {
                    "state": "COMMENTED",
                    "author": {"login": self._CLAUDE_LOGIN},
                    "commit": {"oid": _HEAD_SHA},
                }
            ],
        }
        result = _evaluate(
            reviews=reviews,
            pr_comments=pr_comments,
            codex_bypass=False,
            claude_logins=frozenset({self._CLAUDE_LOGIN}),
        )
        self.assertFalse(result["blocked"])
        self.assertTrue(result["claude_relaxed"])
        self.assertIsNotNone(result["claude_relaxed_detail"])
        self.assertFalse(result["claude_review_request_needed"])

    def test_claude_configured_but_not_reviewed_yet_blocks_and_requests(
        self,
    ) -> None:
        # 上限検知 + Claude 許可リスト設定済み + Claude レビュー無し
        # -> ブロック (依頼コメントを投稿する判定になる)。
        pr_comments = {"nodes": self._usage_limit_state_comments()}
        result = _evaluate(
            reviews=_EMPTY_REVIEWS,
            pr_comments=pr_comments,
            codex_bypass=False,
            claude_logins=frozenset({self._CLAUDE_LOGIN}),
        )
        self.assertTrue(result["blocked"])
        self.assertTrue(result["claude_review_request_needed"])
        self.assertFalse(result["claude_relaxed"])

    def test_claude_allowlist_unset_does_not_satisfy_even_with_matching_comment(
        self,
    ) -> None:
        # 上限検知 + Claude 許可リスト未設定 -> Claude 経路では充足しない。
        # たとえ「Claude らしき」ログインのレビューが実際に head SHA に
        # 付いていても、claude_logins が空なら一切考慮しない。
        pr_comments = {"nodes": self._usage_limit_state_comments()}
        reviews = {
            "pageInfo": {"hasNextPage": False},
            "nodes": [
                {
                    "state": "COMMENTED",
                    "author": {"login": self._CLAUDE_LOGIN},
                    "commit": {"oid": _HEAD_SHA},
                }
            ],
        }
        result = _evaluate(
            reviews=reviews,
            pr_comments=pr_comments,
            codex_bypass=False,
            claude_logins=frozenset(),  # 未設定
        )
        self.assertTrue(result["blocked"])
        self.assertFalse(result["claude_relaxed"])
        self.assertFalse(result["claude_review_request_needed"])
        self.assertTrue(
            any("Claude 許可リスト" in r for r in result["reasons"])
        )

    def test_claude_request_comment_not_reposted_for_same_head_sha(self) -> None:
        # 同じ head SHA への依頼コメントを重複投稿しない
        # (claude_review_request_needed=False、応答待ちのまま)。
        pr_comments = {
            "nodes": self._usage_limit_state_comments()
            + [
                self._claude_request_marker_comment(
                    _HEAD_SHA, "2026-09-07T16:02:00Z"
                )
            ]
        }
        result = _evaluate(
            reviews=_EMPTY_REVIEWS,
            pr_comments=pr_comments,
            codex_bypass=False,
            claude_logins=frozenset({self._CLAUDE_LOGIN}),
        )
        self.assertTrue(result["blocked"])
        self.assertFalse(result["claude_review_request_needed"])
        self.assertTrue(
            any("依頼済み" in r for r in result["reasons"])
        )

    def test_usage_limit_with_unresolved_thread_still_blocks_even_with_claude_review(
        self,
    ) -> None:
        # 上限検知 + 未解決スレッドあり -> ブロック
        # (Claude レビューがあっても未解決スレッド判定は緩めない)。
        pr_comments = {"nodes": self._usage_limit_state_comments()}
        reviews = {
            "pageInfo": {"hasNextPage": False},
            "nodes": [
                {
                    "state": "COMMENTED",
                    "author": {"login": self._CLAUDE_LOGIN},
                    "commit": {"oid": _HEAD_SHA},
                }
            ],
        }
        threads = {
            "pageInfo": {"hasNextPage": False},
            "nodes": [
                {
                    "isResolved": False,
                    "comments": {
                        "nodes": [{"path": "a.rs", "author": {"login": "octocat"}}]
                    },
                }
            ],
        }
        result = _evaluate(
            threads=threads,
            reviews=reviews,
            pr_comments=pr_comments,
            codex_bypass=False,
            claude_logins=frozenset({self._CLAUDE_LOGIN}),
        )
        self.assertTrue(result["blocked"])
        # 条件3自体は Claude レビューで緩和されているはずだが、条件1で
        # 独立してブロックされる。
        self.assertTrue(result["claude_relaxed"])
        self.assertTrue(
            any("未解決のレビュースレッド" in r for r in result["reasons"])
        )

    def test_reviewer_not_in_claude_allowlist_does_not_satisfy(self) -> None:
        # Claude 許可リストに無い投稿者のレビュー -> 充足しない。
        pr_comments = {"nodes": self._usage_limit_state_comments()}
        reviews = {
            "pageInfo": {"hasNextPage": False},
            "nodes": [
                {
                    "state": "COMMENTED",
                    "author": {"login": "some-other-bot"},
                    "commit": {"oid": _HEAD_SHA},
                }
            ],
        }
        result = _evaluate(
            reviews=reviews,
            pr_comments=pr_comments,
            codex_bypass=False,
            claude_logins=frozenset({self._CLAUDE_LOGIN}),
        )
        self.assertTrue(result["blocked"])
        self.assertFalse(result["claude_relaxed"])
        self.assertTrue(result["claude_review_request_needed"])

    def test_claude_comment_after_request_does_not_relax(self) -> None:
        # Issue #194: 依頼より後の Claude のコメントは緩和シグナルに
        # しない。`claude-code-action` は起動直後に進捗コメント
        # (Issue #203) を投稿するため、これを数えると「レビューが 1 文字も
        # 書かれていない時点でマージ要件が緩和される」。同一コメントが
        # 編集され続けるため createdAt では進捗中と完了後を区別できない。
        pr_comments = {
            "nodes": self._usage_limit_state_comments()
            + [
                self._claude_request_marker_comment(
                    _HEAD_SHA, "2026-09-07T16:02:00Z"
                ),
                {
                    "author": {"login": self._CLAUDE_LOGIN},
                    "body": "確認しました。特に問題は見当たりません。",
                    "createdAt": "2026-09-07T16:05:00Z",
                },
            ]
        }
        result = _evaluate(
            reviews=_EMPTY_REVIEWS,
            pr_comments=pr_comments,
            codex_bypass=False,
            claude_logins=frozenset({self._CLAUDE_LOGIN}),
        )
        self.assertTrue(result["blocked"])
        self.assertFalse(result["claude_relaxed"])
        # 依頼コメントは既にあるので再投稿はしない (応答待ち)。
        self.assertFalse(result["claude_review_request_needed"])

    def test_claude_progress_comment_does_not_relax(self) -> None:
        # Issue #203 で実測された進捗コメントそのものの形。これが緩和を
        # 成立させてしまうと、PR #198 のように起動後 104ms で
        # is_error:true で終了した (= レビューが行われなかった) ケースでも
        # マージ要件が満たされてしまう。
        pr_comments = {
            "nodes": self._usage_limit_state_comments()
            + [
                self._claude_request_marker_comment(
                    _HEAD_SHA, "2026-09-07T16:02:00Z"
                ),
                {
                    "author": {"login": self._CLAUDE_LOGIN},
                    "body": "Claude is working…",
                    "createdAt": "2026-09-07T16:02:30Z",
                },
            ]
        }
        result = _evaluate(
            reviews=_EMPTY_REVIEWS,
            pr_comments=pr_comments,
            codex_bypass=False,
            claude_logins=frozenset({self._CLAUDE_LOGIN}),
        )
        self.assertTrue(result["blocked"])
        self.assertFalse(result["claude_relaxed"])

    def test_claude_review_on_an_older_commit_does_not_relax(self) -> None:
        # 緩和は「現在の head SHA へのレビュー」に限る。古い commit への
        # レビューで通してしまうと、push 後の未レビュー差分が素通りする。
        pr_comments = {
            "nodes": self._usage_limit_state_comments()
            + [
                self._claude_request_marker_comment(
                    _HEAD_SHA, "2026-09-07T16:02:00Z"
                ),
            ]
        }
        reviews = {
            "pageInfo": {"hasNextPage": False},
            "nodes": [
                {
                    "state": "COMMENTED",
                    "author": {"login": self._CLAUDE_LOGIN},
                    "commit": {"oid": "0" * 40},
                }
            ],
        }
        result = _evaluate(
            reviews=reviews,
            pr_comments=pr_comments,
            codex_bypass=False,
            claude_logins=frozenset({self._CLAUDE_LOGIN}),
        )
        self.assertTrue(result["blocked"])
        self.assertFalse(result["claude_relaxed"])

    def test_waiting_reason_carries_a_usage_limit_diagnostic(self) -> None:
        # Issue #194 / PR #209: 「@codex review は投稿済み、でも上限
        # メッセージを検知していない」状態のとき、それが「まだ返信が
        # 無い」のか「返信を取りこぼした」のかをログから区別できる
        # ようにする。判定そのものは変えない。
        pr_comments = {
            "nodes": [
                {
                    "body": (
                        "@codex review\n"
                        f"<!-- auto-merge:codex-review-request:{_HEAD_SHA} -->"
                    ),
                    "createdAt": "2026-09-07T16:00:00Z",
                    "author": {"login": "noan98", "__typename": "User"},
                }
            ]
        }
        result = _evaluate(
            reviews=_EMPTY_REVIEWS,
            pr_comments=pr_comments,
            codex_bypass=False,
            claude_logins=frozenset({self._CLAUDE_LOGIN}),
        )
        waiting = [r for r in result["reasons"] if "応答を待っています" in r]
        self.assertEqual(len(waiting), 1)
        self.assertIn("診断:", waiting[0])
        self.assertIn("Codex ログイン一致0件", waiting[0])
        # 実際に観測した著者名を出す (null かログイン名違いかの区別用)。
        self.assertIn("観測した著者=noan98", waiting[0])

    def test_usage_limit_diagnostic_distinguishes_a_rejected_author(self) -> None:
        # ログイン名は一致するが `__typename` が Bot でないケース。
        # 「Codex は返信しているが著者判定で落ちている」と読める
        # 内訳が出ること。
        pr_comments = {
            "nodes": [
                {
                    "body": (
                        "@codex review\n"
                        f"<!-- auto-merge:codex-review-request:{_HEAD_SHA} -->"
                    ),
                    "createdAt": "2026-09-07T16:00:00Z",
                    "author": {"login": "noan98", "__typename": "User"},
                },
                {
                    "body": "You have reached your Codex usage limits for code reviews.",
                    "createdAt": "2026-09-07T16:00:30Z",
                    "author": {
                        "login": "chatgpt-codex-connector[bot]",
                        "__typename": "User",
                    },
                },
            ]
        }
        result = _evaluate(
            reviews=_EMPTY_REVIEWS,
            pr_comments=pr_comments,
            codex_bypass=False,
            claude_logins=frozenset({self._CLAUDE_LOGIN}),
        )
        waiting = [r for r in result["reasons"] if "応答を待っています" in r]
        self.assertEqual(len(waiting), 1)
        self.assertIn("Codex ログイン一致1件", waiting[0])
        self.assertIn("__typename=User", waiting[0])
        self.assertIn("chatgpt-codex-connector[bot]", waiting[0])
        self.assertIn("著者判定通過0件", waiting[0])
        # 判定そのものは変わらない (安全側でブロックのまま)。
        self.assertTrue(result["blocked"])
        self.assertFalse(result["claude_review_request_needed"])

    def test_request_comment_body_contains_title_and_marker(self) -> None:
        from check_review_gate import claude_review_request_comment_body

        body = claude_review_request_comment_body(_HEAD_SHA, "テストPR")
        self.assertIn("@claude", body)
        self.assertIn("テストPR", body)
        self.assertIn(
            f"<!-- auto-merge:claude-review-request:{_HEAD_SHA} -->", body
        )

    def test_request_comment_body_without_title(self) -> None:
        from check_review_gate import claude_review_request_comment_body

        body = claude_review_request_comment_body(_HEAD_SHA, None)
        self.assertIn("@claude", body)


class GraphQLBotLoginSpellingTest(unittest.TestCase):
    """Issue #194 / PR #209: GraphQL の `Bot` アクタの `login` には
    `[bot]` が付かない。

    REST は `chatgpt-codex-connector[bot]` を返すが、このスクリプトが
    読む GraphQL は `chatgpt-codex-connector` を返す。許可リストは
    REST 表記で書かれていたため、**Codex のコメント・レビュー・👍 が
    1 件も照合されていなかった**。PR #209 の診断行で実測:

        観測した著者=chatgpt-codex-connector,noan98 / Codex ログイン一致0件

    このクラスの各テストは、`[bot]` の正規化が無いと FAIL する。
    """

    _GRAPHQL_CODEX = "chatgpt-codex-connector"  # `[bot]` なし

    def test_graphql_spelling_counts_as_a_codex_review(self) -> None:
        reviews = {
            "pageInfo": {"hasNextPage": False},
            "nodes": [
                {
                    "state": "COMMENTED",
                    "author": {"login": self._GRAPHQL_CODEX, "__typename": "Bot"},
                    "commit": {"oid": _HEAD_SHA},
                }
            ],
        }
        result = _evaluate(reviews=reviews, codex_bypass=False)
        self.assertFalse(result["codex_review_request_needed"])

    def test_graphql_spelling_usage_limit_triggers_claude_fallback(self) -> None:
        # PR #209 で実際に起きていた状況そのもの。
        pr_comments = {
            "nodes": [
                {
                    "body": (
                        "@codex review\n"
                        f"<!-- auto-merge:codex-review-request:{_HEAD_SHA} -->"
                    ),
                    "createdAt": "2026-09-07T16:00:00Z",
                    "author": {"login": "noan98", "__typename": "User"},
                },
                {
                    "body": "You have reached your Codex usage limits for code reviews.",
                    "createdAt": "2026-09-07T16:00:10Z",
                    "author": {"login": self._GRAPHQL_CODEX, "__typename": "Bot"},
                },
            ]
        }
        result = _evaluate(
            reviews=_EMPTY_REVIEWS,
            pr_comments=pr_comments,
            codex_bypass=False,
            claude_logins=frozenset({"claude[bot]"}),
        )
        self.assertTrue(result["claude_review_request_needed"])

    def test_graphql_spelling_claude_review_relaxes(self) -> None:
        # 許可リストは REST 表記 `claude[bot]`、実データは GraphQL 表記
        # `claude`。正規化が無いと緩和が成立しない。
        pr_comments = {
            "nodes": [
                {
                    "body": (
                        "@codex review\n"
                        f"<!-- auto-merge:codex-review-request:{_HEAD_SHA} -->"
                    ),
                    "createdAt": "2026-09-07T16:00:00Z",
                    "author": {"login": "noan98", "__typename": "User"},
                },
                {
                    "body": "You have reached your Codex usage limits for code reviews.",
                    "createdAt": "2026-09-07T16:00:10Z",
                    "author": {"login": self._GRAPHQL_CODEX, "__typename": "Bot"},
                },
                {
                    "body": (
                        "@claude レビューをお願いします\n"
                        f"<!-- auto-merge:claude-review-request:{_HEAD_SHA} -->"
                    ),
                    "createdAt": "2026-09-07T16:01:00Z",
                    "author": {"login": "noan98", "__typename": "User"},
                },
            ]
        }
        reviews = {
            "pageInfo": {"hasNextPage": False},
            "nodes": [
                {
                    "state": "COMMENTED",
                    "author": {"login": "claude", "__typename": "Bot"},
                    "commit": {"oid": _HEAD_SHA},
                }
            ],
        }
        result = _evaluate(
            reviews=reviews,
            pr_comments=pr_comments,
            codex_bypass=False,
            claude_logins=frozenset({"claude[bot]"}),
        )
        self.assertTrue(result["claude_relaxed"])

    def test_a_human_user_named_claude_does_not_satisfy_the_gate(self) -> None:
        # `[bot]` を落として比較するようにした副作用の封じ込め
        # (Issue #194): `claude` という **ユーザ** アカウントのレビューは
        # 許可リストに一致してはならない。`__typename == "Bot"` で弾く。
        pr_comments = {
            "nodes": [
                {
                    "body": (
                        "@codex review\n"
                        f"<!-- auto-merge:codex-review-request:{_HEAD_SHA} -->"
                    ),
                    "createdAt": "2026-09-07T16:00:00Z",
                    "author": {"login": "noan98", "__typename": "User"},
                },
                {
                    "body": "You have reached your Codex usage limits for code reviews.",
                    "createdAt": "2026-09-07T16:00:10Z",
                    "author": {"login": self._GRAPHQL_CODEX, "__typename": "Bot"},
                },
            ]
        }
        reviews = {
            "pageInfo": {"hasNextPage": False},
            "nodes": [
                {
                    "state": "COMMENTED",
                    "author": {"login": "claude", "__typename": "User"},
                    "commit": {"oid": _HEAD_SHA},
                }
            ],
        }
        result = _evaluate(
            reviews=reviews,
            pr_comments=pr_comments,
            codex_bypass=False,
            claude_logins=frozenset({"claude[bot]"}),
        )
        self.assertFalse(result["claude_relaxed"])
        self.assertTrue(result["blocked"])

    def test_lookalike_is_still_rejected_after_normalization(self) -> None:
        # PR #192 の P2 指摘は守られたままであること。正規化しても
        # `chatgpt-codex-connector-review` は一致しない。
        reviews = {
            "pageInfo": {"hasNextPage": False},
            "nodes": [
                {
                    "state": "COMMENTED",
                    "author": {
                        "login": "chatgpt-codex-connector-review",
                        "__typename": "Bot",
                    },
                    "commit": {"oid": _HEAD_SHA},
                }
            ],
        }
        result = _evaluate(reviews=reviews, codex_bypass=False)
        self.assertTrue(result["codex_review_request_needed"])


class CodexLoginExactMatchTest(unittest.TestCase):
    """P2 (2026-09-07 の Codex レビュー指摘、PR #192): ログイン判定は完全
    一致でなければならない。前方一致だと `chatgpt-codex-connector-review`
    のような別名アカウントの 👍/レビューで必須要件がすり抜けてしまう。
    このクラスの各テストは、判定が前方一致 (`str.startswith`) のままだと
    FAIL し、完全一致に直したことで PASS する。
    """

    def test_lookalike_login_prefix_review_not_accepted(self) -> None:
        # "chatgpt-codex-connector-review" は
        # "chatgpt-codex-connector" で始まるが別のアカウント。
        reviews = {
            "pageInfo": {"hasNextPage": False},
            "nodes": [
                {
                    "state": "COMMENTED",
                    "author": {
                        "login": "chatgpt-codex-connector-review",
                        "__typename": "User",
                    },
                    "commit": {"oid": _HEAD_SHA},
                    "body": "**Reviewed commit:** `80246e861a`",
                }
            ],
        }
        result = _evaluate(reviews=reviews, codex_bypass=False)
        self.assertTrue(result["blocked"])
        # 別名アカウントのレビューは無視され、正規の Codex は未レビュー
        # 扱いになるため「自動リクエストする」判定になる。
        self.assertTrue(result["codex_review_request_needed"])
        self.assertIn(_HEAD_SHA[:7], result["reasons"][0])

    def test_lookalike_login_thumbs_up_not_accepted(self) -> None:
        reactions = {
            "pageInfo": {"hasNextPage": False},
            "nodes": [
                {
                    "createdAt": "2026-09-07T15:32:00Z",
                    "user": {
                        "login": "chatgpt-codex-connector-imposter",
                        "__typename": "User",
                    },
                }
            ],
        }
        result = _evaluate(
            reviews=_EMPTY_REVIEWS, reactions=reactions, codex_bypass=False
        )
        self.assertTrue(result["blocked"])

    def test_exact_login_match_still_accepted(self) -> None:
        # 完全一致に厳格化しても、正規の Codex ログインは引き続き通る
        # (回帰していないことの確認)。
        reviews = {
            "pageInfo": {"hasNextPage": False},
            "nodes": [
                {
                    "state": "COMMENTED",
                    "author": {"login": _CODEX_LOGIN, "__typename": "Bot"},
                    "commit": {"oid": _HEAD_SHA},
                }
            ],
        }
        result = _evaluate(reviews=reviews, codex_bypass=False)
        self.assertFalse(result["blocked"])

    def test_login_match_case_insensitive(self) -> None:
        # ログイン名の大小文字ゆらぎは許容する (完全一致だが大小無視)。
        reviews = {
            "pageInfo": {"hasNextPage": False},
            "nodes": [
                {
                    "state": "COMMENTED",
                    "author": {
                        "login": _CODEX_LOGIN.upper(),
                        "__typename": "Bot",
                    },
                    "commit": {"oid": _HEAD_SHA},
                }
            ],
        }
        result = _evaluate(reviews=reviews, codex_bypass=False)
        self.assertFalse(result["blocked"])

    def test_correct_login_but_wrong_typename_rejected(self) -> None:
        # __typename が取得できていて "Bot" 以外なら、ログイン名が一致
        # していても拒否する (なりすましアカウントへの追加防御)。
        reviews = {
            "pageInfo": {"hasNextPage": False},
            "nodes": [
                {
                    "state": "COMMENTED",
                    "author": {"login": _CODEX_LOGIN, "__typename": "User"},
                    "commit": {"oid": _HEAD_SHA},
                }
            ],
        }
        result = _evaluate(reviews=reviews, codex_bypass=False)
        self.assertTrue(result["blocked"])

    def test_missing_typename_field_does_not_block(self) -> None:
        # __typename を取得していない (テストデータや将来の互換性) 場合は
        # ログイン名の完全一致だけで判定する。
        reviews = {
            "pageInfo": {"hasNextPage": False},
            "nodes": [
                {
                    "state": "COMMENTED",
                    "author": {"login": _CODEX_LOGIN},
                    "commit": {"oid": _HEAD_SHA},
                }
            ],
        }
        result = _evaluate(reviews=reviews, codex_bypass=False)
        self.assertFalse(result["blocked"])


class PushObservedAtSecurityRegressionTest(unittest.TestCase):
    """P1 (2026-09-07 の Codex レビュー指摘、PR #192): 猶予期間とシグナルc
    の基準時刻に git commit の committer date を使ってはいけない。
    committer date は「commit をローカルで作った時刻」であり「GitHub に
    push された時刻」ではないため、ローカルで古い日時のコミットを作って
    今 push する (cherry-pick/rebase でも起こる) 操作で、(a) 猶予期間が
    即座に満たされる、(b) 以前の head に付いていた古い 👍 のリアクションが
    「現在の head をレビュー済み」と誤認される、という2つの防御が同時に
    破られる。

    この単体テストは committer date と push 観測時刻の違いそのものは検証
    できない (どちらも `evaluate_review_gate` にとってはただの ISO8601
    文字列であり、区別する情報を持たない — 修正の本体は呼び出し側
    `review_gate_decision.sh` が渡す値を committer date から check-suites
    の作成時刻に変更したことにある。docs/decisions.md D91 参照)。
    ここでは代わりに、呼び出し側が正しく「直近の push 観測時刻」を渡した
    場合に、シナリオどおり安全側の判定になることを確認する。
    """

    def test_reaction_from_before_a_recent_push_is_rejected(self) -> None:
        # シナリオ: 攻撃者 (あるいは単なる rebase) が過去の日時のコミットを
        # 用意し、それを今 push した。もし誤って committer date
        # (= 過去の日時) を基準にしていれば、その過去の日時の直後に付いた
        # 古い 👍 のリアクションが「現在の head をレビュー済み」と誤認され、
        # 猶予期間も同時に即座に満たされてしまう。
        #
        # ここでは呼び出し側が正しい値 (直近の push 観測時刻、6分前) を
        # 渡した場合を検証する: 古い 👍 (push 観測時刻より前) はシグナルc
        # を満たさず、かつ push 観測から6分しか経っていないため猶予期間も
        # 満たさない — 結果として正しくブロックされる。
        recent_push_observed_at = "2026-09-07T16:25:00Z"  # 実際の push 観測 (6分前)
        now = "2026-09-07T16:31:00Z"
        stale_reaction = {
            "pageInfo": {"hasNextPage": False},
            "nodes": [
                {
                    # 古い head (attacker が用意した過去の committer date
                    # 相当) の直後に付いていた 👍。
                    "createdAt": "2026-09-07T15:05:00Z",
                    "user": {"login": _CODEX_LOGIN, "__typename": "Bot"},
                }
            ],
        }

        result = evaluate_review_gate(
            review_threads=_EMPTY_THREADS,
            latest_reviews=_EMPTY_REVIEWS,
            head_push_observed_at=recent_push_observed_at,
            now=now,
            grace_period_minutes=15,
            reactions=stale_reaction,
            head_sha=_HEAD_SHA,
            codex_bypass=False,
            pr_comments=_EMPTY_PR_COMMENTS,
        )
        self.assertTrue(result["blocked"])
        # Codex 待ち (古い👍は不成立 -> 自動リクエストする判定) + 猶予期間
        # (6分しか経っていない) の2件がともに正しくブロック理由になって
        # いること。
        self.assertEqual(len(result["reasons"]), 2)
        self.assertTrue(result["codex_review_request_needed"])
        self.assertTrue(any("猶予期間" in r for r in result["reasons"]))

    def test_reaction_after_recent_push_is_accepted(self) -> None:
        # 対照: push 観測時刻より後に付いた 👍 は正しく受理される
        # (誤検出だけでなく、正当なケースを壊していないことも確認する)。
        recent_push_observed_at = "2026-09-07T16:25:00Z"
        now = "2026-09-07T16:41:00Z"  # push から16分後 (猶予期間も満了)
        fresh_reaction = {
            "pageInfo": {"hasNextPage": False},
            "nodes": [
                {
                    "createdAt": "2026-09-07T16:26:00Z",  # push の1分後
                    "user": {"login": _CODEX_LOGIN, "__typename": "Bot"},
                }
            ],
        }

        result = evaluate_review_gate(
            review_threads=_EMPTY_THREADS,
            latest_reviews=_EMPTY_REVIEWS,
            head_push_observed_at=recent_push_observed_at,
            now=now,
            grace_period_minutes=15,
            reactions=fresh_reaction,
            head_sha=_HEAD_SHA,
            codex_bypass=False,
            pr_comments=_EMPTY_PR_COMMENTS,
        )
        self.assertFalse(result["blocked"])


class MultipleReasonsTest(unittest.TestCase):
    def test_thread_and_changes_requested_reported_together(self) -> None:
        threads = {
            "pageInfo": {"hasNextPage": False},
            "nodes": [
                {
                    "isResolved": False,
                    "comments": {
                        "nodes": [{"path": "a.rs", "author": {"login": "u1"}}]
                    },
                }
            ],
        }
        reviews = {
            "pageInfo": {"hasNextPage": False},
            "nodes": [{"state": "CHANGES_REQUESTED", "author": {"login": "u2"}}],
        }
        result = _evaluate(
            threads=threads,
            reviews=reviews,
            push_observed_at="2026-09-07T16:30:00Z",
        )
        self.assertTrue(result["blocked"])
        self.assertEqual(len(result["reasons"]), 3)  # 未解決 + CHANGES_REQUESTED + 猶予期間

    def test_all_four_conditions_reported_together(self) -> None:
        threads = {
            "pageInfo": {"hasNextPage": False},
            "nodes": [
                {
                    "isResolved": False,
                    "comments": {
                        "nodes": [{"path": "a.rs", "author": {"login": "u1"}}]
                    },
                }
            ],
        }
        reviews = {
            "pageInfo": {"hasNextPage": False},
            "nodes": [{"state": "CHANGES_REQUESTED", "author": {"login": "u2"}}],
        }
        result = evaluate_review_gate(
            threads,
            reviews,
            "2026-09-07T16:30:00Z",
            _NOW,
            _GRACE,
            reactions=_EMPTY_REACTIONS,
            head_sha=_HEAD_SHA,
            codex_bypass=False,
            pr_comments=_EMPTY_PR_COMMENTS,
        )
        self.assertTrue(result["blocked"])
        self.assertEqual(len(result["reasons"]), 4)
        self.assertTrue(result["codex_review_request_needed"])


class PR185RegressionTest(unittest.TestCase):
    """PR #185 の実例 (Issue #188 本文のタイムライン) を再現する回帰テスト。

    15:31:14 PR作成 -> 15:35:29 Codex がレビュー投稿 (head SHA
    80246e861a... に対する未解決スレッド) -> 15:37:55 (作成から6.7分後、
    猶予期間15分未満) に旧実装はマージしていた。新しい判定では、
    Codex レビュー自体は head SHA に一致しているため条件3は満たすが、
    未解決スレッドと猶予期間の2つでブロックされるべき。
    """

    def test_pr185_timeline_is_blocked(self) -> None:
        threads = {
            "pageInfo": {"hasNextPage": False},
            "nodes": [
                {
                    "isResolved": False,
                    "comments": {
                        "nodes": [
                            {
                                "path": "src/browser/suspension.rs",
                                "author": {"login": "chatgpt-codex-connector"},
                            }
                        ]
                    },
                }
            ],
        }
        reviews = {
            "pageInfo": {"hasNextPage": False},
            "nodes": [
                {
                    "state": "COMMENTED",
                    "author": {"login": _CODEX_LOGIN, "__typename": "Bot"},
                    "commit": {"oid": _HEAD_SHA},
                    "body": "**Reviewed commit:** `80246e861a`",
                }
            ],
        }
        result = evaluate_review_gate(
            review_threads=threads,
            latest_reviews=reviews,
            head_push_observed_at="2026-09-07T15:31:14Z",
            now="2026-09-07T15:37:55Z",
            grace_period_minutes=15,
            reactions=_EMPTY_REACTIONS,
            head_sha=_HEAD_SHA,
            codex_bypass=False,
            pr_comments=_EMPTY_PR_COMMENTS,
        )
        self.assertTrue(result["blocked"])
        self.assertEqual(len(result["reasons"]), 2)  # 未解決スレッド + 猶予期間
        # Codex 自体は head SHA を正しくレビュー済みなので、自動リクエストは
        # 不要 (条件3はブロック理由に含まれない)。
        self.assertFalse(result["codex_review_request_needed"])


if __name__ == "__main__":
    unittest.main()
