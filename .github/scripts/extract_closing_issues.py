#!/usr/bin/env python3
"""PR 本文からクロージングキーワード付きの Issue 番号を抽出する。

Issue #168: auto-merge.yml が GITHUB_TOKEN でマージするため、`Closes #NN`
などのキーワードによる GitHub 標準の自動クローズが効いていない
(GITHUB_TOKEN の操作は GitHub 側のそれ以上の自動処理を誘発しない)。
そのため auto-merge.yml がマージ成功後に本文を自分でパースし、対象の
Issue を明示的に `gh issue close` する。本モジュールはそのパース部分だけを
切り出したもので、workflow の YAML には抽出ロジックをベタ書きしない
(テスト可能にするため。docs/decisions.md D83 参照)。

GitHub 本体の挙動に合わせて以下をサポートする:

- キーワード: close/closes/closed, fix/fixes/fixed, resolve/resolves/resolved
  (大文字小文字を区別しない)
- 1 行に複数 (`Closes #77, closes #73`) / 複数行にまたがる併記
- フェンスコードブロック (``` ... ```) 内と、引用行 (`>` で始まる行) 内の
  キーワードは無視する (CLAUDE.md 「Issue と PR の紐付け」節と同じ注意書き)
- `owner/repo#123` のような他リポジトリ参照は拾わない (キーワード直後に
  空白 + `#数字` が続く形しか受け付けないため、`owner/repo#123` のように
  `#` の直前が `/` を含む識別子の場合は自然にマッチしない)
- キーワードを伴わない `#123` 単独はキーワードマッチの対象外なので拾わない
- PR 番号自体 (`(#170)` など) はキーワードを伴わないので拾わない
"""

from __future__ import annotations

import re
import sys

# フェンスコードブロック (``` ... ```)。開始側の言語指定 (```python 等) も
# まとめて除去する。閉じフェンスが無い (本文が壊れている) 場合でも、
# 全体を安全側 (コードとみなして無視) に倒すため DOTALL で欲張りに消す。
_CODE_BLOCK_RE = re.compile(r"```.*?```", re.DOTALL)
_UNCLOSED_CODE_BLOCK_RE = re.compile(r"```.*\Z", re.DOTALL)

# クロージングキーワード (GitHub がサポートするもの一式)。
_KEYWORDS = (
    "close",
    "closes",
    "closed",
    "fix",
    "fixes",
    "fixed",
    "resolve",
    "resolves",
    "resolved",
)

# 「キーワード + 任意のコロン + 空白 + #数字」。
# キーワード直後が空白ではなく `owner/repo#123` のような文字列の場合は
# マッチしないため、他リポジトリ参照は自然に除外される。
_CLOSES_RE = re.compile(
    r"\b(?:" + "|".join(_KEYWORDS) + r")\b\s*:?\s+#(\d+)\b",
    re.IGNORECASE,
)


def _strip_code_blocks(text: str) -> str:
    text = _CODE_BLOCK_RE.sub(" ", text)
    # 閉じられていないフェンスが残っていたら、そこから末尾まで無視する。
    text = _UNCLOSED_CODE_BLOCK_RE.sub(" ", text)
    return text


def _strip_quoted_lines(text: str) -> str:
    kept = []
    for line in text.splitlines():
        if line.lstrip().startswith(">"):
            continue
        kept.append(line)
    return "\n".join(kept)


def extract_closing_issues(pr_body: str | None) -> list[int]:
    """PR 本文からクロージングキーワードが付いた Issue 番号を抽出する。

    コードブロック・引用行を除去したうえでキーワードを走査し、登場順を
    保ちつつ重複を除いた Issue 番号のリストを返す。キーワードが 1 つも
    見つからなければ空リストを返す。
    """
    if not pr_body:
        return []

    text = _strip_code_blocks(pr_body)
    text = _strip_quoted_lines(text)

    seen: dict[int, None] = {}
    for match in _CLOSES_RE.finditer(text):
        seen.setdefault(int(match.group(1)), None)
    return list(seen.keys())


def main(argv: list[str]) -> int:
    """CLI: 標準入力 (または引数で渡されたファイル) から PR 本文を読み、
    抽出した Issue 番号を 1 行 1 件で標準出力に書く。
    キーワードが無ければ何も出力しない (終了コードは常に 0)。
    """
    if len(argv) > 1:
        with open(argv[1], "r", encoding="utf-8") as f:
            body = f.read()
    else:
        body = sys.stdin.read()

    for issue_number in extract_closing_issues(body):
        print(issue_number)
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
