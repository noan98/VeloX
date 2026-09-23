//! `browser::` の複数モジュールで共有する小さな汎用ヘルパー。
//!
//! 特定の機能に属さない、文字列や `Vec` に対する数行の処理だけを置く。
//! 各モジュールがそれぞれ同じ実装を持っていたものを 1 か所にまとめたもので、
//! 振る舞いは元の実装と同一。

/// `s` を UTF-8 で最大 `max_bytes` バイトに切り詰める。マルチバイト文字の
/// 途中では切らず (スライスが panic しないよう) 手前の `char` 境界まで戻す。
/// `s` が収まっていればそのまま返す。
pub(crate) fn truncate_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// `pred` に当てはまる要素を `items` からすべて取り除き、1 件でも消えたら
/// `true` を返す。
pub(crate) fn remove_where<T>(items: &mut Vec<T>, mut pred: impl FnMut(&T) -> bool) -> bool {
    let before = items.len();
    items.retain(|item| !pred(item));
    items.len() != before
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_utf8_returns_input_unchanged_when_it_fits() {
        assert_eq!(truncate_utf8("hello", 5), "hello");
        assert_eq!(truncate_utf8("hello", 100), "hello");
        assert_eq!(truncate_utf8("", 0), "");
    }

    #[test]
    fn truncate_utf8_backs_off_to_a_char_boundary() {
        // "あ" は 3 バイト。4 バイト目までに収まるのは先頭 1 文字だけ。
        assert_eq!(truncate_utf8("ああ", 4), "あ");
        assert_eq!(truncate_utf8("ああ", 2), "");
        assert_eq!(truncate_utf8("hello", 0), "");
    }

    #[test]
    fn remove_where_reports_whether_anything_was_removed() {
        let mut items = vec![1, 2, 3, 2];
        assert!(remove_where(&mut items, |&n| n == 2));
        assert_eq!(items, vec![1, 3]);
        assert!(!remove_where(&mut items, |&n| n == 9));
        assert_eq!(items, vec![1, 3]);
    }
}
