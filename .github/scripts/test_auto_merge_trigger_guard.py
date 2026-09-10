"""auto-merge.yml のジョブ `if` (トリガ発火条件) の単体テスト。

**なぜこのテストが要るか**: 本リポジトリは 2026-09-09 に public へ変更された
(docs/decisions.md D102)。public リポジトリでは GitHub の任意のユーザが
Issue / PR にコメントでき、レビューも提出できる。auto-merge ジョブは
`contents: write` と `AUTO_MERGE_TOKEN` (repo 権限の PAT) を持つため、
「誰の `issue_comment` / `pull_request_review` で起動するか」は
セキュリティ上の境界そのものである。

ところが `if` の式は **main にマージしないと一度も評価されない** — 手元でも
PR の CI でも実行されない。過去に何度も「Linux 上で検知できたはずの失敗を
Windows CI まで持ち込んだ」(docs/performance-targets.md §29.9) のと同じ構図
なので、式を YAML から読み出して評価するテストをここに置く。

式の評価には GitHub Actions 式言語のごく一部だけを実装したミニ評価器を使う
(`&&` / `||` / `!` / `!=` / `contains()` / `fromJSON()` / コンテキスト参照)。
本物の評価器ではないため、**式にここで未対応の構文を足したらテストが
`SyntaxError` などで落ちる** — 黙って通ってしまうことはない。
"""

from __future__ import annotations

import json
import re
import unittest
from pathlib import Path

import yaml

WORKFLOW = Path(__file__).resolve().parents[1] / "workflows" / "auto-merge.yml"


class _Value:
    """GitHub Actions の式で扱う値 (null を含む) のラッパ。

    Actions の比較は型が違うと数値へキャストする規則があり、`null` と `''`
    はどちらも 0 になるので **等しい**。`github.event.issue.pull_request.url`
    が存在しない (= Issue へのコメント) ケースの判定がこの規則に依存して
    いるため、素の Python の比較では再現できない。
    """

    def __init__(self, raw):
        self.raw = raw

    @staticmethod
    def _num(v):
        if v is None:
            return 0.0
        if isinstance(v, bool):
            return 1.0 if v else 0.0
        if isinstance(v, (int, float)):
            return float(v)
        if isinstance(v, str):
            if v == "":
                return 0.0
            try:
                return float(v)
            except ValueError:
                return float("nan")
        return float("nan")

    def __eq__(self, other):
        other_raw = other.raw if isinstance(other, _Value) else other
        if type(self.raw) is type(other_raw) or (
            isinstance(self.raw, str) and isinstance(other_raw, str)
        ):
            return self.raw == other_raw
        return self._num(self.raw) == self._num(other_raw)

    def __ne__(self, other):
        return not self.__eq__(other)

    def __bool__(self):
        return bool(self.raw)

    def __hash__(self):
        return hash(self.raw)


def _lookup(context: dict, path: str):
    node = context
    for part in path.split("."):
        if not isinstance(node, dict) or part not in node:
            return None
        node = node[part]
    return node


_STRING_LITERAL = re.compile(r"'(?:[^']|'')*'")


def _translate_code(fragment: str) -> str:
    """文字列リテラルの**外側**だけを Python の式に置き換える。"""
    # `!=` を先に退避してから `!` を `not` に変える (順番を逆にすると壊れる)
    src = fragment.replace("!=", "\x00NE\x00")
    src = src.replace("&&", " and ").replace("||", " or ")
    src = src.replace("!", " not ")
    src = src.replace("\x00NE\x00", "!=")
    # コンテキスト参照 (github.event_name / github.event.comment.body など)
    return re.sub(r"\bgithub(?:\.[A-Za-z_][A-Za-z0-9_]*)+", lambda m: f"g({m.group(0)!r})", src)


def _translate(expression: str) -> str:
    """Actions の式を、同じ意味の Python 式へ機械的に置き換える。

    文字列リテラルの中身には一切手を触れない。`'<!-- auto-merge:'` の `!` を
    `not` に置換してしまう事故 (実際にこのテストの初版で踏んだ) を防ぐため、
    リテラルとそれ以外を分けて処理する。Actions のリテラル内で `''` は
    シングルクォート 1 文字を表す。
    """
    out = []
    position = 0
    for match in _STRING_LITERAL.finditer(expression):
        out.append(_translate_code(expression[position : match.start()]))
        out.append(repr(match.group(0)[1:-1].replace("''", "'")))
        position = match.end()
    out.append(_translate_code(expression[position:]))
    return "".join(out)


