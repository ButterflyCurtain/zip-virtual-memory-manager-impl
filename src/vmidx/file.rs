//! vmidx ファイル全体の serialize / parse（仕様 Section 1 のレイアウト）。
//!
//! ```text
//! [ FILE HEADER       ]  固定 128 バイト、オフセット 0
//! [ ENTRY TABLE       ]  entry_count × 96、name_hash 昇順
//! [ NAME HEAP         ]  連結された UTF-8 名前列
//! [ ADVISORY REGION   ]  カウンタ群（CRC なし、in-place 更新）
//! [ CHECKPOINT CHUNKS ]  追記ゾーン、セッション中に EOF まで伸びる
//! ```
//!
//! 先頭 4 領域は構造的書き直し（Section 6.3）でまとめて書かれ、セッション中は
//! 動かない。[`VmidxBuilder::serialize`] はこの「完全なインデックス像」を
//! 1 つのバイト列として組み立てる（consolidation 後の像、各エントリ 1 チャンク）。
//!
//! [`Vmidx`] は read-only で mmap した（= バイトスライスとして借りた）像の上の
//! 軽量ビュー。open() の検証カスケード（Section 7）のうち、構造に閉じた段
//! ——マジック / format_version / header_crc32（[`Header::decode`]）と領域境界
//! （step 4）——をパース時に行う。ENTRY RECORD / CHECKPOINT CHUNK の CRC は
//! 触れた時に遅延検証する（step 5）。fingerprint 照合（step 3、archive.zip 相手）
//! は呼び出し側の責務。

use super::{
    Advisory, BlockState, Checkpoint, CheckpointChunk, DecodeError, EntryRecord, Header, hash_name,
    rd_u64, ENTRY_SIZE, HEADER_SIZE,
};

/// 完全な vmidx 像を組み立てるビルダ。構造的書き直し（Section 6.3）で
/// vmidx.tmp に書く像を作るのに使う。各エントリのチェックポイントは 1 つの
/// CHECKPOINT CHUNK に集約される（consolidation 後の形）。
pub struct VmidxBuilder {
    /// FILE HEADER の `flags`（[`super::flags`]）。
    pub flags: u16,
    /// マウントの page_size。
    pub page_size: u32,
    /// 展開バイト単位のチェックポイント基準間隔。
    pub checkpoint_interval: u64,
    pub source_file_size: u64,
    pub source_inode: u64,
    pub source_mtime_ns: u64,
    /// Central Directory ブロックの XXH3-128（16 バイト）+ 4 バイトゼロ詰め。
    pub source_cd_hash: [u8; 20],
    entries: Vec<BuildEntry>,
    block_state: Vec<BlockState>,
}

struct BuildEntry {
    name: String,
    record: EntryRecord,
    checkpoints: Vec<Checkpoint>,
}

impl Default for VmidxBuilder {
    fn default() -> VmidxBuilder {
        VmidxBuilder {
            flags: 0,
            page_size: 4096,
            checkpoint_interval: 1_048_576,
            source_file_size: 0,
            source_inode: 0,
            source_mtime_ns: 0,
            source_cd_hash: [0u8; 20],
            entries: Vec::new(),
            block_state: Vec::new(),
        }
    }
}

impl VmidxBuilder {
    pub fn new() -> VmidxBuilder {
        VmidxBuilder::default()
    }

    /// エントリを追加する。`record` の `name_hash` / `name_offset` / `name_len` /
    /// `chunk_head_offset` / `checkpoint_count` は無視され、`serialize()` 時に
    /// `name` と `checkpoints` から計算・設定される。`checkpoints` の各記録の
    /// プロバイダは `record.provider_type` と一致していなければならない
    /// （STORE / UNSUPPORTED にチェックポイントは付けられない）。
    pub fn push(&mut self, name: impl Into<String>, record: EntryRecord, checkpoints: Vec<Checkpoint>) {
        self.entries.push(BuildEntry {
            name: name.into(),
            record,
            checkpoints,
        });
    }

