"""perf-windows.yml の入力とサマリ用正規表現が食い違っていないことの単体テスト。

**なぜこのテストが要るか**

`perf-windows.yml` のサマリ生成は PowerShell で書かれており、**Windows
ランナー上でしか動かない。** 壊れても CI (Linux) は緑のままで、気づくのは
「高コストな Windows run を 1 回使い切ったあと、Job Summary に表が出て
いないことに気づいたとき」である。実際に起きた壊れ方が 2 つ、workflow 本体の
コメントに記録されている。

- run 34247895418: 並べ替え用の正規表現が `tabs_hold_` を勘定に入れて
  おらず、`tabs_hold_20` が数値順から落ちて `_1, _10, _20, _5, _50` という
  文字列順になった。
- `tabs_hold_resume_` (Issue #176) でも同じ漏れをやりかけた。`hold_` の
  直後に数字を要求する形だと `hold_resume_20` で外れる。

どちらも **run を 1 回捨てて初めて分かる**種類の失敗である。正規表現と
シナリオ一覧はどちらもこの YAML の中にあるので、突き合わせは Linux 上で
できる。

ページ一覧についても同じ: Issue #231 で `pages` 入力を足した際、指定された
ファイルが `scripts/bench/pages/` に無ければジョブを落とすようにした。
つまり `page` の choice に実在しないファイル名が残っていると、**それを
選んだ run が起動直後に失敗する。**
"""

from __future__ import annotations

import re
import unittest
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = ROOT / ".github" / "workflows" / "perf-windows.yml"
PAGES_DIR = ROOT / "scripts" / "bench" / "pages"

# workflow 本体に**この字面どおり**書かれている前提の正規表現。
#
# ここを定数として持つのは重複ではなく意図である: workflow 側だけを直して
# このテストを直し忘れれば `test_*_is_written_verbatim_in_the_workflow` が
# 落ち、「正規表現を変えた」ことが必ず目に入る。逆にテストだけ直しても
# 同じく落ちる。
SORT_REGEX = r"^tabs_(?:hold_(?:resume_)?)?(\d+)$"
SCALING_REGEX = r"^tabs_(?:hold_)?\d+$"


def _on_section(doc: dict) -> dict:
    """workflow の `on:` セクション。PyYAML は裸の `on` を `True` と読む。"""
    for key in ("on", True):
        if key in doc:
            return doc[key]
    raise AssertionError("`on:` セクションが無い")


class PerfWindowsInputsTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.text = WORKFLOW.read_text(encoding="utf-8")
        doc = yaml.safe_load(cls.text)
        inputs = _on_section(doc)["workflow_dispatch"]["inputs"]
        cls.scenario_options = list(inputs["scenario"]["options"])
        cls.page_options = list(inputs["page"]["options"])
        cls.inputs = inputs

    # --- ページ一覧 (Issue #231) -------------------------------------

    def test_every_page_option_exists_on_disk(self) -> None:
        """choice に無いファイルを選ぶと、ジョブが起動直後に落ちる。"""
        missing = [p for p in self.page_options if not (PAGES_DIR / p).is_file()]
        self.assertEqual(
            [],
            missing,
            "`page` の choice に scripts/bench/pages/ に無いファイルがある。"
            "これを選んだ run は計測に入る前に失敗する: " + ", ".join(missing),
        )

    # 逆向き (ディレクトリにあるページがすべて choice に出ていること) は
    # **検査しない。** `scripts/bench/pages/` にはシナリオ専用のフィクスチャ
    # が同居しており、汎用の計測対象ページではないからである:
    #
    #   - `busy.html`             — `background_cpu` 専用 (`?idle=1` を付けて
    #                               読み込む。src/browser/automation.rs 参照)
    #   - `download.html`         — ダウンロード系の検証用
    #   - `network_activity.html` — ネットワーク活動の検出用
    #                               (src/browser/network_activity.rs 参照)
    #
    # これらを `page` の choice に並べると「どのページでも同じように測れる」
    # という誤った示唆を与える。choice は**汎用の計測対象ページだけ**の
    # 一覧である。

    def test_the_scenario_specific_fixtures_are_not_offered_as_pages(self) -> None:
        """シナリオ専用のフィクスチャを汎用ページとして選ばせない。

        上のコメントの理由を、後から `page` の choice を「ディレクトリと
        揃える」方向に直されないよう固定しておく。
        """
        fixtures = ("busy.html", "download.html", "network_activity.html")
        offered = [f for f in fixtures if f in self.page_options]
        self.assertEqual(
            [],
            offered,
            "シナリオ専用のフィクスチャが `page` の choice に入っている。"
            "汎用の計測対象ページではない: " + ", ".join(offered),
        )

    def test_pages_input_exists_and_is_free_text(self) -> None:
        """`pages` は choice ではなくカンマ区切りの文字列である。

        choice にしてしまうと複数指定できず、Issue #231 が解こうとした
        「1 run に複数ページを収める」ができない。
        """
        self.assertIn("pages", self.inputs)
        self.assertEqual("string", self.inputs["pages"]["type"])

    # --- 並べ替え用の正規表現 ----------------------------------------

    def test_sort_regex_is_written_verbatim_in_the_workflow(self) -> None:
        self.assertIn(
            SORT_REGEX,
            self.text,
            "並べ替え用の正規表現が workflow 側と食い違っている。"
            "どちらかだけを直した可能性がある",
        )

    def test_scaling_regex_is_written_verbatim_in_the_workflow(self) -> None:
        self.assertIn(
            SCALING_REGEX,
            self.text,
            "スケーリング表の絞り込み用正規表現が workflow 側と"
            "食い違っている",
        )

    def test_sort_regex_covers_every_parameterized_tab_scenario(self) -> None:
        """`tabs*` のシナリオはすべて数値順に並べられなければならない。

        1 つでも外れると、その 1 行だけが表の末尾 ([int]::MaxValue) に
        飛び、タブ数スケーリングが読めなくなる。
        """
        sort_re = re.compile(SORT_REGEX)
        tab_scenarios = [s for s in self.scenario_options if s.startswith("tabs_")]
        self.assertGreater(len(tab_scenarios), 0, "tabs_ 系のシナリオが 1 つも無い")
        uncovered = [s for s in tab_scenarios if not sort_re.match(s)]
        self.assertEqual(
            [],
            uncovered,
            "並べ替え用の正規表現から漏れている `tabs*` シナリオがある。"
            "これらは数値順ではなく末尾に落ちる: " + ", ".join(uncovered),
        )

    def test_sort_regex_extracts_the_tab_count(self) -> None:
        """キャプチャ 1 はタブ数でなければならない (`[int]$Matches[1]`)。"""
        sort_re = re.compile(SORT_REGEX)
        for scenario in self.scenario_options:
            if not scenario.startswith("tabs_"):
                continue
            with self.subTest(scenario=scenario):
                match = sort_re.match(scenario)
                assert match is not None  # 上のテストが保証する
                self.assertEqual(scenario.rsplit("_", 1)[1], match.group(1))

    # --- スケーリング表の絞り込み ------------------------------------

    def test_scaling_regex_deliberately_excludes_the_resume_family(self) -> None:
        """`tabs_hold_resume_N` をスケーリング表に入れてはならない。

        **これは漏れではなく意図である** (Issue #176 / D97)。
        `tabs_hold_resume_N` の `rss_total_bytes` は**復帰ラウンドの後**の
        窓で測った値で、`tabs_hold_N` の定常値とは別物である (実測で
        527.9 → 666.4 MiB)。同じ表に並べると、`tabs_N` と `tabs_hold_N` を
        混ぜてはならないのと同じ誤読を招く。

        並べ替え用の正規表現とここだけ食い違うので、「揃っていない=バグ」
        と思って直されないよう、意図をテストとして固定しておく。
        """
        scaling_re = re.compile(SCALING_REGEX)
        resume = [s for s in self.scenario_options if s.startswith("tabs_hold_resume_")]
        self.assertGreater(len(resume), 0, "tabs_hold_resume_N が 1 つも無い")
        included = [s for s in resume if scaling_re.match(s)]
        self.assertEqual(
            [],
            included,
            "`tabs_hold_resume_N` がスケーリング表の対象に入っている。"
            "定常値ではなく復帰後の値なので、`tabs_hold_N` と同じ表に"
            "並べてはならない (D97): " + ", ".join(included),
        )

    def test_scaling_regex_covers_the_steady_state_families(self) -> None:
        scaling_re = re.compile(SCALING_REGEX)
        steady = [
            s
            for s in self.scenario_options
            if re.match(r"^tabs_(?:hold_)?\d+$", s)
        ]
        self.assertGreater(len(steady), 0)
        uncovered = [s for s in steady if not scaling_re.match(s)]
        self.assertEqual([], uncovered)


if __name__ == "__main__":
    unittest.main()
