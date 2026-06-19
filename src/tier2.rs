//! Tier 2 spill ストア（設計 LAYER 2 の Tier 2 + `Tier1DirtyStore` / `VmdirtyIndex`）。
//!
//! [`DiffLayer`](crate::difflayer)（Tier 1、メモリ内）から溢れた dirty ページを
//! vmdirty ジャーナル（ディスク）へ書き出し、`(entry_name, page_index)` →
//! ファイル内オフセットの **in-memory 索引**（設計 `VmdirtyIndex`）で読み戻す。
//!
//! このモジュールは「Tier 1 ↔ ディスクの橋渡し」だけを持つ:
//! - [`Tier2::spill_over_limit`]: `dirty_limit` 超過分を FIFO で退避（設計 4.1 /
//!   5.1）。victim 選択は [`DiffLayer::take_spill_victims`]（純ロジック）に委ね、
//!   ここは退避ページを vmdirty へ書いて索引に積むだけ。
//! - [`Tier2::read_page`]: 索引にあれば vmdirty から該当ページを読み戻す。
//! - [`Tier2::write_hit`]: 既に Tier 2 にあるページへの write hit（設計 4.1）＝
//!   新しい DATA RECORD を追記して索引を更新（旧レコードは回復時に supersede）。
//! - [`Tier2::flush`]: Tier 1 常駐ページを全て durable 化し COMMIT MARKER を書く
//!   （設計 4.2 STRICT flush）。**Tier 1 からは外さない**（durable コピーを作る
//!   だけ。commit の [`rehydrate_into`](Tier2::rehydrate_into) と合わせて使う）。
//! - [`Tier2::rehydrate_into`]: Tier 2 のみに在るページを Tier 1 に読み戻す
//!   （commit が `build_full` で全 dirty ページを Tier 1 から読めるようにする）。
//!
//! 末尾ページのテール短長書き込み（`logical_size` でクランプ）により、回復時の
//! `logical_size` 復元（設計 Section 2 の max ルール: `page_index×page_size +
//! data_len`）が正しく効く。読み戻しは [`vmdirty::read_page_at`] が `page_size`
//! までゼロ埋めして均一なページを返す。

use crate::difflayer::DiffLayer;
use crate::vmdirty::{self, DataLoc, Header, SyncPolicy, VmdirtyWriter};
use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::path::Path;

/// Tier 2 ストア（1 マウントにつき 1 つの vmdirty ジャーナルを束ねる）。
pub struct Tier2 {
    writer: VmdirtyWriter,
    /// vmdirty への read 専用ハンドル（追記中の writer ハンドルとは別。位置指定
    /// 読みで Tier 2 ページを引く）。
    read_file: File,
    /// `entry_name` → (`page_index` → ディスク上の最新 DATA RECORD 位置)。
    /// 同じページの write hit / 再 spill では最新の [`DataLoc`] で上書きする
    /// （古いレコードは回復 walk の sequence 順 replay で supersede される）。
    index: HashMap<String, HashMap<u64, DataLoc>>,
    page_size: u64,
}

impl Tier2 {
    /// 新しい vmdirty を作って Tier 2 を開始する（`header` の generation_id で
    /// セッションを刻む）。FILE HEADER を書いて durable にしてから返る。
    pub fn create(
        path: &Path,
        header: &Header,
        sync: SyncPolicy,
        page_size: u64,
    ) -> io::Result<Tier2> {
        let writer = VmdirtyWriter::create(path, header, sync)?;
        let read_file = File::open(path)?;
        Ok(Tier2 {
            writer,
            read_file,
            index: HashMap::new(),
            page_size: page_size.max(1),
        })
    }

    /// このセッションの generation_id。
    pub fn generation_id(&self) -> [u8; 16] {
        self.writer.generation_id()
    }

    /// ページ `(entry, page)` が Tier 2（ディスク）に在るか。
    pub fn has(&self, entry: &str, page: u64) -> bool {
        self.loc(entry, page).is_some()
    }

    fn loc(&self, entry: &str, page: u64) -> Option<DataLoc> {
        self.index.get(entry).and_then(|m| m.get(&page)).copied()
    }

