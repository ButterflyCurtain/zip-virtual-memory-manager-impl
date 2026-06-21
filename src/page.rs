//! ページキャッシュ（設計 LAYER 2a / READ PATH）。
//!
//! 展開後ストリームを固定長ページ（既定 4 KB、マウント時設定可）に区切り、LRU で
//! バウンドしたキャッシュに保持する。読み取り経路（[`mount`](crate::mount)）は
//! キャッシュミス時にだけプロバイダで展開し、目標ページとその先 N ページ
//! （read-ahead）を本キャッシュに充填する。Diff Layer (`difflayer`) が前段に
//! 積まれており、現状の読み取り経路は「Tier 1 → Tier 2 (vmdirty) → ページ
//! キャッシュ → シーク索引 + ソース ZIP」の三段+索引解決。
//!
//! このモジュールは I/O も解凍も持たない純粋なデータ構造で、キーは
//! [`PageKey`]（エントリ表インデックス + ページ番号）、値は 1 ページ分の展開
//! バイト列。末尾ページは短い（`uncompressed_size` がページ境界に揃わない）。
//! その「エントリ e のページ i の範囲」を [`page_extent`] / [`page_count`] に
//! 集約し、read / fill が同じ計算を共有する（IMPLEMENTATION_NOTES の罠を 1 箇所に）。
//!
//! LRU は内製のスラブ + 侵入型双方向リスト（外部クレート不使用）。バイト量で
//! バウンドし、超過時は末尾（最も古い）から退避する。dirty 追跡 (COW) は
//! `difflayer` 側が担当し、本キャッシュは clean ページ専用。`madvise` 系の
//! ヒントは現状未配線（mmap 経由の ADVICE は `disk` 層の TODO）。

use std::collections::HashMap;

/// 既定ページサイズ（バイト）。設計 LAYER 1 の既定値。
pub const DEFAULT_PAGE_SIZE: u64 = 4096;
/// 既定 read-ahead ページ数。設計 READ PATH の `read_ahead_pages` 既定。
pub const DEFAULT_READ_AHEAD_PAGES: u32 = 8;
/// 既定キャッシュ上限（バイト）。設計は「max size = configurable」で既定値を
/// 規定しないため、控えめに 16 MiB を採る。
pub const DEFAULT_CACHE_BYTES: usize = 16 << 20;

/// ページキャッシュ / 読み取り経路の設定（マウント時に決める）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageConfig {
    /// 1 ページのバイト数（`>= 1`。0 は [`PageCache::new`] が 1 に丸める）。
    pub page_size: u64,
    /// キャッシュミス時に目標ページの先へ先読みするページ数（0 = 無効）。
    pub read_ahead_pages: u32,
    /// キャッシュのバイト上限（`page_size` 未満なら 1 ページに丸める）。
    pub cache_bytes: usize,
}

impl Default for PageConfig {
    fn default() -> PageConfig {
        PageConfig {
            page_size: DEFAULT_PAGE_SIZE,
            read_ahead_pages: DEFAULT_READ_AHEAD_PAGES,
            cache_bytes: DEFAULT_CACHE_BYTES,
        }
    }
}

/// 1 ページを一意に識別するキー。`entry` は vmidx エントリ表のインデックス
/// （`Vmidx::lookup` が返す `usize`。同一 vmidx 像の中で安定）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PageKey {
    /// vmidx エントリ表インデックス。
    pub entry: usize,
    /// エントリ内のページ番号（0 始まり）。
    pub page: u64,
}

/// エントリのページ総数（`ceil(uncompressed_size / page_size)`）。
/// サイズ 0 のエントリは 0 ページ。
pub fn page_count(uncompressed_size: u64, page_size: u64) -> u64 {
    let ps = page_size.max(1);
    uncompressed_size.div_ceil(ps)
}

/// エントリのページ `page_index` の (展開ストリーム内開始オフセット, 長さ)。
/// 末尾ページは短く、範囲外ページは長さ 0 を返す。「ページ i の大きさ」を
/// 計算する唯一の場所（read / fill / spill / recompress が共有する想定）。
pub fn page_extent(uncompressed_size: u64, page_index: u64, page_size: u64) -> (u64, usize) {
    let ps = page_size.max(1);
    let start = page_index.saturating_mul(ps);
    if start >= uncompressed_size {
        return (start, 0);
    }
    let len = ps.min(uncompressed_size - start) as usize;
    (start, len)
}

/// 侵入型 LRU リストの番兵（「リンク無し」）。
const NIL: usize = usize::MAX;

/// スラブ内の 1 ノード。`prev`/`next` は [`PageCache::nodes`] のインデックス。
struct Node {
    key: PageKey,
    data: Vec<u8>,
    prev: usize,
    next: usize,
}

/// バイト量でバウンドした LRU ページキャッシュ。
///
/// `map` がキー→スラブインデックス、`nodes` がノード本体、`free` が再利用可能な
/// スラブ枠。`head` が最近使用（MRU）、`tail` が最古（LRU）。退避は `tail` から。
pub struct PageCache {
    page_size: u64,
    max_bytes: usize,
    cur_bytes: usize,
    map: HashMap<PageKey, usize>,
    nodes: Vec<Node>,
    free: Vec<usize>,
    head: usize,
    tail: usize,
    hits: u64,
    misses: u64,
}

