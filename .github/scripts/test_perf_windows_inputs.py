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

# シナリオ専用のフィクスチャ (汎用の計測対象ページではない)。workflow 側の
# `$fixturePages` と同じ内容でなければならない — 下の
# `test_the_fixture_list_matches_the_one_the_workflow_warns_about` が検査する。
FIXTURE_PAGES = ("busy.html", "download.html", "network_activity.html")


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
        offered = [f for f in FIXTURE_PAGES if f in self.page_options]
        self.assertEqual(
            [],
            offered,
            "シナリオ専用のフィクスチャが `page` の choice に入っている。"
            "汎用の計測対象ページではない: " + ", ".join(offered),
        )

    def test_the_fixture_list_matches_the_one_the_workflow_warns_about(self) -> None:
        """フィクスチャ一覧が workflow 側とこちらでずれていないこと。

        `pages` は自由記述なので `page` の choice による保護が効かない。
        workflow 側は弾く代わりに **Job Summary に警告を出す** (弾くと
        `url` に逃げ道が無い — `url` を指定すると loopback 配信自体が
        止まるため)。その警告の対象一覧がここと食い違うと、**新しい
        フィクスチャを足したときに警告だけが漏れる。**
        """
        declared = '$fixturePages = @(' + ", ".join(
            f'"{name}"' for name in FIXTURE_PAGES
        ) + ')'
        self.assertIn(
            declared,
            self.text,
            "workflow 側の $fixturePages と、このテストの FIXTURE_PAGES が"
            f"食い違っている。期待した宣言: {declared}",
        )

    def test_every_fixture_actually_exists(self) -> None:
        """一覧に実在しないファイル名が残っていると、警告が永久に出ない。"""
        missing = [f for f in FIXTURE_PAGES if not (PAGES_DIR / f).is_file()]
        self.assertEqual([], missing, "実在しないフィクスチャ: " + ", ".join(missing))

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



class BeaconWiringTest(unittest.TestCase):
    """`beacon` 入力が **実計測のステップまで** 届いていることの検査。

    Issue #247 の最初の計測は、ここが繋がっていないまま回った。
    `beacon` 入力は `bench_url` の `url` 出力にだけクエリを付けており、
    実計測ループは `pages` 出力からページ名を取って **素の URL を組み直して
    いた**。`pages` は固定ページ配信時に必ず非空なので、beacon 付きの
    分岐には一度も入らない。

    結果として数えられたのは、`url` 出力を使う唯一の場所 —
    **velox を 8 秒だけ起動する診断ステップ** — の分だけだった。
    それを `background_cpu` 8 トライアルの結果として読み、
    §41 / D125 に書いてしまった (撤回済み)。

    **run を 1 回使い切っても気づけない種類の失敗である。** 数字は出るし、
    ステップも緑になる。出てきた数字が別のものの数字であることは、
    workflow を読まないと分からない。
    """

    @classmethod
    def setUpClass(cls) -> None:
        cls.doc = yaml.safe_load(WORKFLOW.read_text(encoding="utf-8"))
        cls.steps = cls.doc["jobs"]["perf-windows"]["steps"]

    def _step(self, name_fragment: str) -> dict:
        for step in self.steps:
            if name_fragment in (step.get("name") or ""):
                return step
        raise AssertionError(f"ステップが見つからない: {name_fragment}")

    def test_the_bench_step_receives_the_beacon_input(self) -> None:
        """実計測ステップが `beacon` を見られなければ、URL に付けようがない。"""
        step = self._step("Run velox-bench")
        self.assertIn("INPUT_BEACON", step.get("env") or {})

    def test_the_bench_step_builds_the_url_with_the_beacon_query(self) -> None:
        """ページ名から URL を組み直す分岐にも beacon クエリが要る。"""
        run = self._step("Run velox-bench")["run"]
        self.assertIn("$beaconQuery", run)
        self.assertIn('"http://127.0.0.1:8731/$pg$beaconQuery"', run)

    def test_the_diagnose_step_does_not_send_beacons(self) -> None:
        """診断は 8 秒の単独起動で、計測の自動操作とは無関係。

        beacon 付きの URL を渡すと、その 8 秒分が計測と同じカウンタに
        混ざる。最初の計測で数えていたのは、まさにこの混入分だった。
        """
        run = self._step("Diagnose VeloX launch")["run"]
        self.assertIn("steps.bench_url.outputs.url_plain", run)
        self.assertNotIn("steps.bench_url.outputs.url }}", run)

    def test_url_plain_is_emitted_on_both_branches(self) -> None:
        """外部 URL を渡した場合も診断ステップは URL を必要とする。"""
        run = self._step("Determine benchmark URL")["run"]
        self.assertEqual(2, run.count("url_plain="))

    def test_every_step_reading_the_beacon_input_does_so_via_env(self) -> None:
        """入力をシェルへ直接展開しない (script injection 対策)。"""
        for step in self.steps:
            with self.subTest(step=step.get("name")):
                self.assertNotIn("inputs.beacon }}", step.get("run") or "")


if __name__ == "__main__":
    unittest.main()
