#!/usr/bin/env python3
"""`docs/decisions/` のカテゴリ索引を `archive.md` から生成し直す (Issue #227 / D116)。

    python3 .github/scripts/generate_decision_index.py

**Decision を追加したらこれを実行する。** 手順は 2 つだけ:

1. `decision_index.py` の `_ASSIGNMENTS` に、新しい番号を主担当カテゴリへ足す
2. このスクリプトを実行する

索引を手で書かないのが D116 決定2 の要点である。見出し・アンカー・並び順は
すべて `archive.md` から決まるので、手で書くと必ずずれる — Issue #227 の
起票時点では 55 件が未索引、既存リンク 22 本のうち 14 本がアンカー切れだった。

`test_decision_index.py` が、生成し忘れ・割り当て漏れ・リンク切れのいずれも
CI で落とす。
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

from decision_index import (
    CATEGORY_FILES,
    DECISIONS_DIR,
    category_of,
    decisions,
    ranges,
)

# 「対象」節の冒頭に置く要約行。網羅的な一覧は「詳細」節が持つ。
SUMMARY = (
    "主担当は **{ranges}** の {count} 件 "
    "(網羅的な一覧は下の「詳細」節。ここは読み進める手がかりとしての要約)。"
)

PREAMBLE = """**このカテゴリが主担当の Decision: {ranges}** ({count} 件)

`archive.md` の全 Decision は、いずれか 1 つのカテゴリが主担当として必ず
この一覧に載る。複数カテゴリにまたがるものは主担当だけに載せ、必要なら
本文から相互参照する (README の「重要なルール」3)。

⚠️ **この節は `generate_decision_index.py` が生成する。手で編集しない。**
`archive.md` との対応は `test_decision_index.py` が CI で検査するので、
追記漏れも、見出しを変えたことによるリンク切れも、そこで落ちる。
"""

README_ROWS = {
    "01": "エンジン、UI、依存関係、基本アーキテクチャ",
    "02": "タブ、履歴、ブックマーク、プライバシー、omnibox",
    "03": "メトリクス、ベンチマーク、メモリ、プロファイリング、回帰検知",
    "04": "コンテンツブロック、DevTools、ダウンロード、権限、サイトデータ、検索",
    "05": "Windows、アイコン、CI、リリース、マルチウィンドウ",
    "06": "入力値堅牢性、依存監査、セッション復元、設定",
}


def render_details(numbers: list[int], items: dict[int, tuple[str, str]]) -> str:
    lines = ["", PREAMBLE.format(ranges=ranges(numbers), count=len(numbers))]
    for number in numbers:
        heading, anchor = items[number]
        body = heading.split(":", 1)[1].strip() if ":" in heading else heading
        lines.append(f"- [D{number}](./archive.md#{anchor}) — {body}")
    lines += ["", "---", "", "- [設計判断アーカイブ (全文)](./archive.md)", ""]
    return "\n".join(lines)


def main() -> int:
    items = decisions()
    unassigned = sorted(n for n in items if category_of(n) is None)
    if unassigned:
        print(
            "decision_index.py の _ASSIGNMENTS に無い Decision がある: "
            + ", ".join(f"D{n}" for n in unassigned),
            file=sys.stderr,
        )
        return 1

    for category, filename in CATEGORY_FILES.items():
        path = DECISIONS_DIR / filename
        text = path.read_text(encoding="utf-8")
        numbers = sorted(n for n in items if category_of(n) == category)

        # 「対象」節の要約行を差し替える (無ければ挿入)。
        summary = SUMMARY.format(ranges=ranges(numbers), count=len(numbers))
        text = re.sub(
            r"(^## 対象\n)(\n主担当は \*\*.*?\n)?",
            lambda m: m.group(1) + "\n" + summary + "\n",
            text,
            count=1,
            flags=re.M | re.S,
        )

        # 「詳細」節を丸ごと置き換える。
        match = re.search(r"^(#{2,3}) 詳細\s*$", text, re.M)
        if not match:
            print(f"{filename} に「詳細」節が無い", file=sys.stderr)
            return 1
        text = (
            text[: match.start()]
            + f"{match.group(1)} 詳細\n"
            + render_details(numbers, items)
        )
        path.write_text(text, encoding="utf-8")
        print(f"{filename}: {len(numbers)} 件")

    # README のカテゴリ表。
    readme = DECISIONS_DIR / "README.md"
    text = readme.read_text(encoding="utf-8")
    rows = ["| ファイル | 対象 | Decision |", "|---|---|---|"]
    for category, filename in CATEGORY_FILES.items():
        numbers = sorted(n for n in items if category_of(n) == category)
        rows.append(f"| [{filename}](./{filename}) | {README_ROWS[category]} | {ranges(numbers)} |")
    table = re.compile(
        r"^\| ファイル \| 対象 \| Decision \|\n\|---\|---\|---\|\n(?:\|.*\n)+", re.M
    )
    if not table.search(text):
        print("README.md のカテゴリ表が見つからない", file=sys.stderr)
        return 1
    readme.write_text(table.sub("\n".join(rows) + "\n", text, count=1), encoding="utf-8")
    print(f"README.md: {len(items)} 件を 6 カテゴリに割り当て")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
