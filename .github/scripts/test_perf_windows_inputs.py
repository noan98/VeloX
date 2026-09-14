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

    def test_the_ab_loop_separates_the_arms(self) -> None:
        """腕ごとに区切らないと low と normal が同じバケットに積み上がる。

        §42.5 / D127 決定5。再計測できたのに `low`/`normal` を分離
        できなかったのは、これが無かったためである。
        """
        run = self._step("Run velox-bench")["run"]
        self.assertIn("beacon-reset", run)
        # A 側・B 側の両方で腕の集計を読み出していること。
        self.assertEqual(2, run.count("Write-BeaconArm "))

    def test_the_reset_helper_is_inert_when_beacon_is_off(self) -> None:
        """既定の計測では serve.py が動いていないので、叩けば必ず失敗する。"""
        run = self._step("Run velox-bench")["run"]
        self.assertIn('if ($env:INPUT_BEACON -ne "true") { return $null }', run)

    def test_the_reset_helper_never_fails_the_job(self) -> None:
        """診断のために 16 分のベンチを捨てない (§42.7)。"""
        run = self._step("Run velox-bench")["run"]
        self.assertIn("::warning::beacon の区切りに失敗しました", run)

    def test_every_step_reading_the_beacon_input_does_so_via_env(self) -> None:
        """入力をシェルへ直接展開しない (script injection 対策)。"""
        for step in self.steps:
            with self.subTest(step=step.get("name")):
                self.assertNotIn("inputs.beacon }}", step.get("run") or "")


