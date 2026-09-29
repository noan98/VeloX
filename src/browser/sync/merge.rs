//! 端末内の同期状態 ([`SyncState`]) と、種別ごとの「見え方」(ビュー)。
//! docs/sync.md §6。
//!
//! 収束するのは [`SyncState`] (レコードの集合) で、これは [`Record::merge`]
//! だけで決まる。ブックマークの URL 重複・壊れた `folder` 参照・設定の
//! 許可リストのような**意味的な整合**はビュー側で毎回決定的に解決する
//! (状態を書き換えないので、どの端末でも同じ入力から同じ見え方になる)。

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use super::clock::Hlc;
use super::record::{Kind, Record, RecordKey};

/// レコードの集合。全端末で最終的に同一になる部分。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SyncState {
    records: BTreeMap<RecordKey, Record>,
}

impl SyncState {
    pub fn new() -> Self {
        Self::default()
    }

    /// レコード (またはデルタ) を結合する。変化があれば `true`。
    pub fn apply(&mut self, incoming: &Record) -> bool {
        match self.records.get_mut(&incoming.key) {
            Some(existing) => existing.merge(incoming),
            None => {
                let mut fresh = Record {
                    key: incoming.key.clone(),
                    fields: BTreeMap::new(),
                    tombstone: None,
                };
                fresh.merge(incoming);
                self.records.insert(incoming.key.clone(), fresh);
                true
            }
        }
    }

    pub fn get(&self, key: &RecordKey) -> Option<&Record> {
        self.records.get(key)
    }

    /// 墓石を含む全レコード (完全再送・スナップショット用)。
    pub fn all(&self) -> impl Iterator<Item = &Record> {
        self.records.values()
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// 生きているレコードだけ (種別指定)。
    pub fn live(&self, kind: Kind) -> impl Iterator<Item = &Record> {
        self.records
            .values()
            .filter(move |r| r.key.kind == kind && r.is_live())
    }

    /// 墓石の刈り取り。`wall < before_wall` の墓石を持つ死んだレコードを
    /// 捨てる。**全端末がその墓石を受け取った後でなければ安全でない**
    /// (古い端末が復帰すると削除が巻き戻る) — 呼び出し側 (将来はサーバの
    /// 最小 ack カーソル) が保証する。docs/sync.md §5.4。
    pub fn compact(&mut self, before_wall: u64) -> usize {
        let before = self.records.len();
        self.records.retain(|_, r| {
            r.is_live() || !matches!(&r.tombstone, Some(Hlc { wall, .. }) if *wall < before_wall)
        });
        before - self.records.len()
    }
}

// ---- ブックマーク ---------------------------------------------------------

/// ブックマークの見え方。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookmarkView {
    pub id: String,
    pub url: String,
    pub title: Option<String>,
    /// 実在する (生きている) フォルダの ID、なければルート。
    pub folder: Option<String>,
    pub order: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderView {
    pub id: String,
    pub name: String,
    pub order: i64,
}

fn str_field(r: &Record, name: &str) -> Option<String> {
    r.get(name).and_then(Value::as_str).map(str::to_owned)
}

fn int_field(r: &Record, name: &str) -> i64 {
    r.get(name).and_then(Value::as_i64).unwrap_or(0)
}

/// フォルダ一覧 (名前は必須。欠けたレコードは未完成として隠す)。
pub fn folder_view(state: &SyncState) -> Vec<FolderView> {
    let mut v: Vec<FolderView> = state
        .live(Kind::BookmarkFolder)
        .filter_map(|r| {
            Some(FolderView {
                id: r.key.id.clone(),
                name: str_field(r, "name")?,
                order: int_field(r, "order"),
            })
        })
        .collect();
    v.sort_by(|a, b| (a.order, &a.id).cmp(&(b.order, &b.id)));
    v
}

/// ブックマーク一覧。
///
/// - `url` が無いレコードは未完成として隠す。
/// - **同じ URL は 1 件にまとめる** (ローカルストアが URL で重複排除する
///   D32 と揃える)。勝者は `(created_at, id)` が最小のもの。敗者は状態には
///   残る (隠すだけ) ので、勝者が削除されれば次の候補が表に出る。
/// - `folder` が存在しない/削除済みのフォルダを指すならルートに落とす
///   (D62 の壊れた `folder_id` の自己修復と同じ扱い)。
/// - 並びは `(folder, order, id)`。手動並べ替えは `order` の LWW。
pub fn bookmark_view(state: &SyncState) -> Vec<BookmarkView> {
    let folders: BTreeSet<String> = folder_view(state).into_iter().map(|f| f.id).collect();
    let mut winners: BTreeMap<String, (u64, &Record)> = BTreeMap::new();
    for r in state.live(Kind::Bookmark) {
        let Some(url) = str_field(r, "url") else {
            continue;
        };
        let created = r.get("created_at").and_then(Value::as_u64).unwrap_or(0);
        match winners.get(&url) {
            Some((c, w)) if (*c, &w.key.id) <= (created, &r.key.id) => {}
            _ => {
                winners.insert(url, (created, r));
            }
        }
    }
    let mut v: Vec<BookmarkView> = winners
        .into_iter()
        .map(|(url, (_, r))| BookmarkView {
            id: r.key.id.clone(),
            url,
            title: str_field(r, "title"),
            folder: str_field(r, "folder").filter(|f| folders.contains(f)),
            order: int_field(r, "order"),
        })
        .collect();
    v.sort_by(|a, b| (&a.folder, a.order, &a.id).cmp(&(&b.folder, b.order, &b.id)));
    v
}

// ---- 設定 -----------------------------------------------------------------

/// 同期する設定グループ (許可リスト)。`performance` (端末の RAM/CPU 依存)、
/// `downloads` (保存先パス)、`advanced` (端末固有の診断) は同期しない。
pub const SYNCED_SETTING_GROUPS: &[&str] = &["general", "appearance", "search", "privacy"];

/// `group.key` 形式の設定パスが同期対象か。
pub fn is_syncable_setting(path: &str) -> bool {
    match path.split_once('.') {
        Some((group, key)) => !key.is_empty() && SYNCED_SETTING_GROUPS.contains(&group),
        None => false,
    }
}

/// 生きている同期対象の設定 (`path -> value`)。許可リスト外のキーは、
/// リモートが送ってきても**見せない** (悪意ある端末が保存先パスなどを
/// 書き換えられない)。
pub fn settings_view(state: &SyncState) -> BTreeMap<String, Value> {
    state
        .live(Kind::Setting)
        .filter(|r| is_syncable_setting(&r.key.id))
        .filter_map(|r| Some((r.key.id.clone(), r.get("value")?.clone())))
        .collect()
}

// ---- 履歴 -----------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryView {
    pub id: String,
    pub url: String,
    pub title: Option<String>,
    pub visited_at: u64,
}