    /// ADVISORY REGION の BLOCK STATE 群を設定する（miss_count は entry_count
    /// 個のゼロで初期化される）。
    pub fn set_block_state(&mut self, block_state: Vec<BlockState>) {
        self.block_state = block_state;
    }

    /// 完全な vmidx 像をバイト列へ組み立てる。全オフセットを実レイアウトで
    /// 確定し、ENTRY RECORD の `chunk_head_offset` をチェックポイントゾーンの
    /// 実位置へ patch する。
    pub fn serialize(mut self) -> Vec<u8> {
        // 1. name_hash を計算し、テーブル整列順（name_hash 昇順・同一ハッシュ内は
        //    名前バイト昇順）に並べる。
        for e in &mut self.entries {
            e.record.name_hash = hash_name(&e.name);
        }
        self.entries.sort_by(|a, b| {
            a.record
                .name_hash
                .cmp(&b.record.name_hash)
                .then_with(|| a.name.as_bytes().cmp(b.name.as_bytes()))
        });

        // 2. NAME HEAP を組み立て、各レコードの name_offset / name_len を確定。
        let mut name_heap = Vec::new();
        for e in &mut self.entries {
            e.record.name_offset = name_heap.len() as u64;
            e.record.name_len = e.name.len() as u16;
            name_heap.extend_from_slice(e.name.as_bytes());
        }

        // 3. 先頭 4 領域のオフセットを確定。
        let entry_count = self.entries.len();
        let entry_table_offset = HEADER_SIZE;
        let name_heap_offset = entry_table_offset + entry_count * ENTRY_SIZE;
        let name_heap_size = name_heap.len();
        let advisory_offset = name_heap_offset + name_heap_size;

        let advisory = Advisory {
            miss_count: vec![0; entry_count],
            block_state: std::mem::take(&mut self.block_state),
        };
        let advisory_size = advisory.byte_size();
        let checkpoint_zone = advisory_offset + advisory_size;

        // 4. チェックポイントゾーンを組み立て、各エントリの chunk_head_offset を
        //    実位置へ patch（各エントリ 1 チャンク、next_chunk_offset = 0）。
        let mut chunk_bytes = Vec::new();
        let mut running = checkpoint_zone;
        for (i, e) in self.entries.iter_mut().enumerate() {
            if e.checkpoints.is_empty() {
                e.record.chunk_head_offset = 0;
                e.record.checkpoint_count = 0;
                continue;
            }
            let count = e.checkpoints.len();
            let chunk = CheckpointChunk {
                entry_index: i as u64,
                provider: e.record.provider_type,
                next_chunk_offset: 0,
                checkpoints: std::mem::take(&mut e.checkpoints),
            };
            let enc = chunk.encode();
            e.record.chunk_head_offset = running as u64;
            e.record.checkpoint_count = count as u64;
            running += enc.len();
            chunk_bytes.extend_from_slice(&enc);
        }

        // 5. FILE HEADER を確定。
        let header = Header {
            flags: self.flags,
            page_size: self.page_size,
            checkpoint_interval: self.checkpoint_interval,
            source_file_size: self.source_file_size,
            source_inode: self.source_inode,
            source_mtime_ns: self.source_mtime_ns,
            source_cd_hash: self.source_cd_hash,
            entry_count: entry_count as u64,
            entry_table_offset: entry_table_offset as u64,
            name_heap_offset: name_heap_offset as u64,
            name_heap_size: name_heap_size as u64,
            advisory_offset: advisory_offset as u64,
            advisory_size: advisory_size as u64,
        };

        // 6. レイアウト順に連結。各セクションの長さは上で確定したオフセットと
        //    一致する。
        let mut out = Vec::with_capacity(checkpoint_zone + chunk_bytes.len());
        out.extend_from_slice(&header.encode());
        for e in &self.entries {
            out.extend_from_slice(&e.record.encode());
        }
        out.extend_from_slice(&name_heap);
        let mut adv_buf = vec![0u8; advisory_size];
        advisory.encode_into(&mut adv_buf);
        out.extend_from_slice(&adv_buf);
        out.extend_from_slice(&chunk_bytes);
        debug_assert_eq!(out.len(), checkpoint_zone + chunk_bytes.len());
        out
    }
}

