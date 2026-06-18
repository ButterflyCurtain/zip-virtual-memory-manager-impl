//! NAME HEAP とエントリテーブルの組み立て・ルックアップ。
//!
//! エントリテーブルは `name_hash` 昇順（同一ハッシュ内は名前バイト昇順）に
//! 並ぶ。NAME HEAP はその順序で名前 UTF-8 を区切りなく連結したもの（NUL なし、
//! 長さはレコードに持つ）。ルックアップは `name_hash` を二分探索し、同一
//! ハッシュの連なりを名前一致で走査する（仕様 Section 3）。

use super::EntryRecord;

/// XXH3-64 でエントリ名をハッシュする（`name_hash` の値）。
pub fn hash_name(name: &str) -> u64 {
    xxhash_rust::xxh3::xxh3_64(name.as_bytes())
}

/// エントリテーブルと NAME HEAP を組み立てるビルダ。
#[derive(Default)]
pub struct EntryIndexBuilder {
    items: Vec<(String, EntryRecord)>,
}

impl EntryIndexBuilder {
    pub fn new() -> EntryIndexBuilder {
        EntryIndexBuilder::default()
    }

    /// エントリを追加する。`record` の `name_hash` / `name_offset` /
    /// `name_len` は無視され、`build()` 時に `name` から計算・設定される。
    pub fn push(&mut self, name: impl Into<String>, record: EntryRecord) {
        self.items.push((name.into(), record));
    }

    /// テーブル（`name_hash` 昇順・同一ハッシュ内は名前バイト昇順）と
    /// NAME HEAP を組み立てる。
    pub fn build(mut self) -> EntryIndex {
        for (name, rec) in &mut self.items {
            rec.name_hash = hash_name(name);
        }
        self.items.sort_by(|(an, ar), (bn, br)| {
            ar.name_hash
                .cmp(&br.name_hash)
                .then_with(|| an.as_bytes().cmp(bn.as_bytes()))
        });

        let mut name_heap = Vec::new();
        let mut records = Vec::with_capacity(self.items.len());
        for (name, mut rec) in self.items {
            rec.name_offset = name_heap.len() as u64;
            rec.name_len = name.len() as u16;
            name_heap.extend_from_slice(name.as_bytes());
            records.push(rec);
        }
        EntryIndex { records, name_heap }
    }
}

/// 組み立て済みのエントリテーブルと NAME HEAP（インメモリ表現）。
pub struct EntryIndex {
    records: Vec<EntryRecord>,
    name_heap: Vec<u8>,
}

impl EntryIndex {
    /// `name_hash` 昇順のエントリテーブル。
    pub fn records(&self) -> &[EntryRecord] {
        &self.records
    }

    /// 連結された NAME HEAP のバイト列。
    pub fn name_heap(&self) -> &[u8] {
        &self.name_heap
    }

    pub fn entry_count(&self) -> usize {
        self.records.len()
    }

    /// インデックス `i` のエントリ名（NAME HEAP からの借用）。
    pub fn name(&self, i: usize) -> &str {
        std::str::from_utf8(self.name_bytes(i)).expect("name heap is valid UTF-8")
    }

    fn name_bytes(&self, i: usize) -> &[u8] {
        let r = &self.records[i];
        let start = r.name_offset as usize;
        &self.name_heap[start..start + r.name_len as usize]
    }

    /// `path` をルックアップし、見つかればテーブル内インデックスを返す。
    ///
    /// `name_hash` を二分探索して同一ハッシュの先頭を求め、その連なりを
    /// 名前一致で走査する（衝突時のフォールバック）。割り当てなし。
    pub fn lookup(&self, path: &str) -> Option<usize> {
        let h = hash_name(path);
        let mut i = self.records.partition_point(|r| r.name_hash < h);
        while i < self.records.len() && self.records[i].name_hash == h {
            if self.name_bytes(i) == path.as_bytes() {
                return Some(i);
            }
            i += 1;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vmidx::ProviderType;

    fn rec(provider: ProviderType) -> EntryRecord {
        EntryRecord {
            name_hash: 0,
            name_offset: 0,
            name_len: 0,
            provider_type: provider,
            entry_flags: 0,
            method_code: 8,
            local_header_offset: 0,
            data_offset: 0,
            compressed_size: 0,
            uncompressed_size: 0,
            chunk_head_offset: 0,
            checkpoint_count: 0,
            commit_count_for_entry: 0,
        }
    }

    #[test]
    fn build_sorts_lays_out_heap_and_resolves() {
        let names = ["foo.txt", "bar.bin", "dir/baz", "alpha", "zzz"];
        let mut b = EntryIndexBuilder::new();
        for n in names {
            b.push(n, rec(ProviderType::Deflate));
        }
        let idx = b.build();

        assert_eq!(idx.entry_count(), names.len());

        // テーブルは name_hash 昇順。
        for w in idx.records().windows(2) {
            assert!(w[0].name_hash <= w[1].name_hash);
        }

        // NAME HEAP の総長 = 全名前のバイト長合計。各レコードの (offset,len) が
        // 正しい名前を指す。
        let total: usize = names.iter().map(|n| n.len()).sum();
        assert_eq!(idx.name_heap().len(), total);

        // 全ての名前がラウンドトリップでき、lookup が自分の位置を返す。
        for n in names {
            let i = idx.lookup(n).unwrap_or_else(|| panic!("{n} not found"));
            assert_eq!(idx.name(i), n);
        }
    }

    #[test]
    fn lookup_miss_returns_none() {
        let mut b = EntryIndexBuilder::new();
        b.push("present", rec(ProviderType::Store));
        let idx = b.build();
        assert_eq!(idx.lookup("absent"), None);
    }

    #[test]
    fn equal_hash_run_disambiguates_by_name() {
        // 同一 name_hash を持つ 2 レコードを手で構成し、ハッシュ衝突時に
        // 名前一致で正しいレコードを選ぶことを確認する。連なりは名前バイト
        // 昇順: "!" (0x21) < "target" (0x74...)。
        let h = hash_name("target");
        let mut r0 = rec(ProviderType::Store);
        r0.name_hash = h;
        r0.name_offset = 0;
        r0.name_len = 1; // "!"
        let mut r1 = rec(ProviderType::Store);
        r1.name_hash = h;
        r1.name_offset = 1;
        r1.name_len = 6; // "target"
        let idx = EntryIndex {
            records: vec![r0, r1],
            name_heap: b"!target".to_vec(),
        };

        // 連なりの 2 番目（名前一致）が選ばれる。
        assert_eq!(idx.lookup("target"), Some(1));
        // 同じハッシュ帯に無い名前は見つからない。
        assert_eq!(idx.lookup("does-not-exist"), None);
    }
}
