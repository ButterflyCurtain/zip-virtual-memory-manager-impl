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
//!
//! **Tier 2 spill のポリシー核（M3）**: `dirty_limit`（Tier 1 に保持してよい
//! dirty バイト上限）と FIFO victim 選択（設計 Section 5.1）を持つ。上限超過時に
//! 最古から退避すべきページ（[`SpilledPage`]）を [`take_spill_victims`] が返す。
//! ここは「どのページを退かすか」の決定だけで、退避先 vmdirty への **書き出しは
//! 行わない**（I/O は呼び出し側 = `VmdirtyWriter` 委譲）。既定は無制限
//! （[`UNLIMITED`]、M2 互換: spill なし）。
//!
//! FIFO は挿入順（＝最古に*書かれた*ページから退避、設計 5.1 既定）。write hit に
//! よる「最新へ再スタンプ」（write amplification 軽減、5.1 既知ケース）は正しさに
//! 影響しない最適化なので、spill を実 I/O に繋ぐ増分へ回す。
//!
//! [`take_spill_victims`]: DiffLayer::take_spill_victims

use std::collections::{BTreeMap, HashMap};

/// `dirty_limit` の無制限値（spill を起こさない。M2 既定）。
pub const UNLIMITED: u64 = u64::MAX;

/// 1 エントリ分の dirty 状態。
struct DirtyEntry {
    /// ページ番号 → ページバイト列（長さは常に `page_size`）。
    pages: HashMap<u64, Vec<u8>>,
    /// 現在の論理サイズ。read は超過分を短く返し（EOF）、commit はここまでを
    /// materialise する。
    logical_size: u64,
    /// 未変更ページをソースから読んでよい先頭バイト数（high-water）。初期値は
    /// ソースの元 `uncompressed_size`。truncate-shrink で単調に減る（縮小で捨てた
    /// 領域は、後で extend しても蘇らずゼロになる）。created は 0。
    source_size: u64,
}

/// `dirty_limit` 超過で Tier 1 から退避すべき 1 ページ。呼び出し側がこれを
/// `VmdirtyWriter::append_data_record` で vmdirty へ書き、(entry,page)→offset を
/// Tier 2 索引に記録する（設計 Section 4.1）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpilledPage {
    pub entry_name: String,
    pub page_index: u64,
    /// 退避するページの完全バイト列（`page_size` バイト）。
    pub data: Vec<u8>,
}

/// Diff Layer Tier 1。エントリ名（`path`）で dirty 状態を引く。
pub struct DiffLayer {
    page_size: u64,
    /// Tier 1 に保持してよい dirty バイト上限。超過分は spill 候補になる。
    dirty_limit: u64,
    /// 現在 Tier 1 が保持する dirty バイト数（= 常駐ページ数 × `page_size`）。
    dirty_current: u64,
    /// 次に割り当てる FIFO スタンプ（単調増加）。
    next_stamp: u64,
    /// FIFO 並び: スタンプ → (エントリ名, ページ番号)。最小キー = 最古。
    order: BTreeMap<u64, (String, u64)>,
    entries: HashMap<String, DirtyEntry>,
}

impl DiffLayer {
    /// 空の Diff Layer を作る。`page_size` はマウントのページ設定に一致させる。
    /// `dirty_limit` は無制限（spill なし、M2 互換）。
    pub fn new(page_size: u64) -> DiffLayer {
        DiffLayer::with_dirty_limit(page_size, UNLIMITED)
    }

    /// `dirty_limit`（Tier 1 の dirty バイト上限）を指定して作る。`0` は
    /// SPILL_ONLY（書いたページを即 spill 候補にする）。
    pub fn with_dirty_limit(page_size: u64, dirty_limit: u64) -> DiffLayer {
        DiffLayer {
            page_size: page_size.max(1),
            dirty_limit,
            dirty_current: 0,
            next_stamp: 0,
            order: BTreeMap::new(),
            entries: HashMap::new(),
        }
    }

    /// ページサイズ。
    pub fn page_size(&self) -> u64 {
        self.page_size
    }

    /// Tier 1 の dirty バイト上限。
    pub fn dirty_limit(&self) -> u64 {
        self.dirty_limit
    }

