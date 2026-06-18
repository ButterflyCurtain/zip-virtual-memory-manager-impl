//! マウント / 読み取り経路（設計 READ PATH と FIRST-OPEN の最小核）。
//!
//! [`Mount`] は archive.zip のバイト列（呼び出し側が mmap 済み）と、それに対応
//! する vmidx 像を束ね、`read(path, offset, len)` を提供する。設計のレイヤのうち
//! Diff Layer / Page Cache はまだ無く、シーク索引 + ソース ZIP の経路だけを
//! 繋ぐ:
//!
//! 1. vmidx を `lookup(path)` してエントリを引く
//! 2. `provider_type` から [`provider`](crate::provider) を選ぶ
//! 3. `nearest_checkpoint(record, offset)` を引く（無ければ先頭から）
//! 4. レコードの `data_offset` / `compressed_size` でソース ZIP の圧縮バイト列を
//!    切り出し、`provider.read_range` で展開オフセット範囲を得る
//!
//! vmidx 像は所有し（`Vec<u8>`）、`read` のたびに [`Vmidx::parse`] で軽量ビューを
//! 作る（自己参照構造を避けるため。parse はヘッダ 128 バイトの decode と領域境界
//! 検査のみで安価）。

use crate::archive::{Archive, ZipError};
use crate::index_build::{build_vmidx_eager, BuildError, BuildParams};
use crate::provider::{builtin_provider, ProviderError};
use crate::vmidx::{
    hash_cd_block, DecodeError, FingerprintVerdict, ProviderType, SourceStat, Vmidx,
};
use std::fmt;

/// マウントを開く際の失敗。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenError {
    /// ソース ZIP の parse に失敗した。
    Zip(ZipError),
    /// vmidx 像の構築に失敗した。
    Build(BuildError),
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OpenError::Zip(e) => write!(f, "mount open: {e}"),
            OpenError::Build(e) => write!(f, "mount open: {e}"),
        }
    }
}

impl std::error::Error for OpenError {}

/// `read()` の失敗。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    /// 指定の `path` がアーカイブに無い。
    NotFound,
    /// プロバイダを持たない圧縮種別（未対応メソッド）。
    Unsupported(ProviderType),
    /// vmidx の decode 失敗（破損 → 本来は再構築の合図）。
    Vmidx(DecodeError),
    /// プロバイダの解凍失敗。
    Provider(ProviderError),
    /// レコードの `data_offset` / `compressed_size` がソース ZIP の範囲外。
    DataOutOfRange,
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadError::NotFound => write!(f, "read: entry not found"),
            ReadError::Unsupported(p) => write!(f, "read: unsupported provider {p:?}"),
            ReadError::Vmidx(e) => write!(f, "read: {e}"),
            ReadError::Provider(e) => write!(f, "read: {e}"),
            ReadError::DataOutOfRange => write!(f, "read: entry data outside archive"),
        }
    }
}

impl std::error::Error for ReadError {}

/// 1 つのアーカイブに対するマウント。読み取り専用。
pub struct Mount<'a> {
    archive: &'a [u8],
    vmidx_image: Vec<u8>,
}