/// 履歴 (新しい順)。訪問は追記のみ・端末が採番した ID なので衝突せず、
/// 和集合になる。タイトルは後から入る LWW フィールド。
pub fn history_view(state: &SyncState) -> Vec<HistoryView> {
    let mut v: Vec<HistoryView> = state
        .live(Kind::History)
        .filter_map(|r| {
            Some(HistoryView {
                id: r.key.id.clone(),
                url: str_field(r, "url")?,
                title: str_field(r, "title"),
                visited_at: r.get("visited_at").and_then(Value::as_u64)?,
            })
        })
        .collect();
    v.sort_by(|a, b| {
        (std::cmp::Reverse(a.visited_at), &a.id).cmp(&(std::cmp::Reverse(b.visited_at), &b.id))
    });
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::sync::record::tests::hlc;
    use serde_json::json;

    fn put(kind: Kind, id: &str, h: Hlc, fields: &[(&str, Value)]) -> Record {
        Record::put(
            RecordKey::new(kind, id),
            &h,
            fields.iter().map(|(k, v)| (k.to_string(), v.clone())),
        )
    }

    fn bm(id: &str, url: &str, created: u64, wall: u64) -> Record {
        put(
            Kind::Bookmark,
            id,
            hlc(wall, 0, "a"),
            &[("url", json!(url)), ("created_at", json!(created))],
        )
    }

    #[test]
    fn duplicate_urls_collapse_to_oldest_deterministically() {
        let mut s = SyncState::new();
        s.apply(&bm("z", "https://x/", 5, 10));
        s.apply(&bm("y", "https://x/", 5, 11));
        s.apply(&bm("a", "https://x/", 9, 12));
        let v = bookmark_view(&s);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].id, "y");
        // 勝者を消すと次の候補が出る。
        s.apply(&Record::delete(
            RecordKey::new(Kind::Bookmark, "y"),
            &hlc(20, 0, "a"),
        ));
        assert_eq!(bookmark_view(&s)[0].id, "z");
    }

    #[test]
    fn dangling_folder_falls_back_to_root() {
        let mut s = SyncState::new();
        s.apply(&put(
            Kind::BookmarkFolder,
            "f1",
            hlc(1, 0, "a"),
            &[("name", json!("Work"))],
        ));
        let mut b = bm("b1", "https://x/", 1, 2);
        b.merge(&put(
            Kind::Bookmark,
            "b1",
            hlc(3, 0, "a"),
            &[("folder", json!("f1"))],
        ));
        s.apply(&b);
        assert_eq!(bookmark_view(&s)[0].folder.as_deref(), Some("f1"));
        // 別端末がフォルダを削除 → ルートへ (ブックマークは失われない)。
        s.apply(&Record::delete(
            RecordKey::new(Kind::BookmarkFolder, "f1"),
            &hlc(9, 0, "b"),
        ));
        let v = bookmark_view(&s);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].folder, None);
    }

    #[test]
    fn concurrent_reorder_is_lww_and_view_is_sorted() {
        let mut s = SyncState::new();
        s.apply(&bm("a", "https://a/", 1, 1));
        s.apply(&bm("b", "https://b/", 2, 1));
        s.apply(&put(
            Kind::Bookmark,
            "a",
            hlc(5, 0, "x"),
            &[("order", json!(20))],
        ));
        s.apply(&put(
            Kind::Bookmark,
            "a",
            hlc(6, 0, "y"),
            &[("order", json!(-1))],
        ));
        s.apply(&put(
            Kind::Bookmark,
            "b",
            hlc(5, 0, "x"),
            &[("order", json!(0))],
        ));
        let ids: Vec<_> = bookmark_view(&s).into_iter().map(|b| b.id).collect();
        assert_eq!(ids, ["a", "b"]);
    }

    #[test]
    fn incomplete_records_are_hidden() {
        let mut s = SyncState::new();
        s.apply(&put(
            Kind::Bookmark,
            "b",
            hlc(1, 0, "a"),
            &[("title", json!("t"))],
        ));
        assert!(bookmark_view(&s).is_empty());
        s.apply(&put(
            Kind::BookmarkFolder,
            "f",
            hlc(1, 0, "a"),
            &[("order", json!(1))],
        ));
        assert!(folder_view(&s).is_empty());
    }

    #[test]
    fn settings_allowlist_and_per_key_lww() {
        let mut s = SyncState::new();
        let set =
            |id: &str, w: u64, v: Value| put(Kind::Setting, id, hlc(w, 0, "a"), &[("value", v)]);
        s.apply(&set("appearance.theme", 1, json!("dark")));
        s.apply(&set("appearance.theme", 2, json!("light")));
        s.apply(&set("search.engine", 1, json!("ddg")));
        s.apply(&set("downloads.directory", 1, json!("/evil")));
        s.apply(&set("performance.budget", 1, json!(1)));
        s.apply(&set("nogroup", 1, json!(1)));
        let v = settings_view(&s);
        assert_eq!(v.len(), 2);
        assert_eq!(v["appearance.theme"], json!("light"));
        assert!(is_syncable_setting("general.language"));
        assert!(!is_syncable_setting("general."));
    }

    #[test]
    fn history_union_sorted_desc_title_lww() {
        let mut s = SyncState::new();
        let visit = |id: &str, at: u64, w: u64| {
            put(
                Kind::History,
                id,
                hlc(w, 0, "a"),
                &[("url", json!("https://x/")), ("visited_at", json!(at))],
            )
        };
        s.apply(&visit("h1", 100, 1));
        s.apply(&visit("h2", 200, 2));
        s.apply(&put(
            Kind::History,
            "h1",
            hlc(3, 0, "a"),
            &[("title", json!("T"))],
        ));
        let v = history_view(&s);
        assert_eq!(v[0].id, "h2");
        assert_eq!(v[1].title.as_deref(), Some("T"));
        s.apply(&Record::delete(
            RecordKey::new(Kind::History, "h2"),
            &hlc(9, 0, "b"),
        ));
        assert_eq!(history_view(&s).len(), 1);
    }

    #[test]
    fn compact_drops_only_old_dead_records() {
        let mut s = SyncState::new();
        s.apply(&bm("live", "https://l/", 1, 1));
        s.apply(&Record::delete(
            RecordKey::new(Kind::Bookmark, "old"),
            &hlc(5, 0, "a"),
        ));
        s.apply(&Record::delete(
            RecordKey::new(Kind::Bookmark, "new"),
            &hlc(50, 0, "a"),
        ));
        assert_eq!(s.compact(10), 1);
        assert_eq!(s.len(), 2);
        assert!(s.get(&RecordKey::new(Kind::Bookmark, "old")).is_none());
        assert!(s.get(&RecordKey::new(Kind::Bookmark, "new")).is_some());
    }

    #[test]
    fn apply_reports_change() {
        let mut s = SyncState::new();
        let r = bm("a", "https://a/", 1, 1);
        assert!(s.apply(&r));
        assert!(!s.apply(&r));
    }
}