impl PageCache {
    /// 上限 `max_bytes`、ページサイズ `page_size` の空キャッシュを作る。
    /// `page_size` は 1 未満なら 1 に、`max_bytes` は 1 ページ未満なら 1 ページ分に
    /// 丸める（1 ページは必ず保持できる＝読み取りが必ず前進する）。
    pub fn new(page_size: u64, max_bytes: usize) -> PageCache {
        let page_size = page_size.max(1);
        let max_bytes = max_bytes.max(page_size as usize);
        PageCache {
            page_size,
            max_bytes,
            cur_bytes: 0,
            map: HashMap::new(),
            nodes: Vec::new(),
            free: Vec::new(),
            head: NIL,
            tail: NIL,
            hits: 0,
            misses: 0,
        }
    }

    /// 設定からキャッシュを作る。
    pub fn from_config(cfg: &PageConfig) -> PageCache {
        PageCache::new(cfg.page_size, cfg.cache_bytes)
    }

    /// 1 ページのバイト数。
    pub fn page_size(&self) -> u64 {
        self.page_size
    }

    /// 現在のキャッシュ済みページ数。
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// キャッシュが空か。
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// 現在のキャッシュ使用バイト量（ページ本体の合計）。
    pub fn byte_len(&self) -> usize {
        self.cur_bytes
    }

    /// 累積ヒット数（観測用）。
    pub fn hits(&self) -> u64 {
        self.hits
    }

    /// 累積ミス数（観測用）。
    pub fn misses(&self) -> u64 {
        self.misses
    }

    /// `key` が常駐するか（LRU 順は変えない。read-ahead の「既に常駐なら飛ばす」
    /// 判定に使う）。ヒット/ミスカウンタも動かさない。
    pub fn contains(&self, key: PageKey) -> bool {
        self.map.contains_key(&key)
    }

    /// `key` のページを引き、MRU に繰り上げる。ヒット/ミスを計上する。
    pub fn get(&mut self, key: PageKey) -> Option<&[u8]> {
        match self.map.get(&key).copied() {
            Some(idx) => {
                self.move_to_front(idx);
                self.hits += 1;
                Some(&self.nodes[idx].data)
            }
            None => {
                self.misses += 1;
                None
            }
        }
    }

    /// `key` に `data` を入れる（既存なら置換）。MRU に繰り上げ、上限超過分を
    /// 末尾から退避する。
    pub fn insert(&mut self, key: PageKey, data: Vec<u8>) {
        if let Some(&idx) = self.map.get(&key) {
            let old = self.nodes[idx].data.len();
            self.cur_bytes = self.cur_bytes - old + data.len();
            self.nodes[idx].data = data;
            self.move_to_front(idx);
        } else {
            let len = data.len();
            let idx = self.alloc_node(key, data);
            self.push_front(idx);
            self.map.insert(key, idx);
            self.cur_bytes += len;
        }
        self.evict_to_fit();
    }

    /// 全ページを捨てる（累積カウンタは残す）。vmidx 再構築時などに使う想定。
    pub fn clear(&mut self) {
        self.map.clear();
        self.nodes.clear();
        self.free.clear();
        self.head = NIL;
        self.tail = NIL;
        self.cur_bytes = 0;
    }

    /// 上限を下回るまで末尾（LRU）から退避する。最後の 1 ページは退避しない
    /// （単一ページが上限超でも保持し、読み取りが前進できるようにする）。
    fn evict_to_fit(&mut self) {
        while self.cur_bytes > self.max_bytes && self.map.len() > 1 {
            self.evict_tail();
        }
    }

    fn evict_tail(&mut self) {
        let idx = self.tail;
        debug_assert_ne!(idx, NIL);
        self.unlink(idx);
        let key = self.nodes[idx].key;
        self.cur_bytes -= self.nodes[idx].data.len();
        self.map.remove(&key);
        self.nodes[idx].data = Vec::new();
        self.free.push(idx);
    }

    fn alloc_node(&mut self, key: PageKey, data: Vec<u8>) -> usize {
        let node = Node {
            key,
            data,
            prev: NIL,
            next: NIL,
        };
        if let Some(idx) = self.free.pop() {
            self.nodes[idx] = node;
            idx
        } else {
            self.nodes.push(node);
            self.nodes.len() - 1
        }
    }

    fn unlink(&mut self, idx: usize) {
        let (p, n) = (self.nodes[idx].prev, self.nodes[idx].next);
        if p != NIL {
            self.nodes[p].next = n;
        } else {
            self.head = n;
        }
        if n != NIL {
            self.nodes[n].prev = p;
        } else {
            self.tail = p;
        }
    }

    fn push_front(&mut self, idx: usize) {
        self.nodes[idx].prev = NIL;
        self.nodes[idx].next = self.head;
        if self.head != NIL {
            self.nodes[self.head].prev = idx;
        } else {
            self.tail = idx;
        }
        self.head = idx;
    }