class IngestHistoryWiringTest(unittest.TestCase):
    """取り込みジョブ (Issue #211 項目4 後半 / D106 Revisit (1)) の配線。

    このジョブは **週 1 回の schedule 実行でしか走らない。** 壊れていても
    PR の CI は緑のままなので、Linux 上で突き合わせられることは全部ここで
    突き合わせる。とくに書き込み権限を持つジョブなので、「起動条件」と
    「権限の範囲」は字面で固定する。
    """

    @classmethod
    def setUpClass(cls) -> None:
        cls.text = WORKFLOW.read_text(encoding="utf-8")
        cls.doc = yaml.safe_load(cls.text)
        cls.bench = cls.doc["jobs"]["perf-windows"]
        cls.ingest = cls.doc["jobs"]["ingest-history"]

    def _runs(self, *, code_only: bool = False) -> str:
        """取り込みジョブの `run:` を連結する。

        `code_only=True` なら行頭 `#` のコメント行を落とす — 「この書き方を
        してはならない」をコメントで説明している箇所を、実際のコードと
        取り違えないため。
        """
        scripts = [s.get("run") or "" for s in self.ingest["steps"]]
        if not code_only:
            return "\n".join(scripts)
        lines = [ln for script in scripts for ln in script.splitlines() if not ln.strip().startswith("#")]
        return "\n".join(lines)

    def _uses(self, prefix: str) -> list[str]:
        found = []
        for job in self.doc["jobs"].values():
            for step in job["steps"]:
                uses = step.get("uses") or ""
                if uses.startswith(prefix):
                    found.append(uses)
        return found

    def test_the_artifact_name_output_matches_the_upload_step_verbatim(self) -> None:
        """片方だけ直したら落ちること。

        **ここが食い違うと、取り込みジョブは存在しない artifact を取りに
        いって失敗する** — しかも失敗するのは週次実行だけである。
        """
        upload = [s for s in self.bench["steps"] if (s.get("uses") or "").startswith("actions/upload-artifact")]
        self.assertEqual(len(upload), 1)
        self.assertEqual(self.bench["outputs"]["artifact_name"], upload[0]["with"]["name"])

    def test_upload_and_download_artifact_share_a_major_version(self) -> None:
        """artifact の upload/download は major を跨いだ組み合わせが動かない。"""
        uploads = self._uses("actions/upload-artifact@")
        downloads = self._uses("actions/download-artifact@")
        self.assertTrue(uploads and downloads)
        majors = {u.rsplit("@", 1)[1] for u in uploads} | {d.rsplit("@", 1)[1] for d in downloads}
        self.assertEqual(len(majors), 1, f"バージョンが揃っていない: {majors}")

    def test_the_ingest_job_never_runs_for_a_pull_request(self) -> None:
        """fork からの PR で書き込み権限つきジョブを起動しない。"""
        condition = self.ingest["if"]
        self.assertIn("github.event_name == 'schedule'", condition)
        self.assertIn("github.repository == 'noan98/VeloX'", condition)
        # `workflow_dispatch` を許すのは main 上だけ。
        self.assertIn("github.ref == 'refs/heads/main'", condition)
        self.assertNotIn("pull_request", condition)

    def test_only_the_ingest_job_gets_write_permission(self) -> None:
        """計測ジョブ (pull_request でも走る) に書き込み権限を広げない。"""
        self.assertEqual(self.ingest["permissions"], {"contents": "write", "pull-requests": "write"})
        self.assertNotIn("permissions", self.bench)
        self.assertNotIn("permissions", self.doc)

    def test_the_bench_step_publishes_the_conditions_it_actually_used(self) -> None:
        """`inputs.compare_env` は schedule では空。補完後の値を渡すこと。

        ここを `inputs.*` から読み直すと、B 腕の結果が「条件不明」として
        取り込まれなくなる (`perf_history_plan.py` の RefusalTest)。
        """
        for key in ("compare_env", "common_env"):
            with self.subTest(key=key):
                self.assertEqual(self.bench["outputs"][key], "${{ steps.bench_run.outputs." + key + " }}")
                self.assertIn(f'"{key}=$', self.text)

    def test_the_ingest_job_refuses_to_touch_anything_outside_the_history(self) -> None:
        """書き込み権限を持つ自動ジョブの被害範囲を字面で固定する。"""
        runs = self._runs(code_only=True)
        self.assertIn("grep -v '^results/history/'", runs)
        self.assertIn("git add results/history", runs)

    def test_the_emptiness_check_can_see_a_brand_new_history_file(self) -> None:
        """`git diff` は未追跡ファイルを見ない。

        履歴ファイルは初回だけ新規作成なので、`git diff --quiet` で判定すると
        **1 回目の取り込みが「変更なし」として静かに捨てられる。**
        """
        runs = self._runs(code_only=True)
        self.assertIn("git status --porcelain", runs)
        self.assertNotIn("git diff --quiet", runs)

    def test_untracked_directories_are_not_collapsed(self) -> None:
        """既定の porcelain は未追跡ディレクトリを 1 行に畳む。

        初回は `results/history/windows/` ごと未追跡なので、畳まれると
        `?? results/` としか出ず、**範囲外判定が初回で必ず落ちる。**
        `--untracked-files=all` が無ければこの配線は 1 度も成功しない。
        """
        self.assertIn("--untracked-files=all", self._runs(code_only=True))

    def test_the_artifact_is_unpacked_outside_the_work_tree(self) -> None:
        """リポジトリ内に展開すると、未追跡ファイルが範囲外検査を汚す。"""
        download = [
            s for s in self.ingest["steps"] if (s.get("uses") or "").startswith("actions/download-artifact")
        ]
        self.assertEqual(len(download), 1)
        self.assertIn("runner.temp", download[0]["with"]["path"])

    def test_the_checkout_uses_the_same_token_that_opens_the_pull_request(self) -> None:
        """`GITHUB_TOKEN` の push は `synchronize` を発火させない (D91 と同じ罠)。

        作成だけ PAT にしても、2 回目以降の更新で CI が回らなくなる。
        """
        checkout = [s for s in self.ingest["steps"] if (s.get("uses") or "").startswith("actions/checkout")]
        self.assertEqual(len(checkout), 1)
        self.assertIn("AUTO_MERGE_TOKEN", checkout[0]["with"]["token"])

    def test_the_pull_request_body_file_exists(self) -> None:
        runs = self._runs(code_only=True)
        match = re.search(r"--body-file (\S+)", runs)
        self.assertIsNotNone(match, "--body-file が見つからない")
        assert match is not None
        self.assertTrue((ROOT / match.group(1)).is_file(), match.group(1))

    def test_the_generated_pull_request_never_closes_an_issue(self) -> None:
        """毎週作られる PR が Issue を閉じてしまわないこと (D128 / D131)。

        本文の定型文と、ジョブが作るコミットメッセージの両方を見る。
        """
        import sys

        sys.path.insert(0, str(Path(__file__).resolve().parent))
        from check_closing_keywords import extract_closing_issues

        runs = self._runs(code_only=True)
        match = re.search(r"--body-file (\S+)", runs)
        assert match is not None
        body = (ROOT / match.group(1)).read_text(encoding="utf-8")
        self.assertEqual(extract_closing_issues(body), [])
        # コミットメッセージは Markdown として解釈されない (D131 決定3)。
        for commit_message in re.findall(r'-m "([^"]*)"', runs):
            with self.subTest(message=commit_message):
                self.assertEqual(extract_closing_issues(commit_message, markdown=False), [])


if __name__ == "__main__":
    unittest.main()