def evaluate(expression: str, context: dict) -> bool:
    def g(path: str) -> _Value:
        return _Value(_lookup(context, path))

    def contains(search, item) -> bool:
        search_raw = search.raw if isinstance(search, _Value) else search
        item_raw = item.raw if isinstance(item, _Value) else item
        if isinstance(search_raw, (list, tuple)):
            return any(_Value(element) == item_raw for element in search_raw)
        if search_raw is None:
            return False
        return str(item_raw) in str(search_raw)

    def fromJSON(text):  # noqa: N802 — Actions 側の綴りに合わせる
        return json.loads(text.raw if isinstance(text, _Value) else text)

    return bool(
        eval(  # noqa: S307 — 入力はリポジトリ内の workflow 定義のみ
            _translate(expression),
            {"__builtins__": {}},
            {"g": g, "contains": contains, "fromJSON": fromJSON},
        )
    )


def _comment_event(*, login, association, body="LGTM", on_pull_request=True):
    issue = {"pull_request": {"url": "https://api.github.com/repos/o/r/pulls/1"}}
    if not on_pull_request:
        issue = {}
    return {
        "github": {
            "event_name": "issue_comment",
            "event": {
                "issue": issue,
                "comment": {
                    "body": body,
                    "author_association": association,
                    "user": {"login": login},
                },
            },
        }
    }


def _review_event(*, login, association):
    return {
        "github": {
            "event_name": "pull_request_review",
            "event": {"review": {"author_association": association, "user": {"login": login}}},
        }
    }


def _plain_event(name):
    return {"github": {"event_name": name, "event": {}}}


class AutoMergeTriggerGuardTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        workflow = yaml.safe_load(WORKFLOW.read_text(encoding="utf-8"))
        cls.condition = workflow["jobs"]["auto-merge"]["if"]

    def assert_runs(self, context, expected, message):
        self.assertEqual(evaluate(self.condition, context), expected, message)

    # --- コメント / レビュー以外のトリガは従来どおり ------------------
    def test_workflow_run_still_runs(self):
        self.assert_runs(_plain_event("workflow_run"), True, "CI 完了での起動は維持される")

    def test_schedule_and_dispatch_still_run(self):
        for name in ("schedule", "workflow_dispatch"):
            with self.subTest(event=name):
                self.assert_runs(_plain_event(name), True, f"{name} での起動は維持される")

    def test_pull_request_never_runs(self):
        self.assert_runs(
            _plain_event("pull_request"), False, "マージするジョブは pull_request で走らない"
        )

    # --- public リポジトリ対策: 投稿者の絞り込み ----------------------
    def test_owner_comment_on_pull_request_runs(self):
        self.assert_runs(
            _comment_event(login="noan98", association="OWNER"), True, "オーナーのコメントは通す"
        )

    def test_collaborator_and_member_comments_run(self):
        for association in ("MEMBER", "COLLABORATOR"):
            with self.subTest(association=association):
                self.assert_runs(
                    _comment_event(login="someone", association=association),
                    True,
                    f"{association} のコメントは通す",
                )

    def test_outside_user_comment_does_not_run(self):
        for association in ("NONE", "CONTRIBUTOR", "FIRST_TIME_CONTRIBUTOR"):
            with self.subTest(association=association):
                self.assert_runs(
                    _comment_event(login="stranger", association=association),
                    False,
                    "public リポジトリの外部ユーザのコメントでは起動しない",
                )

    def test_review_bots_run_even_without_association(self):
        # ボットの author_association は NONE / CONTRIBUTOR になりうるので、
        # ログイン名の完全一致で別途通す必要がある。
        for login in ("chatgpt-codex-connector[bot]", "claude[bot]"):
            with self.subTest(login=login):
                self.assert_runs(
                    _comment_event(login=login, association="NONE"),
                    True,
                    f"{login} の返答では起動する (このトリガの目的)",
                )

    def test_lookalike_user_cannot_impersonate_bot(self):
        # `claude[bot]` は `[` `]` を含むため GitHub のユーザ名としては作れない。
        # 近い名前を取られても一致しないことを固定する。
        for login in ("claude", "claude-bot", "claude[bot]x", "Claude[bot]"):
            with self.subTest(login=login):
                self.assert_runs(
                    _comment_event(login=login, association="NONE"),
                    False,
                    "ボットに似た名前の一般ユーザでは起動しない",
                )

    def test_own_request_comment_does_not_run(self):
        self.assert_runs(
            _comment_event(
                login="noan98",
                association="OWNER",
                body="<!-- auto-merge:codex-review:abc123 -->\n@codex review",
            ),
            False,
            "auto-merge 自身の依頼コメントでは起動しない (空振り防止)",
        )

    def test_comment_on_plain_issue_does_not_run(self):
        self.assert_runs(
            _comment_event(login="noan98", association="OWNER", on_pull_request=False),
            False,
            "素の Issue へのコメントでは起動しない (必ず空振りするため)",
        )

    # --- pull_request_review も同じ条件で絞る -------------------------
    def test_owner_review_runs(self):
        self.assert_runs(
            _review_event(login="noan98", association="OWNER"), True, "オーナーのレビューは通す"
        )

    def test_outside_user_review_does_not_run(self):
        self.assert_runs(
            _review_event(login="stranger", association="NONE"),
            False,
            "public リポジトリでは誰でもレビューを出せるので外部ユーザは弾く",
        )

    def test_bot_review_runs(self):
        self.assert_runs(
            _review_event(login="claude[bot]", association="NONE"),
            True,
            "レビューボットのレビューでは起動する",
        )