    fn move_to_front(&mut self, idx: usize) {
        if self.head == idx {
            return;
        }
        self.unlink(idx);
        self.push_front(idx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(entry: usize, page: u64) -> PageKey {
        PageKey { entry, page }
    }

    #[test]
    fn page_geometry_handles_short_last_page() {
        // 4 KB ページ、サイズ 10000 → 3 ページ（4096, 4096, 1808）。
        assert_eq!(page_count(10_000, 4096), 3);
        assert_eq!(page_extent(10_000, 0, 4096), (0, 4096));
        assert_eq!(page_extent(10_000, 1, 4096), (4096, 4096));
        assert_eq!(page_extent(10_000, 2, 4096), (8192, 1808));
        // 範囲外ページは長さ 0。
        assert_eq!(page_extent(10_000, 3, 4096), (12_288, 0));
        // ちょうど境界に揃う場合は末尾ページもフル。
        assert_eq!(page_count(8192, 4096), 2);
        assert_eq!(page_extent(8192, 1, 4096), (4096, 4096));
        // サイズ 0 は 0 ページ。
        assert_eq!(page_count(0, 4096), 0);
    }

    #[test]
    fn insert_get_roundtrip_and_counters() {
        let mut c = PageCache::new(16, 1024);
        assert!(c.is_empty());
        assert_eq!(c.get(key(0, 0)), None); // miss
        c.insert(key(0, 0), vec![1, 2, 3]);
        assert_eq!(c.get(key(0, 0)), Some(&[1, 2, 3][..])); // hit
        assert_eq!(c.len(), 1);
        assert_eq!(c.byte_len(), 3);
        assert_eq!(c.misses(), 1);
        assert_eq!(c.hits(), 1);
    }

    #[test]
    fn insert_replaces_and_tracks_bytes() {
        let mut c = PageCache::new(16, 1024);
        c.insert(key(1, 0), vec![0u8; 10]);
        assert_eq!(c.byte_len(), 10);
        c.insert(key(1, 0), vec![0u8; 4]); // 同じキーを上書き
        assert_eq!(c.len(), 1);
        assert_eq!(c.byte_len(), 4);
        assert_eq!(c.get(key(1, 0)).unwrap().len(), 4);
    }

    #[test]
    fn evicts_least_recently_used() {
        // 1 ページ 4 バイト、上限 12 バイト → 3 ページまで。
        let mut c = PageCache::new(4, 12);
        c.insert(key(0, 0), vec![0u8; 4]);
        c.insert(key(0, 1), vec![0u8; 4]);
        c.insert(key(0, 2), vec![0u8; 4]);
        assert_eq!(c.len(), 3);
        // page0 を触って MRU に上げる → 次の退避対象は page1。
        assert!(c.get(key(0, 0)).is_some());
        c.insert(key(0, 3), vec![0u8; 4]); // 上限超 → LRU(page1) を退避
        assert_eq!(c.len(), 3);
        assert!(c.contains(key(0, 0)));
        assert!(!c.contains(key(0, 1)), "page1 should have been evicted");
        assert!(c.contains(key(0, 2)));
        assert!(c.contains(key(0, 3)));
        assert_eq!(c.byte_len(), 12);
    }

    #[test]
    fn keeps_single_page_even_if_oversized() {
        // 上限 < 1 ページ → new が 1 ページ分に丸める。単一ページは退避しない。
        let mut c = PageCache::new(100, 1);
        assert_eq!(c.byte_len(), 0);
        c.insert(key(0, 0), vec![0u8; 100]);
        assert_eq!(c.len(), 1);
        assert!(c.contains(key(0, 0)));
        // 2 つ目を入れると 1 つ目は退避される。
        c.insert(key(0, 1), vec![0u8; 100]);
        assert_eq!(c.len(), 1);
        assert!(c.contains(key(0, 1)));
        assert!(!c.contains(key(0, 0)));
    }

    #[test]
    fn evicted_slots_are_reused() {
        let mut c = PageCache::new(4, 8); // 2 ページ
        for p in 0..10u64 {
            c.insert(key(0, p), vec![p as u8; 4]);
        }
        assert_eq!(c.len(), 2);
        // スラブは退避枠を再利用するので青天井に増えない。insert は「追加 → 退避」
        // 順なので一時的に常駐数 +1 まで増えてから安定する（capacity+1 が上界）。
        assert!(
            c.nodes.len() <= c.len() + 1,
            "slab grew to {} for {} resident pages",
            c.nodes.len(),
            c.len()
        );
        // 最後の 2 ページが残る。
        assert!(c.contains(key(0, 8)));
        assert!(c.contains(key(0, 9)));
    }

    #[test]
    fn clear_empties_storage() {
        let mut c = PageCache::new(4, 64);
        c.insert(key(0, 0), vec![0u8; 4]);
        c.insert(key(0, 1), vec![0u8; 4]);
        c.clear();
        assert!(c.is_empty());
        assert_eq!(c.byte_len(), 0);
        assert!(!c.contains(key(0, 0)));
    }
}
