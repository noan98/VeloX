#!/usr/bin/env python3
"""extract_closing_issues.py のユニットテスト。

実行方法:
    python3 -m unittest .github/scripts/test_extract_closing_issues.py -v
または (このディレクトリから):
    python3 -m unittest test_extract_closing_issues -v

Issue #168 の受け入れ条件に挙げられたケースをそのまま網羅する。
"""

from __future__ import annotations

import unittest

from extract_closing_issues import extract_closing_issues


class ExtractClosingIssuesTest(unittest.TestCase):
    def test_closes(self) -> None:
        self.assertEqual(extract_closing_issues("Closes #123"), [123])

    def test_closes_lowercase(self) -> None:
        self.assertEqual(extract_closing_issues("closes #123"), [123])

    def test_closes_uppercase(self) -> None:
        self.assertEqual(extract_closing_issues("CLOSES #123"), [123])

    def test_fixes(self) -> None:
        self.assertEqual(extract_closing_issues("Fixes #123"), [123])

    def test_resolved(self) -> None:
        self.assertEqual(extract_closing_issues("Resolved #123"), [123])

    def test_all_keyword_variants(self) -> None:
        keywords = [
            "close",
            "closes",
            "closed",
            "fix",
            "fixes",
            "fixed",
            "resolve",
            "resolves",
            "resolved",
        ]
        for i, kw in enumerate(keywords):
            issue_no = 1000 + i
            with self.subTest(keyword=kw):
                body = f"{kw} #{issue_no}"
                self.assertEqual(extract_closing_issues(body), [issue_no])

    def test_single_line_multiple(self) -> None:
        self.assertEqual(
            extract_closing_issues("Closes #77, closes #73"), [77, 73]
        )

    def test_multi_line_multiple(self) -> None:
        body = "Closes #115\nCloses #116\nCloses #154\n"
        self.assertEqual(extract_closing_issues(body), [115, 116, 154])

    def test_code_block_ignored(self) -> None:
        body = "\n".join(
            [
                "普通の説明文です。",
                "```",
                "Closes #999",
                "```",
                "ここには何も無い。",
            ]
        )
        self.assertEqual(extract_closing_issues(body), [])

    def test_code_block_with_language_ignored(self) -> None:
        body = "```python\n# Closes #999\nprint('closes #999')\n```"
        self.assertEqual(extract_closing_issues(body), [])

    def test_mixed_code_block_and_real_keyword(self) -> None:
        body = "\n".join(
            [
                "Closes #1",
                "```",
                "Closes #999",
                "```",
                "Fixes #2",
            ]
        )
        self.assertEqual(extract_closing_issues(body), [1, 2])

    def test_quoted_line_ignored(self) -> None:
        body = "> Closes #999\n\n本文はこちら。"
        self.assertEqual(extract_closing_issues(body), [])

    def test_quoted_line_with_leading_space_ignored(self) -> None:
        body = "  > Closes #999\n"
        self.assertEqual(extract_closing_issues(body), [])

    def test_quoted_and_real_mixed(self) -> None:
        body = "> Closes #999\nCloses #1\n"
        self.assertEqual(extract_closing_issues(body), [1])

    def test_cross_repo_reference_ignored(self) -> None:
        self.assertEqual(
            extract_closing_issues("Closes owner/repo#123"), []
        )

    def test_cross_repo_reference_with_org_ignored(self) -> None:
        self.assertEqual(
            extract_closing_issues("Fixes anthropics/claude#123"), []
        )

    def test_bare_issue_number_ignored(self) -> None:
        self.assertEqual(extract_closing_issues("関連: #123"), [])

    def test_bare_issue_number_alone_ignored(self) -> None:
        self.assertEqual(extract_closing_issues("#123"), [])

    def test_no_keywords_at_all(self) -> None:
        body = "このPRは特に何かをクローズするものではありません。"
        self.assertEqual(extract_closing_issues(body), [])

    def test_empty_body(self) -> None:
        self.assertEqual(extract_closing_issues(""), [])

    def test_none_body(self) -> None:
        self.assertEqual(extract_closing_issues(None), [])

    def test_duplicate_issue_numbers_deduplicated(self) -> None:
        body = "Closes #1\nFixes #1\n"
        self.assertEqual(extract_closing_issues(body), [1])

    def test_substring_word_not_matched_as_keyword(self) -> None:
        # "prefixes" の中に "fixes" が部分文字列として含まれるが、
        # 単語境界が無いのでキーワードとして扱わない。
        self.assertEqual(extract_closing_issues("prefixes #123"), [])

    def test_pr_self_number_not_matched(self) -> None:
        body = "このPR (#170) は Closes #55 を含みます。"
        self.assertEqual(extract_closing_issues(body), [55])

    def test_colon_after_keyword(self) -> None:
        self.assertEqual(extract_closing_issues("Closes: #123"), [123])

    def test_inline_code_keyword_ignored(self) -> None:
        # Issue #252: PR #249 は「入れていません」という否定文なのに、
        # バッククォート囲みの文字列が拾われて #247 が誤クローズされた。
        # 実際の文面をそのまま回帰テストにする。
        body = (
            "**この PR に `Closes #247` は入れていません。** 項目1 は未解決の"
            "ままなので、\n#247 をその 1 点に絞って書き換えました。"
        )
        self.assertEqual(extract_closing_issues(body), [])

    def test_inline_code_double_backtick_ignored(self) -> None:
        self.assertEqual(extract_closing_issues("``Closes #123``"), [])

    def test_inline_code_does_not_swallow_real_keyword(self) -> None:
        # 本文に別のコードスパンがあっても、独立行のキーワードは拾う。
        body = "Closes #5\n\n`cargo test --lib` を通しました。"
        self.assertEqual(extract_closing_issues(body), [5])

    def test_issue_number_in_inline_code_after_keyword(self) -> None:
        # 番号側だけがコードスパンなら、キーワードは素のままなので拾う。
        body = "Closes #77 (`#77` は Epic の子)"
        self.assertEqual(extract_closing_issues(body), [77])

    def test_unclosed_backtick_does_not_swallow_following_lines(self) -> None:
        # 閉じていないバッククォートは書き損じとみなし、その行だけで止める。
        # 行をまたいで消すと下にある正当なキーワードを巻き添えにする。
        body = "途中で ` を書き損じた行\nCloses #9"
        self.assertEqual(extract_closing_issues(body), [9])

    def test_keyword_split_across_inline_code_boundary(self) -> None:
        # コードスパンの外にキーワード、中に番号 — GitHub 本体は
        # コードスパン内の参照をリンクしないので閉じない側に倒す。
        self.assertEqual(extract_closing_issues("Closes `#123`"), [])

    def test_a_stray_backtick_earlier_on_the_line_shifts_the_pairing(self) -> None:
        """既知のエッジケース。**意図した挙動として固定する。**

        同じ行に未閉じのバッククォートが先にあると、それが後続の
        コードスパンの開き側とペアになり、囲んだつもりのキーワードが
        地の文として残る。

        **これは CommonMark に忠実な結果である。** 仕様は「最左の
        バッククォート列と、それに続く最初の同じ長さの列をペアにする」
        と定めており、GitHub 本体も同じ行を同じように解釈する — つまり
        本体もこのキーワードを地の文として扱う。D83 の「本体の挙動に
        合わせる」という前提の範囲内なので、ここを本体より安全側へ
        倒すことはしない。

        PR #250 のレビューで指摘された経路。
        """
        body = "未閉じの ` の後に `Closes #5` と書いた行"
        self.assertEqual(extract_closing_issues(body), [5])

    def test_an_even_number_of_stray_backticks_keeps_the_span_intact(self) -> None:
        """裸のバッククォートが偶数個なら、ペアがずれず漏れない。"""
        body = "a ` b ` c `Closes #7` d"
        self.assertEqual(extract_closing_issues(body), [])

    def test_epic_body_from_claude_md_example(self) -> None:
        body = "Closes #115\nCloses #116\nCloses #154"
        self.assertEqual(extract_closing_issues(body), [115, 116, 154])


if __name__ == "__main__":
    unittest.main()
