#!/usr/bin/env python3
"""`check_closing_keywords.py` の単体テスト (Issue #253 / D128)。

この検査が守るのは「**PR 本文に書いていない Issue が、コミットメッセージ
経由で閉じられる**」という壊れ方である。Issue #247 が同じ日に 2 度目の
誤クローズをされた経路そのもの。
"""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from check_closing_keywords import _split_messages, main, unintended_closes  # noqa: E402


class UnintendedClosesTest(unittest.TestCase):
    def test_the_actual_accident_is_detected(self):
        """**これが本題。** PR #250 のコミット `d189b19` の実文面。

        PR 本文は #247 のキーワードを意図的に外していたが、コミット
        メッセージにバッククォート囲みで字面が残っていた。GitHub 本体は
        それを読んで #247 を閉じた。
        """
        commit = (
            "fix(ci): 自動クローズがインラインコード内のキーワードを拾わないようにする (#252)\n"
            "\n"
            "そのため PR #249 の「この PR に `Closes #247` は入れていません」という\n"
            "否定文からキーワードが抽出され、マージ 3 秒後に Issue #247 が\n"
            "--reason completed で閉じられた。\n"
        )
        self.assertEqual(unintended_closes("本文... Closes #252", [commit]), [247])

    def test_backticks_do_not_protect_a_commit_message(self):
        """**PR 本文で効く逃げ道が、コミットメッセージでは効かない。**

        GitHub はコミットメッセージを Markdown として解釈しない
        (D128 決定4)。ここを `markdown=True` で読むと、検査は事故を
        素通りさせる。
        """
        self.assertEqual(unintended_closes("", ["説明: `Closes #5` と書いた"]), [5])

    def test_a_quoted_line_in_a_commit_message_is_not_protected_either(self):
        # 引用行も同じ。コミットメッセージに Markdown の意味は無い。
        self.assertEqual(unintended_closes("", ["説明:\n> Closes #6\n"]), [6])

    def test_a_fenced_block_in_a_commit_message_is_not_protected_either(self):
        self.assertEqual(unintended_closes("", ["例:\n```\nCloses #7\n```\n"]), [7])

    def test_a_declared_close_is_not_flagged(self):
        """PR #220 の形。本文に書いてあるなら意図どおりなので黙る。"""
        commit = "fix(ci): 猶予期間の満了で auto-merge が再評価されるようにする\n\nCloses #219\n"
        self.assertEqual(unintended_closes("本文...\n\nCloses #219", [commit]), [])

    def test_the_body_may_use_markdown_escapes(self):
        """**本文側は Markdown として読む。** GitHub 本体がそうするため。

        引用行に入れた宣言は GitHub も閉じないので、「宣言済み」には
        ならない — したがってコミット側の同じ番号は警告の対象になる。
        """
        self.assertEqual(unintended_closes("> Closes #8", ["Closes #8"]), [8])

    def test_a_body_only_close_is_normal(self):
        # CLAUDE.md が指示している普通の書き方。何も言わない。
        self.assertEqual(unintended_closes("Closes #9", ["docs: なにか書いた"]), [])

    def test_several_commits_are_all_scanned(self):
        commits = ["a: 何か", "b: 説明 Closes #11", "c: 別の説明 Fixes #12"]
        self.assertEqual(unintended_closes("Closes #10", commits), [11, 12])

    def test_duplicates_across_commits_are_reported_once(self):
        self.assertEqual(unintended_closes("", ["Closes #13", "Closes #13"]), [13])

    def test_no_commits_is_not_an_error(self):
        self.assertEqual(unintended_closes("Closes #14", []), [])

    def test_an_empty_body_still_flags_commit_closes(self):
        # 本文が空 = 何も宣言していない = コミットの close はすべて意図外。
        self.assertEqual(unintended_closes(None, ["Closes #15"]), [15])


class SplitMessagesTest(unittest.TestCase):
    """`git log --format=%B%x00` の出力を割る。

    **行ではなく NUL で区切る**必要がある — コミットメッセージ自体が
    改行を含むため。
    """

    def test_messages_are_split_on_nul(self):
        raw = "first line\nbody\0second\0"
        self.assertEqual(_split_messages(raw), ["first line\nbody", "second"])

    def test_trailing_and_blank_entries_are_dropped(self):
        self.assertEqual(_split_messages("a\0\0\0b\0"), ["a", "b"])

    def test_empty_input_gives_no_messages(self):
        self.assertEqual(_split_messages(""), [])

    def test_a_message_containing_newlines_survives_intact(self):
        raw = "subject\n\nbody line 1\nbody line 2\0"
        self.assertEqual(_split_messages(raw), ["subject\n\nbody line 1\nbody line 2"])


class BrokenWiringTest(unittest.TestCase):
    """配線が壊れているときは落とす (PR #257 のレビュー指摘)。

    **黙って緑を返すと、検知器が死んだまま誰も気づかない。** それは
    この検査が防ごうとしている「静かに歯止めが外れる」形そのものである。

    `ci.yml` の Python テスト探索 (「テストが 1 件も見つからない」で
    `::error::` + `exit 1`) と同じ規律。
    """

    def test_missing_arguments_fail_instead_of_passing_quietly(self):
        self.assertEqual(main(["check_closing_keywords.py"]), 1)

    def test_one_argument_fails(self):
        self.assertEqual(main(["check_closing_keywords.py", "body.txt"]), 1)

    def test_too_many_arguments_fail(self):
        self.assertEqual(main(["x", "a", "b", "c"]), 1)


if __name__ == "__main__":
    unittest.main()
