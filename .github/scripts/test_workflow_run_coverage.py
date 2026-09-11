"""auto-merge.yml の `workflow_run.workflows` に列挙漏れが無いことの単体テスト。

**なぜこのテストが要るか** (Issue #225 / docs/decisions D107):

auto-merge は「他の workflow が完了するたびに起動し、PR の check-runs を
全部見て、すべて success/skipped なら merge する」という作りになっている。
起動のきっかけは `workflow_run` トリガで、**そこに名前を書いた workflow の
完了でしか発火しない。**

そのため `pull_request` で走る workflow がこのリストから漏れていると、
次の順序で **auto-merge が二度と起きない PR** ができる。

1. `CI` が先に完了 → auto-merge 起動 → 漏れている workflow がまだ実行中
   なので「全部は終わっていない」と判断して何もせず終了
2. その後で漏れている workflow が完了 → **監視対象外なので誰も起動しない**
3. CI 全部緑・`mergeable_state: clean` のまま放置される

実際に PR #223 (`Performance (Windows)` が未列挙) でこれが起きた。
`schedule` は安全網にならない — cron は 1 日 144 回のはずが実測 36 回
(約6%)、実際の間隔は 2〜5 時間である (Issue #219 / D103)。

しかも **列挙は workflow の `name:` との完全一致が必要**で、ここのずれは
YAML としては正しいので何も警告されない。Issue #225 の起票時点では
`Performance (Windows, manual)` と書かれていたが、週次スケジュール実行の
追加 (D106) で `name:` からは既に "manual" が外れており、Issue の案文を
そのまま貼っていたら **直したつもりで直っていない** ところだった。

つまり検知したい壊れ方は 3 つある。どれも「CI は緑のまま」進行する。

- workflow を新しく足して `pull_request` で走らせたが、列挙を忘れた
- 既存 workflow の `name:` を変えたが、列挙側を直し忘れた
- 列挙側に、実在しない名前 (旧名・誤記) が書いてある
"""

from __future__ import annotations

import unittest
from pathlib import Path

import yaml

WORKFLOW_DIR = Path(__file__).resolve().parents[1] / "workflows"
AUTO_MERGE = WORKFLOW_DIR / "auto-merge.yml"


def _on_section(doc: dict) -> dict:
    """workflow の `on:` セクションを返す。

    PyYAML は YAML 1.1 の規則で **裸の `on` を真偽値 `True` として解釈する**
    ため、`doc["on"]` では取り出せない。両方を見る。
    """
    for key in ("on", True):
        if key in doc:
            section = doc[key]
            # `on: [push, pull_request]` のようなリスト形式・文字列形式も
            # ありうるので、辞書に正規化してから返す。
            if isinstance(section, dict):
                return section
            if isinstance(section, list):
                return {name: None for name in section}
            if isinstance(section, str):
                return {section: None}
    return {}


def _load_workflows() -> dict[Path, dict]:
    return {
        path: yaml.safe_load(path.read_text(encoding="utf-8"))
        for path in sorted(WORKFLOW_DIR.glob("*.yml"))
    }


class WorkflowRunCoverageTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.workflows = _load_workflows()
        auto_merge = cls.workflows[AUTO_MERGE]
        cls.auto_merge_name = auto_merge["name"]
        cls.watched = _on_section(auto_merge)["workflow_run"]["workflows"]
        # path -> name。`name:` が無い workflow は GitHub ではファイルパスが
        # 表示名になるが、本リポジトリでは全ファイルが `name:` を持つ前提。
        cls.names = {path: doc["name"] for path, doc in cls.workflows.items()}

    def test_all_workflows_have_a_name(self) -> None:
        """`name:` が無いと GitHub 上の表示名がファイルパスになり、
        `workflow_run.workflows` での指定が破綻する。"""
        for path, doc in self.workflows.items():
            with self.subTest(workflow=path.name):
                self.assertIn("name", doc, f"{path.name} に `name:` が無い")

    def test_every_pull_request_workflow_is_watched(self) -> None:
        """`pull_request` で走る workflow は、auto-merge が漏れなく監視する。

        auto-merge 自身は対象外 — 自分の完了で自分を起こす必要は無く、
        判定でも自分自身の check-run は除外している。
        """
        missing = []
        for path, doc in self.workflows.items():
            name = self.names[path]
            if name == self.auto_merge_name:
                continue
            if "pull_request" not in _on_section(doc):
                # push / schedule / workflow_dispatch だけの workflow は
                # PR の check-runs に現れないので、監視する必要が無い。
                continue
            if name not in self.watched:
                missing.append(f"{path.name} (name: {name!r})")

        self.assertEqual(
            [],
            missing,
            "`pull_request` で走るのに auto-merge.yml の "
            "`workflow_run.workflows` に無い workflow がある。"
            "この workflow が最後に完了する PR で auto-merge が起動しなくなる: "
            + ", ".join(missing),
        )

    def test_watched_names_all_exist(self) -> None:
        """列挙されている名前は、実在する workflow の `name:` と完全一致する。

        旧名・誤記が残っていても GitHub は何も言わない (そういう名前の
        workflow が完了することが無いだけで、エラーにはならない) ので、
        ここで落とす。
        """
        actual = set(self.names.values())
        unknown = [name for name in self.watched if name not in actual]
        self.assertEqual(
            [],
            unknown,
            "auto-merge.yml の `workflow_run.workflows` に、実在しない "
            "workflow 名がある (`name:` の変更に追随し忘れた可能性): "
            + ", ".join(repr(n) for n in unknown),
        )

    def test_auto_merge_does_not_watch_itself(self) -> None:
        """自分自身を監視すると、実行のたびに自分を起こして無駄に回る。"""
        self.assertNotIn(self.auto_merge_name, self.watched)

    def test_watched_list_has_no_duplicates(self) -> None:
        self.assertEqual(
            len(self.watched),
            len(set(self.watched)),
            f"重複がある: {self.watched}",
        )


if __name__ == "__main__":
    unittest.main()
