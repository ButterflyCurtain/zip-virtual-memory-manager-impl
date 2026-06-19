//! In-memory エントリ表（設計 ENTRY OPERATIONS）。
//!
//! immutable な vmidx の上に被せ、セッション内の **構造変更**（create / remove、
//! ④b で rename）を表現するオーバーレイ。実効的なエントリ集合は
//! 「vmidx ∪ created − tombstone（+ rename）」で、[`kind`](EntryTable::kind) が
//! 1 名前についてそれを判定する。
//!
//! このモジュールは存在と「未変更データの出どころ」だけを持つ純データ構造。
//! 論理サイズ（`logical_size`）と dirty ページは [`DiffLayer`](crate::difflayer)
//! 側に残し、I/O・journaling は呼び出し側（[`mount`](crate::mount) /
//! [`disk`](crate::disk)）の責務。truncate は構造を変えない（論理サイズの変更 +
//! ページ整理）ので、ここには記録しない。
//!
//! **④a の範囲**: Created / Tombstone のみ。rename（現在名 → 別ソース名への
//! 写像）は ④b で `Aliased { source }` を加えて表現する。それまで「あるエントリの
//! ソース名は現在名に一致する」（[`Kind::Source`]）。

use std::collections::HashMap;

/// 1 名前に対するオーバーレイ判断。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Overlay {
    /// セッション内で create された（ソース無し）。同名が vmidx にあっても
    /// 「新規の空エントリ」として扱う（create-after-remove のリスタート含む）。
    Created,
    /// remove された（tombstone）。read/write は ENOENT、commit は出力しない。
    Tombstone,
    // ④b: Aliased { source: String } を追加して rename を表現する。
    //      kind() は Source を返しつつ、ソース読み出しは `source` 名で行う。
}

/// エントリの実効的な種別（vmidx ∪ created − tombstone）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// 存在しない（tombstone、または元々 vmidx に無い）。ENOENT。
    Absent,
    /// セッション内 created。未変更データのソースは無く、`logical_size` 内の
    /// 未書き込みページはゼロ。
    Created,
    /// vmidx 由来。未変更ページはソース（④a では同名）から読む。
    Source,
}

/// vmidx に被せる in-memory エントリ表。
#[derive(Default)]
pub struct EntryTable {
    overlay: HashMap<String, Overlay>,
}

impl EntryTable {
    /// 空のエントリ表（構造変更なし＝実効集合は vmidx のまま）。
    pub fn new() -> EntryTable {
        EntryTable::default()
    }

    /// 構造変更が 1 件も無いか（commit すべきオーバーレイが空か）。
    pub fn is_empty(&self) -> bool {
        self.overlay.is_empty()
    }

    /// 名前 `name` の実効種別。`in_vmidx` は同名のソースエントリが vmidx に
    /// 在るか（呼び出し側が `Vmidx::lookup` で判定して渡す）。
    pub fn kind(&self, name: &str, in_vmidx: bool) -> Kind {
        match self.overlay.get(name) {
            Some(Overlay::Tombstone) => Kind::Absent,
            Some(Overlay::Created) => Kind::Created,
            None => {
                if in_vmidx {
                    Kind::Source
                } else {
                    Kind::Absent
                }
            }
        }
    }

    /// `name` を created にする（create / create-after-remove のリスタート）。
    pub fn mark_created(&mut self, name: &str) {
        self.overlay.insert(name.to_owned(), Overlay::Created);
    }

    /// `name` を tombstone にする（remove）。
    pub fn mark_tombstone(&mut self, name: &str) {
        self.overlay.insert(name.to_owned(), Overlay::Tombstone);
    }

    /// created オーバーレイを持つ名前（commit で新規 LFH/CD を出す対象、
    /// および回復後の再 journal 対象）。
    pub fn created_names(&self) -> impl Iterator<Item = &str> {
        self.overlay
            .iter()
            .filter_map(|(k, v)| matches!(v, Overlay::Created).then_some(k.as_str()))
    }

    /// tombstone の名前（commit で出力から外す対象、回復後の再 journal 対象）。
    pub fn tombstones(&self) -> impl Iterator<Item = &str> {
        self.overlay
            .iter()
            .filter_map(|(k, v)| matches!(v, Overlay::Tombstone).then_some(k.as_str()))
    }

    /// 全オーバーレイを捨てる（commit 完了後）。
    pub fn clear(&mut self) {
        self.overlay.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_table_defers_to_vmidx() {
        let t = EntryTable::new();
        assert!(t.is_empty());
        // vmidx に在る名前は Source、無い名前は Absent。
        assert_eq!(t.kind("a", true), Kind::Source);
        assert_eq!(t.kind("ghost", false), Kind::Absent);
    }

    #[test]
    fn created_overrides_absence_and_source() {
        let mut t = EntryTable::new();
        t.mark_created("new");
        assert_eq!(t.kind("new", false), Kind::Created);
        // 同名が vmidx にあっても created（リスタート）として扱う。
        t.mark_created("existing");
        assert_eq!(t.kind("existing", true), Kind::Created);
        let names: Vec<&str> = {
            let mut v: Vec<&str> = t.created_names().collect();
            v.sort_unstable();
            v
        };
        assert_eq!(names, vec!["existing", "new"]);
    }

    #[test]
    fn tombstone_hides_source() {
        let mut t = EntryTable::new();
        t.mark_tombstone("gone");
        assert_eq!(t.kind("gone", true), Kind::Absent);
        let toms: Vec<&str> = t.tombstones().collect();
        assert_eq!(toms, vec!["gone"]);
    }

    #[test]
    fn marks_override_each_other() {
        let mut t = EntryTable::new();
        t.mark_tombstone("x");
        assert_eq!(t.kind("x", true), Kind::Absent);
        // remove 後に create でリスタート。
        t.mark_created("x");
        assert_eq!(t.kind("x", true), Kind::Created);
        assert!(!t.is_empty());
        t.clear();
        assert!(t.is_empty());
        assert_eq!(t.kind("x", true), Kind::Source);
    }
}