/// read-only で mmap した vmidx 像（バイトスライス）の上の軽量ビュー。
/// 所有はせず、`bytes` の生存期間に縛られる。
pub struct Vmidx<'a> {
    header: Header,
    bytes: &'a [u8],
}

impl<'a> Vmidx<'a> {
    /// 像をパースする。Section 7 の検証カスケードのうち構造に閉じた段を行う:
    /// step 1（≥128B・magic・format_version）/ step 2（header_crc32、
    /// [`Header::decode`] 内）/ step 4（領域境界）。ENTRY RECORD / CHUNK の
    /// CRC（step 5）は触れた時に遅延検証する。fingerprint 照合（step 3）は
    /// 行わない。いずれの失敗も、最終的な応答は「破棄して再構築」。
    pub fn parse(bytes: &'a [u8]) -> Result<Vmidx<'a>, DecodeError> {
        if bytes.len() < HEADER_SIZE {
            return Err(DecodeError::FileTooSmall);
        }
        let header_bytes: &[u8; HEADER_SIZE] = bytes[..HEADER_SIZE].try_into().unwrap();
        let header = Header::decode(header_bytes)?;

        // step 4: 領域境界。各領域がファイル内に収まり、順に並んで重ならないこと。
        // entry_count × 96 が entry table 領域と整合すること。
        let len = bytes.len() as u64;
        let table_size = header
            .entry_count
            .checked_mul(ENTRY_SIZE as u64)
            .ok_or(DecodeError::RegionOutOfBounds)?;
        if header.entry_table_offset < HEADER_SIZE as u64 {
            return Err(DecodeError::RegionOutOfBounds);
        }
        let table_end = header
            .entry_table_offset
            .checked_add(table_size)
            .ok_or(DecodeError::RegionOutOfBounds)?;
        let heap_end = header
            .name_heap_offset
            .checked_add(header.name_heap_size)
            .ok_or(DecodeError::RegionOutOfBounds)?;
        let advisory_end = header
            .advisory_offset
            .checked_add(header.advisory_size)
            .ok_or(DecodeError::RegionOutOfBounds)?;
        if header.name_heap_offset < table_end
            || header.advisory_offset < heap_end
            || advisory_end > len
        {
            return Err(DecodeError::RegionOutOfBounds);
        }

        Ok(Vmidx { header, bytes })
    }

    /// パース済みの FILE HEADER。
    pub fn header(&self) -> &Header {
        &self.header
    }

    /// エントリ数。
    pub fn entry_count(&self) -> u64 {
        self.header.entry_count
    }

    /// 像の生バイト（CHECKPOINT CHUNK 走査などで先頭からのオフセットを使う）。
    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// インデックス `i` の ENTRY RECORD をデコードする。`record_crc32` を
    /// この時に検証する（Section 7 step 5、遅延 CRC）。
    ///
    /// `i` が範囲外ならパニックする（プログラミングエラー。範囲は
    /// `entry_count()` で得られる）。
    pub fn entry(&self, i: usize) -> Result<EntryRecord, DecodeError> {
        assert!(
            (i as u64) < self.header.entry_count,
            "entry index {i} out of range (entry_count {})",
            self.header.entry_count
        );
        let off = self.header.entry_table_offset as usize + i * ENTRY_SIZE;
        let slice: &[u8; ENTRY_SIZE] = self.bytes[off..off + ENTRY_SIZE].try_into().unwrap();
        EntryRecord::decode(slice)
    }

    /// インデックス `i` の `name_hash` だけを読む（デコードなし、二分探索用）。
    fn name_hash_at(&self, i: usize) -> u64 {
        // name_hash は ENTRY RECORD のオフセット 0。
        let off = self.header.entry_table_offset as usize + i * ENTRY_SIZE;
        rd_u64(self.bytes, off)
    }

    /// レコードの指す NAME HEAP 上の名前バイト列。範囲が NAME HEAP 内に
    /// 収まらなければ `None`（破損 → 再構築の合図）。
    fn name_bytes(&self, rec: &EntryRecord) -> Option<&'a [u8]> {
        let heap_start = self.header.name_heap_offset as usize;
        let heap_end = heap_start + self.header.name_heap_size as usize;
        let start = heap_start.checked_add(rec.name_offset as usize)?;
        let end = start.checked_add(rec.name_len as usize)?;
        if end > heap_end {
            return None;
        }
        self.bytes.get(start..end)
    }

