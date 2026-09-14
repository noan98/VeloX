#!/usr/bin/env python3
"""コミットメッセージが PR 本文の意図より多く Issue を閉じないか検査する。

Issue #253 / docs/decisions D128。**Issue #247 が同じ日に 2 度、別の経路で
誤クローズされた。** 1 度目は代行スクリプトがインラインコードを見落とした
もので #252 / D126 で直した。2 度目がここで扱う経路である。

## 2 度目に何が起きたか

PR #250 のコミット `d189b19` の**メッセージ本文**に、1 度目のバグを説明する
ための字面をそのまま書いていた。GitHub 本体はマージ先ブランチに載った
コミットメッセージを解析するので、それが `main` に入った時点で #247 が
閉じられた。

**PR 本文のほうは対策していた。** 盲点はコミットメッセージだった。

## なぜ「気をつける」では防げないのか

1 度目の対策を入れた**そのコミット自身**が 2 度目の経路を踏んだ。バグの説明に
キーワードを書くのは自然な行為で、D126 決定4 に書いたとおり**丁寧に書くほど
踏む罠**になっている。避け続けることを規律に頼れない。

## なぜバッククォートが効かないのか

D126 は「説明したいときはバッククォートで囲め」と CLAUDE.md に書いたが、
**GitHub のコミットメッセージ解析は Markdown のコードスパンを解釈しない。**
PR 本文で効く逃げ道が、ここでは効かない。

## だから検知する

GitHub 本体の解析は止められない。**閉じてしまったものを開き直すより、閉じる
前に気づくほうが安い** (#253 の「設計方針」)。

PR 本文に書かれた集合が**意図**である (CLAUDE.md 「Issue と PR の紐付け」が
そう指示している)。コミットメッセージにしか無い番号は、**意図していないのに
閉じられる**ものなので警告する。

逆向き (本文にあってコミットに無い) は**正常**なので何も言わない。

## 実データでの裏付け

`main` の全履歴を監査したところ、キーワード + 番号の出現は 7 件で、うち
6 件は意図的 (subject にも同じ番号がある)。**事故は `d189b19` の 1 件だけ**
だった。この検査を当時の履歴に掛けると、**誤検知 0 件・事故 1 件を検出**する。
"""

from __future__ import annotations

import sys
from collections.abc import Iterable

from extract_closing_issues import extract_closing_issues

#: コミットメッセージどうしの区切り。`git log --format=%B%x00` と揃える。
COMMIT_SEPARATOR = "\0"


def unintended_closes(pr_body: str | None, commit_messages: Iterable[str]) -> list[int]:
    """コミットメッセージにしか無いクロージング対象を、登場順に返す。

    **PR 本文が意図である。** そこに書かれていない番号をコミットメッセージが
    閉じるなら、それは書き手が意図していない。

    重複は除き、登場順を保つ (`extract_closing_issues` と同じ流儀)。
    """
    # 本文は Markdown として読む (GitHub 本体がそうする)。
    declared = set(extract_closing_issues(pr_body))
    seen: dict[int, None] = {}
    for message in commit_messages:
        # **コミットメッセージは Markdown ではない。** バッククォートで
        # 囲んでも GitHub は閉じる (D128 決定4)。ここで `markdown=True`
        # にすると、**事故そのものを見落とす** — Issue #247 を閉じた
        # `d189b19` のキーワードはバッククォートに囲まれていた。
        for issue in extract_closing_issues(message, markdown=False):
            if issue not in declared:
                seen.setdefault(issue, None)
    return list(seen.keys())


def _split_messages(raw: str) -> list[str]:
    """`git log --format=%B%x00` の出力を 1 コミットずつに割る。

    末尾の空要素は捨てる。コミットメッセージ自体に改行が含まれるので、
    **行ではなく NUL で区切る**必要がある。
    """
    return [part for part in raw.split(COMMIT_SEPARATOR) if part.strip()]


def main(argv: list[str]) -> int:
    """CLI: `check_closing_keywords.py <pr-body-file> <commit-messages-file>`

    警告を出すだけで**失敗させない** (終了コードは常に 0)。閉じる意図が
    無いキーワードは書き手の判断で残ることもありうるし、**この検査が理由で
    マージが止まると、止まった理由の調査コストのほうが事故の修復コストを
    上回る** — 誤クローズは再オープンで戻せる。
    """
    if len(argv) != 3:
        print("usage: check_closing_keywords.py <pr-body-file> <commit-messages-file>")
        return 0

    with open(argv[1], "r", encoding="utf-8") as f:
        body = f.read()
    with open(argv[2], "r", encoding="utf-8") as f:
        messages = _split_messages(f.read())

    unintended = unintended_closes(body, messages)
    if not unintended:
        print(f"OK: コミットメッセージ {len(messages)} 件に、PR 本文に無いクロージング対象はありません")
        return 0

    numbers = ", ".join(f"#{n}" for n in unintended)
    print(
        f"::warning::コミットメッセージが {numbers} を閉じますが、PR 本文には書かれていません。"
        "意図した close なら PR 本文にも書いてください。意図していないなら、"
        "コミットメッセージでキーワードと番号を隣接させないでください "
        "(バッククォートで囲んでもコミットメッセージでは効きません — Issue #253 / D128)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