class TriggerDeclarationTest(unittest.TestCase):
    """`on:` が購読するイベントの宣言を固定する (Issue #219 / D103 決定4)。

    ジョブの `if` をいくら正しく書いても、**そもそもイベントを購読して
    いなければ workflow は起動しない。** ここはその 1 段外側を守る。
    """

    @classmethod
    def setUpClass(cls):
        workflow = yaml.safe_load(WORKFLOW.read_text(encoding="utf-8"))
        # PyYAML は YAML 1.1 として `on:` を真偽値 True に解釈する。
        cls.triggers = workflow[True] if True in workflow else workflow["on"]

    def test_issue_comment_includes_edited(self) -> None:
        """**`edited` が要る。** `claude-code-action` はコメントを編集して完了する。

        進捗コメントを 1 つ立てて編集し続ける実装なので、**レビュー完了は
        `created` ではなく `edited` として届く。** `created` だけだと
        「Claude Code is working…」の時点でしか起動せず、その時点では
        ゲートが「応答待ち」と判定するのは当然で、本当のレビューが載っても
        二度と起動しない (PR #220 で 38 分以上の停止を実測)。
        """
        types = self.triggers["issue_comment"]["types"]
        self.assertIn("created", types)
        self.assertIn(
            "edited",
            types,
            "issue_comment に edited が無いと Claude のレビュー完了を拾えない",
        )

    def test_pull_request_review_is_subscribed(self) -> None:
        self.assertIn("submitted", self.triggers["pull_request_review"]["types"])

    def test_comment_driven_triggers_still_present(self) -> None:
        """D100 決定2 (A) が足したトリガが消えていないこと。"""
        for name in ("issue_comment", "pull_request_review", "workflow_dispatch"):
            with self.subTest(trigger=name):
                self.assertIn(name, self.triggers)


class MiniEvaluatorTest(unittest.TestCase):
    """ミニ評価器そのものの健全性 (これが壊れていると上のテストが無意味になる)。"""

    def test_null_equals_empty_string(self):
        ctx = {"github": {"event": {}}}
        self.assertFalse(evaluate("github.event.issue.pull_request.url != ''", ctx))

    def test_present_string_is_not_empty(self):
        ctx = {"github": {"event": {"issue": {"pull_request": {"url": "https://x"}}}}}
        self.assertTrue(evaluate("github.event.issue.pull_request.url != ''", ctx))

    def test_contains_on_array_is_exact_match(self):
        ctx = {"github": {"event_name": "claude"}}
        self.assertFalse(
            evaluate("contains(fromJSON('[\"claude[bot]\"]'), github.event_name)", ctx),
            "配列に対する contains は部分一致ではなく完全一致でなければならない",
        )

    def test_contains_on_string_is_substring(self):
        ctx = {"github": {"event": {"comment": {"body": "xx <!-- auto-merge:y --> zz"}}}}
        self.assertTrue(evaluate("contains(github.event.comment.body, '<!-- auto-merge:')", ctx))


if __name__ == "__main__":
    unittest.main()