    fn put(&mut self, entry: &str, page: u64, loc: DataLoc) {
        self.index
            .entry(entry.to_owned())
            .or_default()
            .insert(page, loc);
    }

    /// Tier 2 にあるページを `page_size` バイトで読み戻す（末尾の短いページは
    /// ゼロ埋め）。索引に無ければ `None`。
    pub fn read_page(&self, entry: &str, page: u64) -> io::Result<Option<Vec<u8>>> {
        match self.loc(entry, page) {
            Some(loc) => Ok(Some(vmdirty::read_page_at(
                &self.read_file,
                loc.data_offset,
                loc.data_len as usize,
                self.page_size as usize,
            )?)),
            None => Ok(None),
        }
    }

    /// `dirty_limit` 超過分を FIFO で Tier 1 から退避し、vmdirty へ書いて索引へ
    /// 積む（設計 4.1）。victim 選択は [`DiffLayer::take_spill_victims`] が行い、
    /// ここでは末尾ページを `logical_size` でクランプして書く。
    pub fn spill_over_limit(&mut self, diff: &mut DiffLayer) -> io::Result<()> {
        let victims = diff.take_spill_victims();
        for v in victims {
            let logical = diff.logical_size(&v.entry_name).unwrap_or(0);
            let data = clamp_tail(&v.data, v.page_index, self.page_size, logical);
            let loc = self
                .writer
                .append_data_record(&v.entry_name, v.page_index, data)?;
            self.put(&v.entry_name, v.page_index, loc);
        }
        Ok(())
    }

    /// 既に Tier 2 に在るページへの write hit（設計 4.1）。新しい DATA RECORD を
    /// 追記して索引を最新へ。`full_page` は `page_size` バイトの全体像、`logical`
    /// は現在の論理サイズ（テールクランプ用）。
    pub fn write_hit(
        &mut self,
        entry: &str,
        page: u64,
        full_page: &[u8],
        logical: u64,
    ) -> io::Result<()> {
        let data = clamp_tail(full_page, page, self.page_size, logical);
        let loc = self.writer.append_data_record(entry, page, data)?;
        self.put(entry, page, loc);
        Ok(())
    }

    /// STRICT flush（設計 4.2）: Tier 1 常駐ページを全て vmdirty へ書き、COMMIT
    /// MARKER を書いて durable にする。**Tier 1 からページは外さない**（durable な
    /// コピーを作るだけ）。これでクラッシュ前の状態が `recover_committed` で
    /// 丸ごと復元できる。
    pub fn flush(&mut self, diff: &mut DiffLayer) -> io::Result<()> {
        for (entry, page) in diff.resident_pages() {
            let logical = diff.logical_size(&entry).unwrap_or(0);
            let full = diff.page(&entry, page).expect("resident page").to_vec();
            let data = clamp_tail(&full, page, self.page_size, logical);
            let loc = self.writer.append_data_record(&entry, page, data)?;
            self.put(&entry, page, loc);
        }
        let seq = self.writer.last_sequence();
        let count = self.page_count() as u64;
        self.writer.append_commit_marker(seq, count)?;
        Ok(())
    }

    /// Tier 2 のみに在る（Tier 1 に常駐していない）ページを Tier 1 に読み戻す。
    /// commit の前に呼び、`build_full` が全 dirty ページを Tier 1 から読めるように
    /// する。entry の `logical_size` は Diff Layer 側に保たれている前提
    /// （pressure-spill は entry を残す）。
    pub fn rehydrate_into(&self, diff: &mut DiffLayer) -> io::Result<()> {
        for (entry, pages) in &self.index {
            for (&page, &loc) in pages {
                if diff.has_page(entry, page) {
                    continue;
                }
                // entry が（万一）抜けていても insert できるよう保険で確保する。
                diff.ensure_entry(entry, diff.logical_size(entry).unwrap_or(0));
                let buf = vmdirty::read_page_at(
                    &self.read_file,
                    loc.data_offset,
                    loc.data_len as usize,
                    self.page_size as usize,
                )?;
                diff.insert_page(entry, page, buf);
            }
        }
        Ok(())
    }