impl<'a> Mount<'a> {
    /// archive バイト列から EAGER 索引を構築して開く（vmidx が無い「コールド
    /// オープン」相当）。`params` の stat 値が fingerprint に入る。
    pub fn open(archive: &'a [u8], params: &BuildParams) -> Result<Mount<'a>, OpenError> {
        let ar = Archive::parse(archive).map_err(OpenError::Zip)?;
        let vmidx_image = build_vmidx_eager(&ar, params).map_err(OpenError::Build)?;
        Ok(Mount {
            archive,
            vmidx_image,
        })
    }

    /// 既存の vmidx 像を検証して開く（設計 Section 7 の open() カスケード）。
    /// 構造（parse）と fingerprint を照合し、`Valid` / `ValidStale` ならその像を
    /// 使い、`Invalid` または parse 失敗なら EAGER で再構築する（「どの失敗でも
    /// 応答は破棄して再構築」）。
    pub fn open_with_index(
        archive: &'a [u8],
        vmidx_image: Vec<u8>,
        params: &BuildParams,
    ) -> Result<Mount<'a>, OpenError> {
        let ar = Archive::parse(archive).map_err(OpenError::Zip)?;
        let live = SourceStat {
            file_size: params.source_file_size,
            inode: params.source_inode,
            mtime_ns: params.source_mtime_ns,
            cd_hash: hash_cd_block(ar.cd_block()),
        };
        let usable = match Vmidx::parse(&vmidx_image) {
            Ok(v) => !matches!(v.check_fingerprint(&live), FingerprintVerdict::Invalid),
            Err(_) => false,
        };
        if usable {
            return Ok(Mount {
                archive,
                vmidx_image,
            });
        }
        let rebuilt = build_vmidx_eager(&ar, params).map_err(OpenError::Build)?;
        Ok(Mount {
            archive,
            vmidx_image: rebuilt,
        })
    }

    /// 構築済みの vmidx 像（ファイル I/O 層が vmidx.tmp として書き出す対象）。
    pub fn index_bytes(&self) -> &[u8] {
        &self.vmidx_image
    }

