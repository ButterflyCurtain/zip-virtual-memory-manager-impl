//! In-memory エントリ表（設計 ENTRY OPERATIONS）。
//!
//! immutable な vmidx の上に被せ、セッション内の **構造変更**（create / remove /
//! rename）を表現するオーバーレイ。実効的なエントリ集合は
//! 「vmidx ∪ created − tombstone（+ rename）」で、[`kind`](EntryTable::kind) が
//! 1 名前についてそれを判定する。
//!
//! このモジュールは存在と「未変更データの出どころ」だけを持つ純データ構造。
//! 論理サイズ（`logical_size`）と dirty ページは [`DiffLayer`](crate::difflayer)
//! 側に残し、I/O・journaling は呼び出し側（[`mount`](crate::mount) /
//! [`disk`](crate::disk)）の責務。truncate は構造を変えない（論理サイズの変更 +
//! ページ整理）ので、ここには記録しない。
//!
//! **rename（④b）**: 現在名を別のソース名へ写像する `Aliased { source }` で
//! 表現する。`rename(old, new)` は `new → Aliased { source }`（`source` は old の
//! 究極のソース名＝old がプレーンなら old 自身、old が既に別名なら連鎖を畳んだ
//! 元のソース名）と `old → Tombstone` を立てる。[`kind`](EntryTable::kind) は
//! Aliased を [`Kind::Source`] として返しつつ、未変更ページの読み出しは
//! `source` 名で行う（[`aliased_source`](EntryTable::aliased_source)）。
//! `source` は常に **immutable な archive 内の名前**を指す（セッション内の
//! create/remove には影響されない）。

use std::collections::HashMap;

/// 1 名前に対するオーバーレイ判断。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Overlay {
    /// セッション内で create された（ソース無し）。同名が vmidx にあっても
    /// 「新規の空エントリ」として扱う（create-after-remove のリスタート含む）。
    Created,
    /// remove された（tombstone）。read/write は ENOENT、commit は出力しない。
    Tombstone,
    /// rename の結果、この現在名は別のソース名（archive 内の名前）からデータを
    /// 引く（設計 rename()）。kind() は Source を返しつつ、未変更ページの読み出しと
    /// commit の verbatim コピーは `source` で行う。
    Aliased { source: String },
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
            // 別名はソース由来として扱う（ただし読み出しは `aliased_source` の名前）。
            Some(Overlay::Aliased { .. }) => Kind::Source,
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

    /// `name` を `source`（archive 内のソース名）への別名にする（rename ターゲット）。
    pub fn mark_aliased(&mut self, name: &str, source: &str) {
        self.overlay.insert(
            name.to_owned(),
            Overlay::Aliased {
                source: source.to_owned(),
            },
        );
    }

    /// `name` が別名なら、その究極のソース名（archive 内の名前）。プレーンな
    /// ソース・created・tombstone・未知なら `None`。
    pub fn aliased_source(&self, name: &str) -> Option<&str> {
        match self.overlay.get(name) {
            Some(Overlay::Aliased { source }) => Some(source.as_str()),
            _ => None,
        }
    }

    /// `name` が別名（rename ターゲット）か。
    pub fn is_aliased(&self, name: &str) -> bool {
        matches!(self.overlay.get(name), Some(Overlay::Aliased { .. }))
    }

    /// 別名エントリ（現在名, ソース名）の列。commit で現在名へ出力しソースから
    /// データを引く対象、および回復後の再 journal（RENAME）対象。
    pub fn aliases(&self) -> impl Iterator<Item = (&str, &str)> {
        self.overlay.iter().filter_map(|(k, v)| match v {
            Overlay::Aliased { source } => Some((k.as_str(), source.as_str())),
            _ => None,
        })
    }

    /// rename(old → new) のオーバーレイ遷移（存在チェックは呼び出し側）。new は
    /// old のデータ同一性を継承する: created なら created、別名/プレーンなら究極の
    /// ソース名への別名。old は tombstone にする。`old_in_vmidx` は old 名の
    /// ソースエントリが vmidx に在るか。回復 replay と live の両方から使う。
    pub fn apply_rename(&mut self, old: &str, new: &str, old_in_vmidx: bool) {
        let new_overlay = match self.overlay.get(old) {
            // created の rename → 連鎖して created（ソース無し）。
            Some(Overlay::Created) => Overlay::Created,
            // 既に別名 → 究極のソース名を畳んで引き継ぐ（連鎖 rename）。
            Some(Overlay::Aliased { source }) => Overlay::Aliased {
                source: source.clone(),
            },
            // tombstone は本来到達しない（呼び出し側が ENOENT 弾き）。防御的に
            // None と同じ扱い。
            Some(Overlay::Tombstone) | None => {
                if old_in_vmidx {
                    Overlay::Aliased {
                        source: old.to_owned(),
                    }
                } else {
                    // ソースも無い → created 相当（空）。
                    Overlay::Created
                }
            }
        };
        self.overlay.insert(new.to_owned(), new_overlay);
        self.overlay.insert(old.to_owned(), Overlay::Tombstone);
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
    fn rename_plain_source_creates_alias_and_tombstone() {
        let mut t = EntryTable::new();
        // a は vmidx 由来のプレーンなソース。
        t.apply_rename("a", "b", true);
        assert_eq!(t.kind("a", true), Kind::Absent); // old は消える
        assert_eq!(t.kind("b", false), Kind::Source); // new はソース扱い
        assert_eq!(t.aliased_source("b"), Some("a")); // ただしソースは a
        assert!(t.is_aliased("b"));
        let al: Vec<(&str, &str)> = t.aliases().collect();
        assert_eq!(al, vec![("b", "a")]);
    }

    #[test]
    fn rename_chain_folds_to_original_source() {
        let mut t = EntryTable::new();
        t.apply_rename("a", "b", true); // b -> alias(a)
        t.apply_rename("b", "c", false); // b は vmidx に無いが既に別名
        assert_eq!(t.aliased_source("c"), Some("a")); // 連鎖を畳む
        assert_eq!(t.kind("b", false), Kind::Absent);
        assert_eq!(t.kind("c", false), Kind::Source);
    }

    #[test]
    fn rename_created_stays_created() {
        let mut t = EntryTable::new();
        t.mark_created("x");
        t.apply_rename("x", "y", false);
        assert_eq!(t.kind("x", false), Kind::Absent);
        assert_eq!(t.kind("y", false), Kind::Created);
        assert!(!t.is_aliased("y"));
    }

    #[test]
    fn rename_onto_removed_name_reuses_target() {
        let mut t = EntryTable::new();
        // b を消してから a を b へ rename（ターゲット名の再利用）。
        t.mark_tombstone("b");
        t.apply_rename("a", "b", true);
        assert_eq!(t.aliased_source("b"), Some("a"));
        assert_eq!(t.kind("b", true), Kind::Source);
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