    /// レコードの指すエントリ名（NAME HEAP からの借用）。範囲外・非 UTF-8 は
    /// `None`。
    pub fn name(&self, rec: &EntryRecord) -> Option<&'a str> {
        std::str::from_utf8(self.name_bytes(rec)?).ok()
    }

    /// `path` をルックアップする。`name_hash` を二分探索し、同一ハッシュの
    /// 連なりを名前一致で走査する（Section 3、割り当てなし）。見つかれば
    /// (テーブル内インデックス, レコード) を返す。走査で触れたレコードの
    /// CRC はこの時に検証される。
    pub fn lookup(&self, path: &str) -> Result<Option<(usize, EntryRecord)>, DecodeError> {
        let h = hash_name(path);
        let n = self.header.entry_count as usize;

        // name_hash < h の partition point を求める（生フィールドで二分探索）。
        let (mut lo, mut hi) = (0usize, n);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.name_hash_at(mid) < h {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }

        let mut i = lo;
        while i < n && self.name_hash_at(i) == h {
            let rec = self.entry(i)?;
            if self.name_bytes(&rec) == Some(path.as_bytes()) {
                return Ok(Some((i, rec)));
            }
            i += 1;
        }
        Ok(None)
    }

    /// レコードのチェックポイントチェーンを辿り、`uncompressed_offset ≤ target`
    /// の最近チェックポイントを返す（Section 5、read(path) 手順 4）。チャンク
    /// CRC はこの時に検証される。
    pub fn nearest_checkpoint(
        &self,
        rec: &EntryRecord,
        target: u64,
    ) -> Result<Option<Checkpoint>, DecodeError> {
        super::nearest_checkpoint(self.bytes, rec.chunk_head_offset, target)
    }

    /// fingerprint 照合（Section 7 step 3）。`live` は archive.zip の現在の
    /// stat 値と算出済み cd_hash。判定（Valid / ValidStale / Invalid）だけを
    /// 返し、無効時の再構築・CONFLICT 判断はマウント層に委ねる。
    pub fn check_fingerprint(&self, live: &super::SourceStat) -> super::FingerprintVerdict {
        super::check_fingerprint(&self.header, live)
    }

    /// ADVISORY REGION をパースして返す（CRC なし、不整合はゼロへ clamp）。
    pub fn advisory(&self) -> Advisory {
        let start = self.header.advisory_offset as usize;
        let end = start + self.header.advisory_size as usize;
        Advisory::parse(&self.bytes[start..end], self.header.entry_count as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vmidx::{flags, ProviderType};

    fn rec(provider: ProviderType, uncompressed_size: u64) -> EntryRecord {
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
            uncompressed_size,
            chunk_head_offset: 0,
            checkpoint_count: 0,
            commit_count_for_entry: 0,
        }
    }

    fn vmm(uncompressed_offset: u64) -> Checkpoint {
        Checkpoint::DeflateVmm {
            compressed_offset: uncompressed_offset / 2,
            uncompressed_offset,
            capacity: 4096,
        }
    }

    fn build_sample() -> Vec<u8> {
        let mut b = VmidxBuilder::new();
        b.flags = flags::VMM_GENERATED;
        b.page_size = 4096;
        b.source_file_size = 123_456;
        b.source_inode = 42;
        b.source_mtime_ns = 1_700_000_000_000_000_000;
        b.source_cd_hash = [9u8; 20];
        // STORE: チェックポイント無し。
        b.push("store.bin", rec(ProviderType::Store, 1000), vec![]);
        // DEFLATE_VMM: チェックポイント 3 件。
        b.push(
            "blocks.dat",
            rec(ProviderType::DeflateVmm, 1 << 20),
            vec![vmm(0), vmm(4096), vmm(8192)],
        );
        // ZSTD: チェックポイント 2 件。
        b.push(
            "frames.zst",
            rec(ProviderType::Zstd, 1 << 21),
            vec![
                Checkpoint::Zstd {
                    frame_offset: 0,
                    uncompressed_offset: 0,
                },
                Checkpoint::Zstd {
                    frame_offset: 500,
                    uncompressed_offset: 65_536,
                },
            ],
        );
        b.serialize()
    }

    #[test]
    fn full_roundtrip_header_and_layout() {
        let image = build_sample();
        let v = Vmidx::parse(&image).expect("parse ok");
        let h = v.header();
        assert_eq!(h.entry_count, 3);
        assert_eq!(h.entry_table_offset, HEADER_SIZE as u64);
        assert_eq!(h.name_heap_offset, HEADER_SIZE as u64 + 3 * ENTRY_SIZE as u64);
        // 名前の総バイト長。
        let names_len = "store.bin".len() + "blocks.dat".len() + "frames.zst".len();
        assert_eq!(h.name_heap_size, names_len as u64);
        assert_eq!(h.flags, flags::VMM_GENERATED);
        assert_eq!(h.source_inode, 42);
        assert_eq!(h.source_cd_hash, [9u8; 20]);
        // advisory は miss_count(3×8) + block_state_count(8) = 32 バイト。
        assert_eq!(h.advisory_size, 3 * 8 + 8);
    }

    #[test]
    fn lookup_resolves_every_name_and_checkpoints() {
        let image = build_sample();
        let v = Vmidx::parse(&image).expect("parse ok");

        for name in ["store.bin", "blocks.dat", "frames.zst"] {
            let (i, rec) = v
                .lookup(name)
                .expect("lookup ok")
                .unwrap_or_else(|| panic!("{name} not found"));
            assert_eq!(v.name(&rec), Some(name));
            assert!((i as u64) < v.entry_count());
        }

        // STORE はチェックポイント無し。
        let (_, store) = v.lookup("store.bin").unwrap().unwrap();
        assert_eq!(store.chunk_head_offset, 0);
        assert_eq!(store.checkpoint_count, 0);
        assert_eq!(v.nearest_checkpoint(&store, 999).unwrap(), None);

        // DEFLATE_VMM はチェーンを辿って最近チェックポイントが採れる。
        let (_, blocks) = v.lookup("blocks.dat").unwrap().unwrap();
        assert_ne!(blocks.chunk_head_offset, 0);
        assert_eq!(blocks.checkpoint_count, 3);
        let cp = v
            .nearest_checkpoint(&blocks, 5000)
            .expect("walk ok")
            .expect("some checkpoint");
        assert_eq!(cp.uncompressed_offset(), 4096);

        // ZSTD も同様。
        let (_, frames) = v.lookup("frames.zst").unwrap().unwrap();
        let cp = v
            .nearest_checkpoint(&frames, 70_000)
            .expect("walk ok")
            .expect("some checkpoint");
        assert_eq!(cp.uncompressed_offset(), 65_536);
    }

    #[test]
    fn lookup_miss_returns_none() {
        let image = build_sample();
        let v = Vmidx::parse(&image).expect("parse ok");
        assert_eq!(v.lookup("absent.txt").expect("lookup ok"), None);
    }

    #[test]
    fn empty_index_roundtrips() {
        let image = VmidxBuilder::new().serialize();
        let v = Vmidx::parse(&image).expect("parse ok");
        assert_eq!(v.entry_count(), 0);
        assert_eq!(v.lookup("anything").expect("lookup ok"), None);
        assert_eq!(v.advisory(), Advisory::zeroed(0));
    }

    #[test]
    fn advisory_block_state_roundtrips() {
        let mut b = VmidxBuilder::new();
        b.push("a", rec(ProviderType::Store, 1), vec![]);
        b.push("b", rec(ProviderType::Store, 2), vec![]);
        b.set_block_state(vec![BlockState {
            entry_index: 1,
            block_index: 0,
            state_flags: crate::vmidx::block_state_flags::OVERFLOW,
            overflow_count: 2,
            shrink_streak: 0,
        }]);
        let image = b.serialize();
        let v = Vmidx::parse(&image).expect("parse ok");
        let adv = v.advisory();
        assert_eq!(adv.miss_count, vec![0, 0]);
        assert_eq!(adv.block_state.len(), 1);
        assert_eq!(adv.block_state[0].entry_index, 1);
        assert_eq!(adv.block_state[0].overflow_count, 2);
    }

    #[test]
    fn parse_rejects_too_small() {
        assert!(matches!(
            Vmidx::parse(&[0u8; 64]),
            Err(DecodeError::FileTooSmall)
        ));
    }

    #[test]
    fn parse_rejects_region_out_of_bounds() {
        // header_crc は正しいが entry_count が像に対して過大 → 領域境界違反。
        let h = Header {
            flags: 0,
            page_size: 4096,
            checkpoint_interval: 1 << 20,
            source_file_size: 0,
            source_inode: 0,
            source_mtime_ns: 0,
            source_cd_hash: [0u8; 20],
            entry_count: 1000,
            entry_table_offset: HEADER_SIZE as u64,
            name_heap_offset: HEADER_SIZE as u64 + 1000 * ENTRY_SIZE as u64,
            name_heap_size: 0,
            advisory_offset: HEADER_SIZE as u64 + 1000 * ENTRY_SIZE as u64,
            advisory_size: 0,
        };
        let image = h.encode(); // 128 バイトしかない
        assert!(matches!(
            Vmidx::parse(&image),
            Err(DecodeError::RegionOutOfBounds)
        ));
    }

    #[test]
    fn entry_record_crc_validated_lazily() {
        let mut image = build_sample();
        let v = Vmidx::parse(&image).expect("parse ok");
        // 0 番レコードの reserved 近辺の 1 バイトを反転（name_hash には触れない）。
        // `v` は Drop を持たない借用ビューなので、最後の使用（直上の行）で
        // 借用が終わる。明示的な drop は不要。
        let off = v.header().entry_table_offset as usize + 80; // reserved2 領域
        image[off] ^= 0x01;
        let v = Vmidx::parse(&image).expect("header still parses");
        match v.entry(0) {
            Err(DecodeError::RecordCrcMismatch { .. }) => {}
            other => panic!("expected RecordCrcMismatch, got {other:?}"),
        }
    }

    #[test]
    fn header_crc_corruption_fails_parse() {
        let mut image = build_sample();
        image[40] ^= 0x01; // source_mtime_ns（header CRC 対象）の 1 ビット
        match Vmidx::parse(&image) {
            Err(DecodeError::HeaderCrcMismatch { .. }) => {}
            Err(other) => panic!("expected HeaderCrcMismatch, got {other:?}"),
            Ok(_) => panic!("expected HeaderCrcMismatch, got Ok"),
        }
    }

    #[test]
    fn check_fingerprint_through_view() {
        use crate::vmidx::{FingerprintVerdict, SourceStat, hash_cd_block};
        let cd = b"central directory block bytes";
        let mut b = VmidxBuilder::new();
        b.source_file_size = 4096;
        b.source_inode = 7;
        b.source_mtime_ns = 111;
        b.source_cd_hash[..16].copy_from_slice(&hash_cd_block(cd));
        b.push("x", rec(ProviderType::Store, 1), vec![]);
        let image = b.serialize();
        let v = Vmidx::parse(&image).expect("parse ok");

        let live_ok = SourceStat {
            file_size: 4096,
            inode: 7,
            mtime_ns: 111,
            cd_hash: hash_cd_block(cd),
        };
        assert_eq!(v.check_fingerprint(&live_ok), FingerprintVerdict::Valid);

        let live_changed = SourceStat {
            cd_hash: hash_cd_block(b"different cd"),
            ..live_ok
        };
        assert_eq!(
            v.check_fingerprint(&live_changed),
            FingerprintVerdict::Invalid
        );
    }

    #[test]
    fn version_is_one() {
        // format_version が像に正しく書かれていること（Header 経由）。
        let image = build_sample();
        assert_eq!(crate::vmidx::rd_u16(&image, 8), crate::vmidx::FORMAT_VERSION);
    }
}