    /// エントリ `path` の展開ストリーム `[offset, offset + len)` を読む。
    pub fn read(&self, path: &str, offset: u64, len: usize) -> Result<Vec<u8>, ReadError> {
        let vmidx = Vmidx::parse(&self.vmidx_image).map_err(ReadError::Vmidx)?;
        let (_, record) = vmidx
            .lookup(path)
            .map_err(ReadError::Vmidx)?
            .ok_or(ReadError::NotFound)?;
        let provider =
            builtin_provider(record.provider_type).ok_or(ReadError::Unsupported(record.provider_type))?;
        let nearest = vmidx
            .nearest_checkpoint(&record, offset)
            .map_err(ReadError::Vmidx)?;

        let start = record.data_offset as usize;
        let end = start
            .checked_add(record.compressed_size as usize)
            .filter(|&e| e <= self.archive.len())
            .ok_or(ReadError::DataOutOfRange)?;
        let compressed = &self.archive[start..end];

        provider
            .read_range(
                compressed,
                nearest.as_ref(),
                offset,
                len,
                record.uncompressed_size,
            )
            .map_err(ReadError::Provider)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libz_rs_sys as z;
    use std::os::raw::c_int;

    /// STORE / DEFLATE 混在の最小 ZIP を手組みするフィクスチャ。
    struct ZipBuilder {
        body: Vec<u8>,
        cd: Vec<u8>,
        count: u16,
    }

    impl ZipBuilder {
        fn new() -> ZipBuilder {
            ZipBuilder {
                body: Vec::new(),
                cd: Vec::new(),
                count: 0,
            }
        }

        /// 1 エントリ追加。`stored` は LFH/CD に書く実データ（STORE なら生、
        /// DEFLATE なら圧縮済み）、`uncompressed_size` は展開後サイズ。
        fn add(&mut self, name: &str, method: u16, stored: &[u8], uncompressed_size: u32) {
            let lho = self.body.len() as u32;
            let nb = name.as_bytes();
            push_u32(&mut self.body, 0x0403_4b50); // LFH
            push_u16(&mut self.body, 20);
            push_u16(&mut self.body, 0);
            push_u16(&mut self.body, method);
            push_u16(&mut self.body, 0);
            push_u16(&mut self.body, 0);
            push_u32(&mut self.body, 0); // crc（読み取り経路では未検証）
            push_u32(&mut self.body, stored.len() as u32);
            push_u32(&mut self.body, uncompressed_size);
            push_u16(&mut self.body, nb.len() as u16);
            push_u16(&mut self.body, 0);
            self.body.extend_from_slice(nb);
            self.body.extend_from_slice(stored);

            push_u32(&mut self.cd, 0x0201_4b50); // CDFH
            push_u16(&mut self.cd, 20);
            push_u16(&mut self.cd, 20);
            push_u16(&mut self.cd, 0);
            push_u16(&mut self.cd, method);
            push_u16(&mut self.cd, 0);
            push_u16(&mut self.cd, 0);
            push_u32(&mut self.cd, 0);
            push_u32(&mut self.cd, stored.len() as u32);
            push_u32(&mut self.cd, uncompressed_size);
            push_u16(&mut self.cd, nb.len() as u16);
            push_u16(&mut self.cd, 0);
            push_u16(&mut self.cd, 0);
            push_u16(&mut self.cd, 0);
            push_u16(&mut self.cd, 0);
            push_u32(&mut self.cd, 0);
            push_u32(&mut self.cd, lho);
            self.cd.extend_from_slice(nb);
            self.count += 1;
        }

        fn finish(mut self) -> Vec<u8> {
            let cd_offset = self.body.len() as u32;
            let cd_size = self.cd.len() as u32;
            self.body.extend_from_slice(&self.cd);
            push_u32(&mut self.body, 0x0605_4b50);
            push_u16(&mut self.body, 0);
            push_u16(&mut self.body, 0);
            push_u16(&mut self.body, self.count);
            push_u16(&mut self.body, self.count);
            push_u32(&mut self.body, cd_size);
            push_u32(&mut self.body, cd_offset);
            push_u16(&mut self.body, 0);
            self.body
        }
    }

    fn push_u16(v: &mut Vec<u8>, x: u16) {
        v.extend_from_slice(&x.to_le_bytes());
    }
    fn push_u32(v: &mut Vec<u8>, x: u32) {
        v.extend_from_slice(&x.to_le_bytes());
    }

    fn raw_deflate(data: &[u8]) -> Vec<u8> {
        let mut strm = z::z_stream::default();
        let mut out = vec![0u8; data.len() + data.len() / 2 + 1024];
        unsafe {
            let r = z::deflateInit2_(
                &mut strm,
                6,
                z::Z_DEFLATED,
                -15,
                8,
                z::Z_DEFAULT_STRATEGY,
                z::zlibVersion(),
                core::mem::size_of::<z::z_stream>() as c_int,
            );
            assert_eq!(r, z::Z_OK);
            strm.next_in = data.as_ptr();
            strm.avail_in = data.len() as _;
            strm.next_out = out.as_mut_ptr();
            strm.avail_out = out.len() as _;
            assert_eq!(z::deflate(&mut strm, z::Z_FINISH), z::Z_STREAM_END);
            let produced = out.len() - strm.avail_out as usize;
            out.truncate(produced);
            z::deflateEnd(&mut strm);
        }
        out
    }

    fn sample_data(n: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(n);
        let mut state: u32 = 0x9e37_79b9;
        for i in 0..n {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            v.push(((state >> 24) as u8 & 0x1f) ^ (i as u8 & 0x07));
        }
        v
    }

    #[test]
    fn reads_store_and_deflate_entries() {
        let store_data = b"hello, stored world";
        let deflate_data = sample_data(180_000);
        let comp = raw_deflate(&deflate_data);

        let mut zb = ZipBuilder::new();
        zb.add("notes.txt", 0, store_data, store_data.len() as u32);
        zb.add("big.bin", 8, &comp, deflate_data.len() as u32);
        let zip = zb.finish();

        let params = BuildParams {
            checkpoint_interval: 16 * 1024,
            ..BuildParams::default()
        };
        let mount = Mount::open(&zip, &params).expect("open");

        // STORE: 全域・部分域。
        assert_eq!(
            mount.read("notes.txt", 0, store_data.len()).unwrap(),
            store_data
        );
        assert_eq!(mount.read("notes.txt", 7, 6).unwrap(), b"stored");

        // DEFLATE: 先頭・深いオフセット（チェックポイント経由のシーク）。
        assert_eq!(mount.read("big.bin", 0, 100).unwrap(), &deflate_data[..100]);
        for &off in &[10_000u64, 90_000, 179_000] {
            let len = ((deflate_data.len() as u64 - off).min(777)) as usize;
            let got = mount.read("big.bin", off, len).unwrap();
            assert_eq!(
                got,
                &deflate_data[off as usize..off as usize + len],
                "deflate mismatch at {off}"
            );
        }
    }

    #[test]
    fn deflate_entry_has_checkpoints_in_index() {
        // EAGER 索引が DEFLATE エントリにチェックポイントを生成していることを、
        // ビューから直接確かめる（深いオフセットの読みが先頭からでなく
        // チェックポイントから復元される根拠）。
        let data = sample_data(180_000);
        let comp = raw_deflate(&data);
        let mut zb = ZipBuilder::new();
        zb.add("big.bin", 8, &comp, data.len() as u32);
        let zip = zb.finish();
        let params = BuildParams {
            checkpoint_interval: 16 * 1024,
            ..BuildParams::default()
        };
        let mount = Mount::open(&zip, &params).expect("open");
        let vmidx = Vmidx::parse(mount.index_bytes()).unwrap();
        let (_, rec) = vmidx.lookup("big.bin").unwrap().unwrap();
        assert!(rec.checkpoint_count > 0, "expected eager checkpoints");
        // target=90000 で uncompressed_offset>0 の最近 CP が引けること。
        let cp = vmidx.nearest_checkpoint(&rec, 90_000).unwrap();
        assert!(cp.is_some_and(|c| c.uncompressed_offset() > 0));
    }

    #[test]
    fn missing_entry_is_not_found() {
        let mut zb = ZipBuilder::new();
        zb.add("a.txt", 0, b"x", 1);
        let zip = zb.finish();
        let mount = Mount::open(&zip, &BuildParams::default()).expect("open");
        assert_eq!(mount.read("absent", 0, 1), Err(ReadError::NotFound));
    }

    #[test]
    fn open_with_valid_index_reuses_it() {
        let mut zb = ZipBuilder::new();
        zb.add("a.txt", 0, b"abcdef", 6);
        let zip = zb.finish();
        let params = BuildParams {
            source_file_size: 4096,
            source_inode: 5,
            source_mtime_ns: 9,
            ..BuildParams::default()
        };
        // 先に EAGER で作った像を、検証つきで開き直す → 再構築せず使える。
        let image = Mount::open(&zip, &params).unwrap().index_bytes().to_vec();
        let mount = Mount::open_with_index(&zip, image.clone(), &params).expect("open_with_index");
        assert_eq!(mount.read("a.txt", 1, 4).unwrap(), b"bcde");
        // 像はそのまま使われている（再構築なら別バイト列になりうる）。
        assert_eq!(mount.index_bytes(), image.as_slice());
    }

    #[test]
    fn open_with_stale_index_rebuilds() {
        let mut zb = ZipBuilder::new();
        zb.add("a.txt", 0, b"abcdef", 6);
        let zip = zb.finish();
        let params = BuildParams::default();
        // 別アーカイブから作った（cd_hash 不一致＝Invalid な）像を渡す。
        let mut other = ZipBuilder::new();
        other.add("zzz.bin", 0, b"different", 9);
        let other_zip = other.finish();
        let stale = Mount::open(&other_zip, &params).unwrap().index_bytes().to_vec();

        let mount = Mount::open_with_index(&zip, stale, &params).expect("rebuild");
        // 正しいアーカイブの内容が読める＝再構築された。
        assert_eq!(mount.read("a.txt", 0, 6).unwrap(), b"abcdef");
        assert_eq!(mount.read("zzz.bin", 0, 1), Err(ReadError::NotFound));
    }
}
