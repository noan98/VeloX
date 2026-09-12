"""`docs/decisions/` のカテゴリ索引が `archive.md` と食い違わないことの単体テスト。

**なぜこのテストが要るか** (Issue #227):

`docs/decisions/README.md` は「**現在の設計を知りたい → カテゴリ別
インデックスから該当する Decision を開く**」を入口として案内している。
ところが起票時点でその入口から辿れたのは 108 件中 53 件だけで、D71 以降の
38 件は**どのカテゴリファイルにも番号が現れていなかった。**

さらに悪いことに、**存在していたリンク 22 本のうち 14 本はアンカーが実在
しなかった。** `archive.md` の見出しが英語から日本語に書き換えられた際
(PR #220 の分割より前)、リンク側が旧見出しのまま残っていたためである
(例: `#d51-windows-release-github-actions-release-windowsyml` — 実際の
見出しは「D51: Windows リリースビルドは GitHub Actions (`release-windows.yml`)
で行う」)。**リンク切れは Markdown としては正しいので、何も警告されない。**

つまり検知したい壊れ方は 3 つある。どれも「CI は緑のまま」進行する。

- Decision を追記したが、カテゴリ索引に足し忘れた
- `archive.md` の見出しを変えたが、索引側のアンカーを直し忘れた
- 索引側に、実在しない Decision 番号・アンカーが書いてある

どれも `archive.md` とカテゴリファイルを突き合わせれば機械的に分かる。
"""

from __future__ import annotations

import re
import unittest

from decision_index import (
    CATEGORY_FILES,
    DECISIONS_DIR,
    category_of,
    decisions,
    slug,
)

LINK = re.compile(r"\[D(\d+)\]\(\./archive\.md#([^)]+)\)")


class DecisionIndexTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.items = decisions()
        cls.texts = {
            cat: (DECISIONS_DIR / name).read_text(encoding="utf-8")
            for cat, name in CATEGORY_FILES.items()
        }
        cls.links: dict[int, list[tuple[str, str]]] = {}
        for cat, text in cls.texts.items():
            for match in LINK.finditer(text):
                cls.links.setdefault(int(match.group(1)), []).append(
                    (cat, match.group(2))
                )

    def test_the_archive_has_decisions(self) -> None:
        """探索条件が壊れて 0 件になったまま緑にならないようにする。"""
        self.assertGreater(len(self.items), 100)

    def test_every_decision_is_indexed(self) -> None:
        """`archive.md` の全 Decision がどこかのカテゴリから引ける。

        これが README の案内 (「インデックスから開く」) の前提である。
        """
        missing = sorted(n for n in self.items if n not in self.links)
        self.assertEqual(
            [],
            missing,
            "カテゴリ索引から辿れない Decision がある。"
            "`.github/scripts/decision_index.py` の割り当てに足し、"
            "該当カテゴリファイルの「詳細」に追記すること: "
            + ", ".join(f"D{n}" for n in missing),
        )

    def test_every_decision_is_indexed_exactly_once(self) -> None:
        """主担当は 1 つ (README の「重要なルール」3)。

        複数カテゴリに同じ Decision を載せると、どちらが最新か分からなく
        なり、片方だけ直される。
        """
        duplicated = {
            n: [cat for cat, _ in where]
            for n, where in self.links.items()
            if len(where) > 1
        }
        self.assertEqual(
            {},
            duplicated,
            f"複数のカテゴリに載っている Decision がある: {duplicated}",
        )

    def test_every_indexed_decision_exists(self) -> None:
        """索引側に、`archive.md` に無い番号が書いてある状態を防ぐ。"""
        unknown = sorted(n for n in self.links if n not in self.items)
        self.assertEqual(
            [],
            unknown,
            "`archive.md` に存在しない Decision が索引にある: "
            + ", ".join(f"D{n}" for n in unknown),
        )

    def test_every_anchor_resolves(self) -> None:
        """**リンク切れの検知。** Issue #227 の起票時点で 22 本中 14 本が該当。

        アンカーは見出しから機械的に決まるので、見出しを変えれば必ずここで
        落ちる。
        """
        broken = []
        for number, where in sorted(self.links.items()):
            if number not in self.items:
                continue  # 上のテストが報告する
            expected = self.items[number][1]
            for cat, anchor in where:
                if anchor != expected:
                    broken.append(
                        f"{CATEGORY_FILES[cat]} の D{number}: "
                        f"#{anchor} → 正しくは #{expected}"
                    )
        self.assertEqual(
            [],
            broken,
            "アンカーが `archive.md` の見出しと一致しない (リンク切れ):\n  "
            + "\n  ".join(broken),
        )

    def test_each_decision_is_indexed_in_its_assigned_category(self) -> None:
        """割り当て (`decision_index.py`) と実ファイルが一致している。"""
        misplaced = []
        for number, where in sorted(self.links.items()):
            assigned = category_of(number)
            for cat, _ in where:
                if assigned is not None and cat != assigned:
                    misplaced.append(
                        f"D{number}: {CATEGORY_FILES[cat]} にあるが "
                        f"割り当ては {CATEGORY_FILES[assigned]}"
                    )
        self.assertEqual([], misplaced, "\n  ".join(misplaced))

    def test_every_decision_has_an_assignment(self) -> None:
        unassigned = sorted(n for n in self.items if category_of(n) is None)
        self.assertEqual(
            [],
            unassigned,
            "`decision_index.py` の `_ASSIGNMENTS` に無い Decision がある: "
            + ", ".join(f"D{n}" for n in unassigned),
        )

    def test_the_readme_table_lists_every_category(self) -> None:
        readme = (DECISIONS_DIR / "README.md").read_text(encoding="utf-8")
        for name in CATEGORY_FILES.values():
            with self.subTest(category=name):
                self.assertIn(f"](./{name})", readme)

    # --- アンカー生成規則そのもの --------------------------------------

    def test_slug_removes_punctuation_rather_than_hyphenating_it(self) -> None:
        """**この規則を推測で書き換えないための番人。**

        素朴に「記号をハイフンに置換」すると、`セキュリティ・入力値` が
        `セキュリティ-入力値` になり、GitHub の実際のアンカー
        (`セキュリティ入力値`) と食い違う。Issue #227 の作業中に本家
        `github-slugger` と全 115 見出しで突き合わせて確認した値である。
        """
        self.assertEqual("セキュリティ入力値堅牢性", slug("セキュリティ・入力値堅牢性"))
        self.assertEqual("維持ipc-に", slug("維持、IPC に"))
        self.assertEqual(
            "d16-performance-metrics--proc-directly-no-new-dependency",
            slug("D16: Performance metrics — `/proc` directly, no new dependency"),
        )
        # `_` と `-` は残る。矢印の両脇の空白はハイフン 2 つになる。
        self.assertEqual(
            "window_created--toolbar_ready", slug("window_created → toolbar_ready")
        )
        self.assertEqual("タブプロセス", slug("タブ/プロセス"))


if __name__ == "__main__":
    unittest.main()
