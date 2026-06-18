//! Diff Layer Tier 1（設計 LAYER 2 / WRITE PATH のホットな dirty ページ store）。
//!
//! 未コミットの書き込みを保持する唯一のメモリ内コピー。エントリごとに
//! 「dirty ページ表」と論理サイズ（`logical_size`）を持つ。設計の 3 段
//! （Tier 1 → Tier 2 spill → vmdirty journal）のうち、ここでは M2 の最小形
//! ＝ Tier 1 のみ（`dirty_limit = UNLIMITED`、spill なし）を実装する。耐久性
//! （Tier 2 / journal / 回復）は M3 以降。
//!
//! ページは常に `page_size` バイトの完全バッファで保持する（末尾の論理ページが
//! 短くても、超過分はゼロ埋めしておき、実際の長さは `logical_size` で決める）。
//! これにより「書き込みで伸びたエントリの末尾」や「飛び地書き込みの間のゼロ
//! 埋め」を、読み取り・commit の双方が `logical_size` のクランプだけで一貫して
//! 扱える（設計 WRITE PATH の implicit extension / zero-fill gaps）。
//!
//! このモジュールは I/O も圧縮も持たない純データ構造。COW の元ページ取得
//! （キャッシュ/ソース ZIP からの読み出し）と commit の再圧縮は呼び出し側
//! （[`mount`](crate::mount) / [`commit`](crate::commit)）の責務。

use std::collections::HashMap;

/// 1 エントリ分の dirty 状態。
struct DirtyEntry {
    /// ページ番号 → ページバイト列（長さは常に `page_size`）。
    pages: HashMap<u64, Vec<u8>>,
    /// 現在の論理サイズ。read は超過分を短く返し（EOF）、commit はここまでを
    /// materialise する。
    logical_size: u64,
}

/// Diff Layer Tier 1。エントリ名（`path`）で dirty 状態を引く。
pub struct DiffLayer {
    page_size: u64,
    entries: HashMap<String, DirtyEntry>,
}

impl DiffLayer {
    /// 空の Diff Layer を作る。`page_size` はマウントのページ設定に一致させる。
    pub fn new(page_size: u64) -> DiffLayer {
        DiffLayer {
            page_size: page_size.max(1),
            entries: HashMap::new(),
        }
    }

    /// ページサイズ。
    pub fn page_size(&self) -> u64 {
        self.page_size
    }

    /// dirty なエントリが 1 つも無いか（CLEAN なら commit は no-op にできる）。
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// `path` が dirty か（1 度でも write された＝Diff Layer に載っているか）。
    pub fn is_dirty(&self, path: &str) -> bool {
        self.entries.contains_key(path)
    }

    /// dirty なエントリ名を列挙する。
    pub fn dirty_paths(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }

    /// `path` の論理サイズ（dirty でなければ `None`）。
    pub fn logical_size(&self, path: &str) -> Option<u64> {
        self.entries.get(path).map(|e| e.logical_size)
    }

    /// まだ dirty でなければ、論理サイズを `base_size`（ソースの
    /// `uncompressed_size`）で初期化してエントリを作る。既に dirty なら何もしない。
    pub fn ensure_entry(&mut self, path: &str, base_size: u64) {
        self.entries
            .entry(path.to_owned())
            .or_insert_with(|| DirtyEntry {
                pages: HashMap::new(),
                logical_size: base_size,
            });
    }

    /// 論理サイズを設定する（implicit extension / truncate で更新）。
    /// エントリが未作成なら何もしない（先に [`ensure_entry`](Self::ensure_entry)）。
    pub fn set_logical_size(&mut self, path: &str, size: u64) {
        if let Some(e) = self.entries.get_mut(path) {
            e.logical_size = size;
        }
    }

    /// ページ `page` が Diff Layer に載っているか。
    pub fn has_page(&self, path: &str, page: u64) -> bool {
        self.entries
            .get(path)
            .is_some_and(|e| e.pages.contains_key(&page))
    }

    /// ページ `page` のバイト列（`page_size` バイト）。無ければ `None`。
    pub fn page(&self, path: &str, page: u64) -> Option<&[u8]> {
        self.entries
            .get(path)
            .and_then(|e| e.pages.get(&page))
            .map(Vec::as_slice)
    }

    /// ページ `page` を挿入/置換する。`bytes` は `page_size` バイトであること。
    /// エントリが未作成なら何もしない（先に [`ensure_entry`](Self::ensure_entry)）。
    pub fn insert_page(&mut self, path: &str, page: u64, bytes: Vec<u8>) {
        if let Some(e) = self.entries.get_mut(path) {
            e.pages.insert(page, bytes);
        }
    }

    /// ページ `page` への可変参照（in-place 書き込み用）。
    pub fn page_mut(&mut self, path: &str, page: u64) -> Option<&mut Vec<u8>> {
        self.entries.get_mut(path).and_then(|e| e.pages.get_mut(&page))
    }

    /// 全 dirty 状態を捨てる（commit 完了後）。
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_layer_has_no_dirty_entries() {
        let d = DiffLayer::new(4096);
        assert!(d.is_empty());
        assert!(!d.is_dirty("a"));
        assert_eq!(d.logical_size("a"), None);
        assert!(!d.has_page("a", 0));
    }

    #[test]
    fn ensure_entry_initializes_size_once() {
        let mut d = DiffLayer::new(16);
        d.ensure_entry("a", 100);
        assert!(d.is_dirty("a"));
        assert_eq!(d.logical_size("a"), Some(100));
        // 2 度目は初期化しない（既存の論理サイズを保つ）。
        d.set_logical_size("a", 120);
        d.ensure_entry("a", 100);
        assert_eq!(d.logical_size("a"), Some(120));
    }

    #[test]
    fn pages_round_trip_and_mutate() {
        let mut d = DiffLayer::new(8);
        d.ensure_entry("a", 0);
        assert!(!d.has_page("a", 0));
        d.insert_page("a", 0, vec![0u8; 8]);
        assert!(d.has_page("a", 0));
        d.page_mut("a", 0).unwrap()[3] = 0x42;
        assert_eq!(d.page("a", 0).unwrap()[3], 0x42);
    }

    #[test]
    fn clear_drops_everything() {
        let mut d = DiffLayer::new(8);
        d.ensure_entry("a", 0);
        d.insert_page("a", 0, vec![0u8; 8]);
        d.clear();
        assert!(d.is_empty());
        assert!(!d.is_dirty("a"));
    }
}