    /// ディスク上に在る dirty ページの異なり数（COMMIT MARKER の `page_count`、
    /// 情報目的）。
    fn page_count(&self) -> usize {
        self.index.values().map(HashMap::len).sum()
    }
}

/// 退避/書き出すページを `logical_size` でクランプする。末尾ページは
/// `logical - page_start` バイト（短いテール）、内側のページは `page_size`
/// バイトをそのまま返す。回復時の `logical_size` 復元（max ルール）が効くように、
/// 末尾ページの `data_len` を実長に揃えるのが目的。
fn clamp_tail(data: &[u8], page_index: u64, page_size: u64, logical: u64) -> &[u8] {
    let page_start = page_index.saturating_mul(page_size);
    let tail = logical.saturating_sub(page_start);
    let n = (tail.min(page_size) as usize).min(data.len());
    &data[..n]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::difflayer::UNLIMITED;
    use crate::vmdirty::{new_generation_id, now_ns, read_vmdirty};

    struct TempFile(std::path::PathBuf);
    impl TempFile {
        fn new(tag: &str) -> TempFile {
            use std::sync::atomic::{AtomicU32, Ordering};
            static N: AtomicU32 = AtomicU32::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            TempFile(std::env::temp_dir().join(format!(
                "zipvmm_tier2_{}_{}_{}",
                std::process::id(),
                tag,
                n
            )))
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn header(gen_id: [u8; 16], page_size: u32) -> Header {
        Header {
            flags: 0,
            generation_id: gen_id,
            source_file_size: 0,
            source_inode: 0,
            source_cd_hash: [0u8; 20],
            created_at_ns: now_ns(),
            page_size,
        }
    }

    fn tier2(path: &Path, page_size: u64) -> Tier2 {
        Tier2::create(path, &header(new_generation_id(), page_size as u32), SyncPolicy::Sync, page_size)
            .expect("create tier2")
    }

    /// `path` の `page` を識別用バイトで満たして Diff Layer に入れる。
    fn put(d: &mut DiffLayer, path: &str, page: u64, fill: u8, logical: u64) {
        d.ensure_entry(path, logical);
        d.set_logical_size(path, logical);
        d.insert_page(path, page, vec![fill; d.page_size() as usize]);
    }

    #[test]
    fn spill_over_limit_writes_and_reads_back() {
        let tf = TempFile::new("spill");
        let mut t = tier2(tf.path(), 8);
        // 上限 2 ページ。3 ページ入れて 1 枚（最古）を spill。
        let mut d = DiffLayer::with_dirty_limit(8, 2 * 8);
        put(&mut d, "a", 0, 0xA0, 24); // logical 24 = 3 ページ
        put(&mut d, "a", 1, 0xA1, 24);
        put(&mut d, "a", 2, 0xA2, 24);
        assert!(d.over_limit());

        t.spill_over_limit(&mut d).unwrap();
        // 最古 (a,0) が Tier 2 へ。Tier 1 からは消える。
        assert!(!d.has_page("a", 0));
        assert!(t.has("a", 0));
        let p = t.read_page("a", 0).unwrap().unwrap();
        assert_eq!(p, vec![0xA0; 8]);
        // 索引に無いページは None。
        assert!(t.read_page("a", 1).unwrap().is_none());
    }

    #[test]
    fn spill_clamps_short_tail_for_logical_recovery() {
        let tf = TempFile::new("tail");
        let mut t = tier2(tf.path(), 8);
        // logical 10 = ページ0(8B) + ページ1(2B のテール)。上限 0 で即 spill。
        let mut d = DiffLayer::with_dirty_limit(8, 0);
        put(&mut d, "f", 0, 0x11, 10);
        put(&mut d, "f", 1, 0x22, 10);
        t.spill_over_limit(&mut d).unwrap();

        // テールページは 2 バイトだけ読める（残りはゼロ埋め）。
        let p1 = t.read_page("f", 1).unwrap().unwrap();
        assert_eq!(&p1[..2], &[0x22, 0x22]);
        assert!(p1[2..].iter().all(|&b| b == 0));

        // 回復 walk からも logical=10 が復元できる（max ルール）。
        let bytes = std::fs::read(tf.path()).unwrap();
        let r = read_vmdirty(&bytes);
        assert_eq!(r.status, vmdirty::RecoveryStatus::Ok);
        let logical = r
            .uncommitted_pages
            .iter()
            .filter(|p| p.entry_name == "f")
            .map(|p| p.page_index * 8 + p.data.len() as u64)
            .max()
            .unwrap();
        assert_eq!(logical, 10);
    }

    #[test]
    fn write_hit_appends_newer_record() {
        let tf = TempFile::new("hit");
        let mut t = tier2(tf.path(), 8);
        let mut d = DiffLayer::with_dirty_limit(8, 0);
        put(&mut d, "a", 0, 0x01, 8);
        t.spill_over_limit(&mut d).unwrap();
        assert_eq!(t.read_page("a", 0).unwrap().unwrap(), vec![0x01; 8]);

        // Tier 2 ページへ write hit: 新しい内容で上書き。
        t.write_hit("a", 0, &vec![0x99; 8], 8).unwrap();
        assert_eq!(t.read_page("a", 0).unwrap().unwrap(), vec![0x99; 8]);

        // 回復 walk では sequence 順に 2 レコード（古い→新しい）。
        let bytes = std::fs::read(tf.path()).unwrap();
        let r = read_vmdirty(&bytes);
        let pages: Vec<_> = r.uncommitted_pages.iter().map(|p| (p.sequence, p.data[0])).collect();
        assert_eq!(pages, vec![(1, 0x01), (2, 0x99)]);
    }

    #[test]
    fn flush_keeps_tier1_and_writes_commit_marker() {
        let tf = TempFile::new("flush");
        let mut t = tier2(tf.path(), 8);
        let mut d = DiffLayer::new(8); // UNLIMITED = spill しない
        assert_eq!(d.dirty_limit(), UNLIMITED);
        put(&mut d, "a", 0, 0x01, 16);
        put(&mut d, "a", 1, 0x02, 16);

        t.flush(&mut d).unwrap();
        // flush は Tier 1 を空にしない。
        assert!(d.has_page("a", 0));
        assert!(d.has_page("a", 1));
        // ディスク側からは両ページが読める。
        assert_eq!(t.read_page("a", 0).unwrap().unwrap(), vec![0x01; 8]);
        assert_eq!(t.read_page("a", 1).unwrap().unwrap(), vec![0x02; 8]);

        // COMMIT MARKER 済み → 全ページが committed。
        let bytes = std::fs::read(tf.path()).unwrap();
        let r = read_vmdirty(&bytes);
        assert!(r.last_commit_seq > 0);
        assert_eq!(r.committed_pages.len(), 2);
        assert!(r.uncommitted_pages.is_empty());
    }

    #[test]
    fn rehydrate_brings_spilled_pages_back_into_tier1() {
        let tf = TempFile::new("rehydrate");
        let mut t = tier2(tf.path(), 8);
        let mut d = DiffLayer::with_dirty_limit(8, 8); // 1 ページだけ常駐可
        put(&mut d, "a", 0, 0x01, 24);
        put(&mut d, "a", 1, 0x02, 24);
        put(&mut d, "a", 2, 0x03, 24);
        t.spill_over_limit(&mut d).unwrap();
        // (a,0),(a,1) が spill、(a,2) が Tier 1 残留。
        assert!(!d.has_page("a", 0));
        assert!(!d.has_page("a", 1));
        assert!(d.has_page("a", 2));

        t.rehydrate_into(&mut d).unwrap();
        // 全ページが Tier 1 に揃う。
        assert_eq!(d.page("a", 0).unwrap(), &[0x01; 8]);
        assert_eq!(d.page("a", 1).unwrap(), &[0x02; 8]);
        assert_eq!(d.page("a", 2).unwrap(), &[0x03; 8]);
    }
}