    /// 現在 Tier 1 が保持している dirty バイト数。
    pub fn dirty_bytes(&self) -> u64 {
        self.dirty_current
    }

    /// dirty バイトが上限を超えているか（spill が要るか）。
    pub fn over_limit(&self) -> bool {
        self.dirty_current > self.dirty_limit
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
                source_size: base_size,
            });
    }

    /// dirty 状態のキーを `old` から `new` へ付け替える（rename）。`old` に
    /// dirty エントリが無ければ何もしない（純粋な rename で 1 度も書いていない
    /// 場合＝未変更ページはソースから引くので Diff には何も無い）。ページ・論理
    /// サイズ・source_size・FIFO スタンプはそのまま引き継ぐ（会計 `dirty_current`
    /// は不変）。`new` 側の既存エントリは呼び出し側が存在チェック（EEXIST）で
    /// 排除している前提で上書きする。
    pub fn rename_entry(&mut self, old: &str, new: &str) {
        if old == new {
            return;
        }
        if let Some(entry) = self.entries.remove(old) {
            self.entries.insert(new.to_owned(), entry);
            // FIFO 並びの (エントリ名, ページ) も付け替える（スタンプは保持）。
            for (name, _) in self.order.values_mut() {
                if name == old {
                    *name = new.to_owned();
                }
            }
        }
    }

    /// `path` の source_size（未変更ページをソースから読んでよい先頭バイト数）。
    /// dirty でなければ `None`。
    pub fn source_size(&self, path: &str) -> Option<u64> {
        self.entries.get(path).map(|e| e.source_size)
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
    /// 新規ページは FIFO 末尾（最新）に並び、`dirty_bytes` を `page_size` 増やす。
    /// 既存ページの置換はデータだけ差し替え、並び順・会計は変えない。
    pub fn insert_page(&mut self, path: &str, page: u64, bytes: Vec<u8>) {
        let stamp = self.next_stamp;
        let Some(e) = self.entries.get_mut(path) else {
            return;
        };
        match e.pages.get_mut(&page) {
            Some(existing) => *existing = bytes,
            None => {
                e.pages.insert(page, bytes);
                self.order.insert(stamp, (path.to_owned(), page));
                self.next_stamp += 1;
                self.dirty_current += self.page_size;
            }
        }
    }

    /// ページ `page` への可変参照（in-place 書き込み用）。
    pub fn page_mut(&mut self, path: &str, page: u64) -> Option<&mut Vec<u8>> {
        self.entries.get_mut(path).and_then(|e| e.pages.get_mut(&page))
    }

    /// `dirty_limit` を超えている間、最古（FIFO 先頭）のページを Tier 1 から外し、
    /// 退避すべきページを古い順に返す（設計 Section 5.1 のスピル選択）。返したページは
    /// もう Tier 1 に無い（呼び出し側が vmdirty へ書く責務）。ページが全部抜けても
    /// エントリ自体（`logical_size`）は残す。無制限なら常に空を返す。
    pub fn take_spill_victims(&mut self) -> Vec<SpilledPage> {
        let mut victims = Vec::new();
        while self.dirty_current > self.dirty_limit {
            let Some((_, (entry_name, page_index))) = self.order.pop_first() else {
                break; // Tier 1 が空（dirty_limit < page_size でもここで止まる）
            };
            if let Some(e) = self.entries.get_mut(&entry_name)
                && let Some(data) = e.pages.remove(&page_index)
            {
                self.dirty_current -= self.page_size;
                victims.push(SpilledPage {
                    entry_name,
                    page_index,
                    data,
                });
            }
        }
        victims
    }

    /// Tier 1 に常駐している `(entry_name, page_index)` を FIFO（挿入順＝最古から）
    /// で列挙する。flush（全 spill）が durable 化の順序として使う。
    pub fn resident_pages(&self) -> Vec<(String, u64)> {
        self.order.values().cloned().collect()
    }

    /// エントリ `path` を丸ごと（全ページ + 論理サイズ）落とす（remove / create
    /// リスタート用）。FIFO 並びと会計からも当該ページを取り除く。エントリが
    /// 無ければ何もしない。
    pub fn remove_entry(&mut self, path: &str) {
        if let Some(e) = self.entries.remove(path) {
            let dropped = e.pages.len() as u64;
            self.dirty_current = self
                .dirty_current
                .saturating_sub(dropped * self.page_size);
            self.order.retain(|_, (p, _)| p != path);
        }
    }

    /// `path` を `new_size` へ縮める（truncate-shrink）。`new_size` 以降に完全に
    /// 収まるページを Tier 1 から落とし、境界ページの末尾（`new_size % page_size`
    /// 以降）をゼロ埋めする（設計 truncate: "final partial page is zero-padded in
    /// its tail at commit"。再 extend 時に古い末尾が蘇らないよう即ゼロ化する）。
    /// FIFO 並びと会計を更新する。エントリが無ければ何もしない。論理サイズ自体は
    /// 呼び出し側が [`set_logical_size`](Self::set_logical_size) で更新する。
    pub fn truncate_pages(&mut self, path: &str, new_size: u64) {
        let ps = self.page_size;
        let Some(e) = self.entries.get_mut(path) else {
            return;
        };
        // 捨てた末尾はソースからも蘇らせない（source high-water を縮める）。
        e.source_size = e.source_size.min(new_size);
        let before = e.pages.len() as u64;
        e.pages.retain(|&page, _| page * ps < new_size);
        let dropped = before - e.pages.len() as u64;
        // 境界ページの末尾をゼロ化（new_size がページ境界でない場合のみ）。
        let tail = (new_size % ps) as usize;
        if tail != 0
            && let Some(buf) = e.pages.get_mut(&(new_size / ps))
        {
            for b in &mut buf[tail..] {
                *b = 0;
            }
        }
        self.dirty_current = self.dirty_current.saturating_sub(dropped * ps);
        self.order.retain(|_, (p, pg)| !(p == path && *pg * ps >= new_size));
    }

    /// 全 dirty 状態を捨てる（commit 完了後）。
    pub fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
        self.dirty_current = 0;
        self.next_stamp = 0;
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
    fn rename_entry_moves_state_and_keeps_accounting() {
        let mut d = DiffLayer::new(8);
        d.ensure_entry("a", 16);
        d.insert_page("a", 0, vec![0xAB; 8]);
        d.set_logical_size("a", 20);
        let before = d.dirty_bytes();
        d.rename_entry("a", "b");
        // old は消え、new が状態を引き継ぐ。
        assert!(!d.is_dirty("a"));
        assert!(d.is_dirty("b"));
        assert_eq!(d.logical_size("b"), Some(20));
        assert_eq!(d.source_size("b"), Some(16));
        assert_eq!(d.page("b", 0).unwrap()[0], 0xAB);
        assert_eq!(d.dirty_bytes(), before); // 会計は不変
    }

    #[test]
    fn rename_entry_noop_when_source_clean() {
        let mut d = DiffLayer::new(8);
        // 1 度も書いていない名前の rename は Diff に何も作らない。
        d.rename_entry("a", "b");
        assert!(!d.is_dirty("a"));
        assert!(!d.is_dirty("b"));
    }

    #[test]
    fn rename_entry_preserves_fifo_victim_order() {
        // a の古いページ → b へ rename しても FIFO 最古のまま退避される。
        let mut d = DiffLayer::with_dirty_limit(8, 8);
        put(&mut d, "a", 0); // 最古
        put(&mut d, "c", 0); // 新しい
        d.rename_entry("a", "b");
        let victims = d.take_spill_victims();
        assert_eq!(victims.len(), 1);
        assert_eq!(victims[0].entry_name, "b"); // 付け替え後の名前で最古
        assert_eq!(victims[0].page_index, 0);
    }

    #[test]
    fn clear_drops_everything() {
        let mut d = DiffLayer::new(8);
        d.ensure_entry("a", 0);
        d.insert_page("a", 0, vec![0u8; 8]);
        d.clear();
        assert!(d.is_empty());
        assert!(!d.is_dirty("a"));
        assert_eq!(d.dirty_bytes(), 0);
    }

    /// `path` の `page` を 1 枚挿入するヘルパ（中身は識別用にページ番号で埋める）。
    fn put(d: &mut DiffLayer, path: &str, page: u64) {
        d.ensure_entry(path, 0);
        d.insert_page(path, page, vec![page as u8; d.page_size() as usize]);
    }

    #[test]
    fn unlimited_never_spills() {
        let mut d = DiffLayer::new(8); // 既定 = 無制限
        assert_eq!(d.dirty_limit(), UNLIMITED);
        for p in 0..100 {
            put(&mut d, "a", p);
        }
        assert_eq!(d.dirty_bytes(), 100 * 8);
        assert!(!d.over_limit());
        assert!(d.take_spill_victims().is_empty());
        assert_eq!(d.dirty_bytes(), 100 * 8);
    }

    #[test]
    fn accounting_tracks_resident_bytes() {
        let mut d = DiffLayer::with_dirty_limit(8, UNLIMITED);
        put(&mut d, "a", 0);
        put(&mut d, "a", 1);
        assert_eq!(d.dirty_bytes(), 16);
        // 既存ページの置換は会計を増やさない。
        d.insert_page("a", 0, vec![9u8; 8]);
        assert_eq!(d.dirty_bytes(), 16);
        assert_eq!(d.page("a", 0).unwrap()[0], 9);
    }

    #[test]
    fn fifo_evicts_oldest_first_across_entries() {
        // 上限 = 3 ページ。5 ページ入れたら最古 2 枚が退避される。
        let mut d = DiffLayer::with_dirty_limit(8, 3 * 8);
        put(&mut d, "a", 0); // 最古
        put(&mut d, "b", 7);
        put(&mut d, "a", 1);
        put(&mut d, "c", 0);
        put(&mut d, "b", 8); // 最新
        assert!(d.over_limit());

        let victims = d.take_spill_victims();
        // 古い順 = 挿入順に 2 枚。
        assert_eq!(victims.len(), 2);
        assert_eq!(
            (victims[0].entry_name.as_str(), victims[0].page_index),
            ("a", 0)
        );
        assert_eq!(
            (victims[1].entry_name.as_str(), victims[1].page_index),
            ("b", 7)
        );
        // 退避ページのデータも持って出る。
        assert_eq!(victims[0].data, vec![0u8; 8]);
        assert_eq!(victims[1].data, vec![7u8; 8]);

        // Tier 1 は上限ちょうどに収まり、退避ページはもう無い。
        assert_eq!(d.dirty_bytes(), 3 * 8);
        assert!(!d.over_limit());
        assert!(!d.has_page("a", 0));
        assert!(!d.has_page("b", 7));
        assert!(d.has_page("a", 1));
        assert!(d.has_page("c", 0));
        assert!(d.has_page("b", 8));
        // 2 回目の呼び出しは何も退避しない。
        assert!(d.take_spill_victims().is_empty());
    }

    #[test]
    fn spilling_all_pages_keeps_entry_and_logical_size() {
        // SPILL_ONLY（上限 0）: 入れたページは即 spill 候補。
        let mut d = DiffLayer::with_dirty_limit(8, 0);
        d.ensure_entry("a", 100); // 論理サイズ 100
        d.insert_page("a", 0, vec![1u8; 8]);
        d.insert_page("a", 1, vec![2u8; 8]);
        let victims = d.take_spill_victims();
        assert_eq!(victims.len(), 2);
        assert_eq!(d.dirty_bytes(), 0);
        // ページは全部抜けたが、エントリと論理サイズは残る（commit/read が要る）。
        assert!(d.is_dirty("a"));
        assert_eq!(d.logical_size("a"), Some(100));
        assert!(!d.has_page("a", 0));
        assert!(!d.has_page("a", 1));
    }

    #[test]
    fn clear_resets_spill_accounting() {
        let mut d = DiffLayer::with_dirty_limit(8, 0);
        put(&mut d, "a", 0);
        d.clear();
        assert_eq!(d.dirty_bytes(), 0);
        assert!(!d.over_limit());
        // クリア後に入れ直しても FIFO/会計が壊れていない。
        put(&mut d, "b", 0);
        assert_eq!(d.dirty_bytes(), 8);
        let victims = d.take_spill_victims();
        assert_eq!(victims.len(), 1);
        assert_eq!(victims[0].entry_name, "b");
    }
}
