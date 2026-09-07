#!/usr/bin/env python3
"""check_review_gate.py のユニットテスト。

実行方法:
    python3 -m unittest .github/scripts/test_check_review_gate.py -v
または (このディレクトリから):
    python3 -m unittest test_check_review_gate -v

Issue #188 の完了条件・PR #185 / #189 / #191 の実例をそのまま網羅する。

テスト方針: 「未解決スレッド」「CHANGES_REQUESTED」「猶予期間」の3条件を
検証するテストクラスは `codex_bypass=True` を既定にして Codex 必須判定を
分離する (この3条件は Codex 要件と無関係のため)。Codex 必須判定
(条件3、`automerge-without-codex` ラベルでの免除を含む) は専用の
`CodexReviewRequiredTest` で個別に検証する。`CodexLoginExactMatchTest` /
`PushObservedAtSecurityRegressionTest` は 2026-09-07 の Codex レビュー
指摘 (PR #192、docs/decisions.md D91) を受けて追加した回帰テスト:
Codex ログイン判定の前方一致 (別名アカウントによるなりすまし) と、猶予期間
/シグナルcの基準時刻に committer date を使う設計 (attacker が操作可能な
タイムスタンプ) の2件の脆弱性を防ぐ。
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


def _evaluate(
    threads=_EMPTY_THREADS,
    reviews=_EMPTY_REVIEWS,
    push_observed_at=_HEAD_PUSH_OBSERVED_1H_AGO,
    now=_NOW,
    grace=_GRACE,
    reactions=_EMPTY_REACTIONS,
    head_sha=_HEAD_SHA,
    codex_bypass=True,
):
    """条件1/2/4 (Codex 非依存の条件) を検証するための既定ヘルパー。

    `codex_bypass=True` を既定にしているため、Codex レビューの有無に
    関わらず条件3ではブロックされない。
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
        result = _evaluate(reviews=_EMPTY_REVIEWS, codex_bypass=False)
        self.assertTrue(result["blocked"])
        self.assertIn("Codex のレビュー待ち", result["reasons"][0])
        self.assertIn(_HEAD_SHA[:7], result["reasons"][0])

    def test_codex_review_matches_head_sha_via_commit_oid_not_blocked(self) -> None:
        reviews = self._reviews_with_codex(commit={"oid": _HEAD_SHA})
        result = _evaluate(reviews=reviews, codex_bypass=False)
        self.assertFalse(result["blocked"])

    def test_codex_review_on_stale_commit_blocked(self) -> None:
        # PR #185 のケース: Codex がレビューしたのは古いコミットで、
        # その後 push された新しい head SHA には未対応。
        stale_sha = "0" * 40
        reviews = self._reviews_with_codex(commit={"oid": stale_sha})
        result = _evaluate(reviews=reviews, codex_bypass=False)
        self.assertTrue(result["blocked"])
        self.assertIn("Codex のレビュー待ち", result["reasons"][0])

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
        self.assertIn("Codex のレビュー待ち", result["reasons"][0])

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
        )
        self.assertTrue(result["blocked"])
        # Codex 待ち (古い👍は不成立) + 猶予期間 (6分しか経っていない) の
        # 2件がともに正しくブロック理由になっていること。
        self.assertEqual(len(result["reasons"]), 2)
        self.assertTrue(
            any("Codex のレビュー待ち" in r for r in result["reasons"])
        )
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
        )
        self.assertTrue(result["blocked"])
        self.assertEqual(len(result["reasons"]), 4)


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
        )
        self.assertTrue(result["blocked"])
        self.assertEqual(len(result["reasons"]), 2)  # 未解決スレッド + 猶予期間
        self.assertFalse(
            any("Codex のレビュー待ち" in r for r in result["reasons"])
        )


if __name__ == "__main__":
    unittest.main()
