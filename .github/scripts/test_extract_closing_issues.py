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

    def test_epic_body_from_claude_md_example(self) -> None:
        body = "Closes #115\nCloses #116\nCloses #154"
        self.assertEqual(extract_closing_issues(body), [115, 116, 154])


if __name__ == "__main__":
    unittest.main()
