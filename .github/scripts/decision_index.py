"""`docs/decisions/` のカテゴリ索引と `archive.md` の対応を扱う共有モジュール。

Issue #227。`docs/decisions/README.md` は「現在の設計を知りたい →
カテゴリ別インデックスから該当する Decision を開く」を入口として案内して
いるが、起票時点でその入口から辿れるのは 108 件中 53 件だけだった。さらに
**既存リンク 22 本のうち 14 本はアンカーが実在しない** (`archive.md` が
日本語見出しに書き換えられた際、リンク側が旧英語見出しのまま残っていた)。

このモジュールは、その対応を「人が手で維持するもの」から「機械が検査できる
もの」に変える:

- [`decisions`] が `archive.md` から Decision 番号・見出し・アンカーを取り出す
- [`category_of`] が主担当カテゴリを返す (README の「重要なルール」3)
- `test_decision_index.py` が両者と実ファイルの突き合わせを検査する

## アンカー生成について

GitHub は Markdown 見出しから `github-slugger` でアンカーを作る。[`slug`] は
その規則の移植で、**`archive.md` の全 115 見出しに対して本家
`github-slugger` と 1 件の差も無いことを確認してある** (Issue #227 の作業
中に `npm i github-slugger` で突き合わせ)。

⚠️ **素朴な「記号をハイフンに置換」では合わない。** GitHub は `、` `・`
`「」` `→` `—` を**ハイフンにせず除去する** (`セキュリティ・入力値` →
`セキュリティ入力値`)。この点を推測で実装して 115 本のリンクを量産すると、
今回直したのと同じリンク切れをそのまま作り直すことになる。
"""

from __future__ import annotations

import re
import unicodedata
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
DECISIONS_DIR = ROOT / "docs" / "decisions"
ARCHIVE = DECISIONS_DIR / "archive.md"

CATEGORY_FILES = {
    "01": "01-foundation.md",
    "02": "02-tabs-session.md",
    "03": "03-performance.md",
    "04": "04-browser-features.md",
    "05": "05-platform-release.md",
    "06": "06-security-maintenance.md",
}

# 主担当カテゴリ。D1–D70 は `README.md` の表が元々示していた割り当てを
# そのまま引き継ぎ、丸ごと未索引だった D71 以降を新たに割り当てた。
# 複数カテゴリにまたがる Decision は 1 つだけをここに書く (README の
# 「重要なルール」3)。
_ASSIGNMENTS: dict[str, list[int | tuple[int, int]]] = {
    # エンジン、UI、依存関係、基本アーキテクチャ
    "01": [(1, 7)],
    # タブ、履歴、ブックマーク、プライバシー、omnibox
    "02": [(8, 15), 20, (22, 40), 74, 119],
    # メトリクス、ベンチマーク、メモリ、プロファイリング、回帰検知
    "03": [
        16, 19, 21, (41, 49), (56, 58),
        79, 80, 81, 82, (84, 90), (92, 97), 99, 101, (104, 106), (109, 115), 117, 118,
        120,
    ],
    # コンテンツブロック、DevTools、ダウンロード、権限、サイトデータ、検索
    "04": [17, 18, (59, 60), 64, 66, 69, 71, 72, (75, 78)],
    # Windows、アイコン、CI、リリース、マルチウィンドウ
    "05": [(50, 55), 68, 70, 73, 83, 91, 98, 100, 103, 107, 108],
    # 入力値堅牢性、依存監査、セッション復元、設定
    "06": [61, 62, 63, 65, 67, 102, 116],
}

CATEGORIES = tuple(CATEGORY_FILES)


def _expand(spans: list[int | tuple[int, int]]) -> list[int]:
    out: list[int] = []
    for span in spans:
        if isinstance(span, tuple):
            out.extend(range(span[0], span[1] + 1))
        else:
            out.append(span)
    return out


_CATEGORY_OF: dict[int, str] = {}
for _cat, _spans in _ASSIGNMENTS.items():
    for _n in _expand(_spans):
        if _n in _CATEGORY_OF:  # pragma: no cover - 開発時の取り違え検知
            raise AssertionError(
                f"D{_n} が {_CATEGORY_OF[_n]} と {_cat} の両方に割り当てられている"
            )
        _CATEGORY_OF[_n] = _cat


def category_of(number: int) -> str | None:
    """主担当カテゴリ ("01"〜"06")。未割り当てなら `None`。"""
    return _CATEGORY_OF.get(number)


def slug(heading_text: str) -> str:
    """GitHub が Markdown 見出しから生成するアンカー (`github-slugger` 相当)。

    小文字化 → 句読点・記号・制御文字を除去 (`-` と `_` は残す) →
    空白を `-` に。除去であってハイフンへの置換ではない点が要:
    `セキュリティ・入力値` は `セキュリティ入力値` になる。
    """
    out: list[str] = []
    for ch in heading_text.strip().lower():
        if ch in "-_":
            out.append(ch)
            continue
        category = unicodedata.category(ch)
        if category[0] in ("P", "S", "C"):
            continue
        if category[0] == "Z" or ch.isspace():
            out.append(" ")
        else:
            out.append(ch)
    return "".join(out).replace(" ", "-")


def decisions(archive: Path = ARCHIVE) -> dict[int, tuple[str, str]]:
    """`archive.md` の `## Dxx: ...` 見出しを {番号: (見出し, アンカー)} で返す。"""
    found: dict[int, tuple[str, str]] = {}
    for line in archive.read_text(encoding="utf-8").split("\n"):
        if not line.startswith("## D"):
            continue
        text = line[3:]
        match = re.match(r"^D(\d+)\b", text)
        if not match:
            continue
        number = int(match.group(1))
        if number in found:  # pragma: no cover - 開発時の取り違え検知
            raise AssertionError(f"D{number} の見出しが複数ある")
        found[number] = (text, slug(text))
    return found


def ranges(numbers: list[int]) -> str:
    """連番をまとめた表示 (`D41–D49, D56` のような形)。"""
    parts: list[str] = []
    numbers = sorted(numbers)
    i = 0
    while i < len(numbers):
        j = i
        while j + 1 < len(numbers) and numbers[j + 1] == numbers[j] + 1:
            j += 1
        parts.append(f"D{numbers[i]}" if i == j else f"D{numbers[i]}–D{numbers[j]}")
        i = j + 1
    return ", ".join(parts)
