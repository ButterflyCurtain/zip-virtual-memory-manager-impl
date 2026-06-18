//! マウント / 読み取り経路（設計 READ PATH と FIRST-OPEN の最小核）。
//!
//! [`Mount`] は archive.zip のバイト列（呼び出し側が mmap 済み）と、それに対応
//! する vmidx 像、[`PageCache`] を束ね、`read(path, offset, len)` を提供する。
//! 設計のレイヤのうち Diff Layer はまだ無く、読み取りは「ページキャッシュ →
//! シーク索引 + ソース ZIP」の 2 段:
//!
//! - [`read_cached`]: 要求範囲の各ページをキャッシュから取り、ミスしたら
//!   [`fill_run`] で目標ページ + read-ahead ページ分をまとめて展開・充填する。
//! - [`fill_run`]（キャッシュミス時）:
//!   1. vmidx を `lookup(path)` してエントリを引く（呼び出し側で済ませて渡す）
//!   2. `provider_type` から [`provider`](crate::provider) を選ぶ
//!   3. 目標ページから連続する未常駐ページのランを決める（read-ahead）
//!   4. `nearest_checkpoint(record, run_start)` を起点に `provider.read_range` で
//!      ラン全体を 1 回で展開し、ページに切ってキャッシュへ入れる
//!
//! ラン一括展開により、read-ahead は「目標 + N ページ」を 1 回の checkpoint 復元
//! + 前進デコードで賄う（設計 READ PATH の read-ahead 償却。デコーダ状態を跨いで
//! 持ち回る最適化はさらに後段）。[`read_entry`] は索引だけを使う無キャッシュの
//! 下位プリミティブとして残す。
//!
//! vmidx 像は所有し（`Vec<u8>`）、`read` のたびに [`Vmidx::parse`] で軽量ビューを
//! 作る（自己参照構造を避けるため。parse はヘッダ 128 バイトの decode と領域境界
//! 検査のみで安価）。[`Mount`] はページキャッシュを `RefCell` で内部可変に持つ
//! ため、`read(&self)` のまま使えるが `Sync` ではない（並行アクセスは lock 層
//! ＝後段の担当）。

use crate::archive::{Archive, ZipError};
use crate::index_build::{build_vmidx_eager, BuildError, BuildParams};
use crate::page::{page_count, page_extent, PageCache, PageConfig, PageKey};
use crate::provider::{builtin_provider, check_range, ProviderError};
use crate::vmidx::{
    hash_cd_block, DecodeError, EntryRecord, FingerprintVerdict, ProviderType, SourceStat, Vmidx,
};
use std::cell::RefCell;
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
    cfg: PageConfig,
    cache: RefCell<PageCache>,
}

impl<'a> Mount<'a> {
    /// archive バイト列から EAGER 索引を構築して開く（vmidx が無い「コールド
    /// オープン」相当）。`params` の stat 値が fingerprint に入る。ページ設定は
    /// 既定（[`PageConfig::default`]）。
    pub fn open(archive: &'a [u8], params: &BuildParams) -> Result<Mount<'a>, OpenError> {
        Mount::open_with_page_config(archive, params, PageConfig::default())
    }

    /// [`Mount::open`] にページ設定を指定する版。
    pub fn open_with_page_config(
        archive: &'a [u8],
        params: &BuildParams,
        cfg: PageConfig,
    ) -> Result<Mount<'a>, OpenError> {
        let (vmidx_image, _) = resolve_index(archive, None, params)?;
        Ok(Mount::assemble(archive, vmidx_image, cfg))
    }

    /// 既存の vmidx 像を検証して開く（設計 Section 7 の open() カスケード）。
    /// `Valid` / `ValidStale` ならその像を使い、`Invalid` または parse 失敗なら
    /// EAGER で再構築する（「どの失敗でも応答は破棄して再構築」）。
    pub fn open_with_index(
        archive: &'a [u8],
        vmidx_image: Vec<u8>,
        params: &BuildParams,
    ) -> Result<Mount<'a>, OpenError> {
        let (vmidx_image, _) = resolve_index(archive, Some(vmidx_image), params)?;
        Ok(Mount::assemble(archive, vmidx_image, PageConfig::default()))
    }

    fn assemble(archive: &'a [u8], vmidx_image: Vec<u8>, cfg: PageConfig) -> Mount<'a> {
        let cache = RefCell::new(PageCache::from_config(&cfg));
        Mount {
            archive,
            vmidx_image,
            cfg,
            cache,
        }
    }

    /// 構築済みの vmidx 像（ファイル I/O 層が vmidx.tmp として書き出す対象）。
    pub fn index_bytes(&self) -> &[u8] {
        &self.vmidx_image
    }

    /// エントリ `path` の展開ストリーム `[offset, offset + len)` を読む。
    /// ページキャッシュ経由（ミス時のみ展開 + read-ahead 充填）。
    pub fn read(&self, path: &str, offset: u64, len: usize) -> Result<Vec<u8>, ReadError> {
        let mut cache = self.cache.borrow_mut();
        read_cached(
            self.archive,
            &self.vmidx_image,
            &mut cache,
            &self.cfg,
            path,
            offset,
            len,
        )
    }
}

/// open カスケード本体（設計 Section 7）。既存 vmidx 像があれば parse +
/// fingerprint を照合し、使えるならそのまま、`Invalid` / parse 失敗なら EAGER で
/// 再構築する。戻り値は (採用する像, 再構築したか)。ファイル I/O 層は再構築フラグ
/// を見て vmidx.tmp 書き出しの要否を決める。
pub fn resolve_index(
    archive: &[u8],
    existing: Option<Vec<u8>>,
    params: &BuildParams,
) -> Result<(Vec<u8>, bool), OpenError> {
    let ar = Archive::parse(archive).map_err(OpenError::Zip)?;
    if let Some(bytes) = existing {
        let live = SourceStat {
            file_size: params.source_file_size,
            inode: params.source_inode,
            mtime_ns: params.source_mtime_ns,
            cd_hash: hash_cd_block(ar.cd_block()),
        };
        let usable = match Vmidx::parse(&bytes) {
            Ok(v) => !matches!(v.check_fingerprint(&live), FingerprintVerdict::Invalid),
            Err(_) => false,
        };
        if usable {
            return Ok((bytes, false));
        }
    }
    let rebuilt = build_vmidx_eager(&ar, params).map_err(OpenError::Build)?;
    Ok((rebuilt, true))
}

/// vmidx 像とソース ZIP バイト列から 1 エントリの `[offset, offset + len)` を読む。
/// `archive` と `vmidx_image` はそれぞれ呼び出し側が所有（mmap / Vec）し、ここでは
/// 借用するだけ。
pub fn read_entry(
    archive: &[u8],
    vmidx_image: &[u8],
    path: &str,
    offset: u64,
    len: usize,
) -> Result<Vec<u8>, ReadError> {
    let vmidx = Vmidx::parse(vmidx_image).map_err(ReadError::Vmidx)?;
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
        .filter(|&e| e <= archive.len())
        .ok_or(ReadError::DataOutOfRange)?;
    let compressed = &archive[start..end];

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

/// ページキャッシュ経由で 1 エントリの `[offset, offset + len)` を読む。
/// 要求範囲がまたぐ各ページについて、常駐していればキャッシュから、ミスなら
/// [`fill_run`] で目標ページ + read-ahead 分を展開・充填してから取り出す。
/// `cache` の `page_size` がページ境界を決める。
pub fn read_cached(
    archive: &[u8],
    vmidx_image: &[u8],
    cache: &mut PageCache,
    cfg: &PageConfig,
    path: &str,
    offset: u64,
    len: usize,
) -> Result<Vec<u8>, ReadError> {
    let vmidx = Vmidx::parse(vmidx_image).map_err(ReadError::Vmidx)?;
    let (entry, record) = vmidx
        .lookup(path)
        .map_err(ReadError::Vmidx)?
        .ok_or(ReadError::NotFound)?;

    // 範囲検査はプロバイダと同じ判定（エントリサイズ超は OutOfRange）。
    check_range(offset, len, record.uncompressed_size).map_err(ReadError::Provider)?;
    if len == 0 {
        return Ok(Vec::new());
    }

    let page_size = cache.page_size();
    let total_pages = page_count(record.uncompressed_size, page_size);
    let end = offset + len as u64;

    let mut out = Vec::with_capacity(len);
    let mut pos = offset;
    while pos < end {
        let page = pos / page_size;
        let key = PageKey { entry, page };
        let page_start = page * page_size;
        let in_page = (pos - page_start) as usize;

        if let Some(data) = cache.get(key) {
            // ヒット。末尾ページは短く、data.len() が当該ページの実長。
            let take = (data.len() - in_page).min((end - pos) as usize);
            out.extend_from_slice(&data[in_page..in_page + take]);
            pos += take as u64;
            continue;
        }

        // ミス（直前の get が None を返した時点で計上済み）。目標ページ +
        // read-ahead 分を 1 回で展開し（ラン）、キャッシュへ充填する。当該ページは
        // ランの先頭なので、退避方針に依らず確実に取れる戻り値バイト列から直接
        // 切り出す（ラン > キャッシュ容量でも前進できる）。
        let (bytes, run_start) =
            fill_run(archive, &vmidx, cache, cfg, &record, entry, page, total_pages)?;
        debug_assert_eq!(run_start, page_start);
        let page_len = page_extent(record.uncompressed_size, page, page_size).1;
        let take = (page_len - in_page).min((end - pos) as usize);
        out.extend_from_slice(&bytes[in_page..in_page + take]);
        pos += take as u64;
    }
    Ok(out)
}

/// 目標ページ `target_page` を含む「連続する未常駐ページのラン」を 1 回の
/// `read_range` で展開し、ページに切ってキャッシュへ入れる。戻り値は
/// (ラン展開バイト列, ラン先頭の展開オフセット = `target_page * page_size`)。
/// ランは `[target_page, last]` で、`last` は read-ahead 上限
/// （`cfg.read_ahead_pages`）かエントリ末尾、あるいは最初に常駐していた先読み
/// ページの手前で止まる（設計 READ PATH: read-ahead はエントリ境界で止まり、
/// 常駐ページは再展開しない）。
///
/// キャッシュへの充填と独立に戻り値でランを返すのは、ランがキャッシュ容量を
/// 超えるとき目標ページ自身が充填中に退避されうるため。呼び出し側は戻り値から
/// 目標ページを取り出し、キャッシュは将来のヒット用に充填する。
#[allow(clippy::too_many_arguments)]
fn fill_run(
    archive: &[u8],
    vmidx: &Vmidx,
    cache: &mut PageCache,
    cfg: &PageConfig,
    record: &EntryRecord,
    entry: usize,
    target_page: u64,
    total_pages: u64,
) -> Result<(Vec<u8>, u64), ReadError> {
    let provider = builtin_provider(record.provider_type)
        .ok_or(ReadError::Unsupported(record.provider_type))?;
    let page_size = cache.page_size();

    // 先読みランの末尾を決める（目標ページ自身は呼び出し側でミス確定）。
    let read_ahead = cfg.read_ahead_pages as u64;
    let last_limit = target_page
        .saturating_add(read_ahead)
        .min(total_pages - 1);
    let mut last = target_page;
    while last < last_limit && !cache.contains(PageKey { entry, page: last + 1 }) {
        last += 1;
    }

    // ラン全域を 1 回で展開する（checkpoint 復元 + 前進デコードを read-ahead 分で
    // 償却）。
    let (run_start, _) = page_extent(record.uncompressed_size, target_page, page_size);
    let (last_start, last_len) = page_extent(record.uncompressed_size, last, page_size);
    let run_len = (last_start - run_start) as usize + last_len;

    let nearest = vmidx
        .nearest_checkpoint(record, run_start)
        .map_err(ReadError::Vmidx)?;
    let start = record.data_offset as usize;
    let aend = start
        .checked_add(record.compressed_size as usize)
        .filter(|&e| e <= archive.len())
        .ok_or(ReadError::DataOutOfRange)?;
    let compressed = &archive[start..aend];

    let bytes = provider
        .read_range(
            compressed,
            nearest.as_ref(),
            run_start,
            run_len,
            record.uncompressed_size,
        )
        .map_err(ReadError::Provider)?;

    // ランをページに切って充填する。
    for page in target_page..=last {
        let (ps, plen) = page_extent(record.uncompressed_size, page, page_size);
        let from = (ps - run_start) as usize;
        cache.insert(PageKey { entry, page }, bytes[from..from + plen].to_vec());
    }
    Ok((bytes, run_start))
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

    /// `big.bin` 1 エントリ（DEFLATE）だけの ZIP を組む共通フィクスチャ。
    fn deflate_zip(data: &[u8]) -> Vec<u8> {
        let comp = raw_deflate(data);
        let mut zb = ZipBuilder::new();
        zb.add("big.bin", 8, &comp, data.len() as u32);
        zb.finish()
    }

    #[test]
    fn read_ahead_populates_neighbor_pages() {
        let data = sample_data(180_000);
        let cfg = PageConfig {
            page_size: 4096,
            read_ahead_pages: 8,
            cache_bytes: 16 << 20,
            ..PageConfig::default()
        };
        let zip = deflate_zip(&data);
        let params = BuildParams {
            checkpoint_interval: 16 * 1024,
            ..BuildParams::default()
        };
        let mount = Mount::open_with_page_config(&zip, &params, cfg).expect("open");

        // ページ 0 の途中を 1 バイト読むだけで、ページ 0..=8 が充填される。
        let got = mount.read("big.bin", 100, 1).unwrap();
        assert_eq!(got, &data[100..101]);
        {
            let cache = mount.cache.borrow();
            assert_eq!(cache.len(), 9, "target + 8 read-ahead pages");
            for p in 0..=8u64 {
                assert!(cache.contains(PageKey { entry: 0, page: p }), "page {p}");
            }
            assert!(!cache.contains(PageKey { entry: 0, page: 9 }));
            assert_eq!(cache.misses(), 1, "single miss drove the whole run");
        }

        // 先読み済みページ（ページ 5）の読みはミスを増やさない（ヒット）。
        let off = 5 * 4096 + 17;
        let got = mount.read("big.bin", off as u64, 50).unwrap();
        assert_eq!(got, &data[off..off + 50]);
        assert_eq!(mount.cache.borrow().misses(), 1, "served from cache");
    }

    #[test]
    fn read_ahead_disabled_loads_only_target_page() {
        let data = sample_data(50_000);
        let cfg = PageConfig {
            page_size: 4096,
            read_ahead_pages: 0,
            cache_bytes: 16 << 20,
            ..PageConfig::default()
        };
        let zip = deflate_zip(&data);
        let params = BuildParams {
            checkpoint_interval: 16 * 1024,
            ..BuildParams::default()
        };
        let mount = Mount::open_with_page_config(&zip, &params, cfg).expect("open");
        mount.read("big.bin", 0, 1).unwrap();
        assert_eq!(mount.cache.borrow().len(), 1);
    }

    #[test]
    fn multi_page_read_spans_pages_and_short_tail() {
        // 末尾が短いページになるサイズ（4096*N にしない）。
        let data = sample_data(10_000); // 4096,4096,1808 の 3 ページ
        let cfg = PageConfig {
            page_size: 4096,
            read_ahead_pages: 8,
            cache_bytes: 16 << 20,
            ..PageConfig::default()
        };
        let zip = deflate_zip(&data);
        let params = BuildParams::default();
        let mount = Mount::open_with_page_config(&zip, &params, cfg).expect("open");

        // 3 ページ全域を 1 回で（複数ページ跨ぎ + 短い末尾）。
        assert_eq!(mount.read("big.bin", 0, data.len()).unwrap(), data);
        // ページ境界をまたぐ部分読み。
        assert_eq!(
            mount.read("big.bin", 4090, 20).unwrap(),
            &data[4090..4110]
        );
        // 末尾ぴったり（短い末尾ページ）。
        assert_eq!(
            mount.read("big.bin", 9_990, 10).unwrap(),
            &data[9_990..10_000]
        );
        // 末尾超過は OutOfRange。
        assert!(matches!(
            mount.read("big.bin", 9_990, 20),
            Err(ReadError::Provider(ProviderError::OutOfRange { .. }))
        ));
    }

    #[test]
    fn small_cache_evicts_but_stays_correct() {
        let data = sample_data(120_000);
        // ページ 2 枚分しか持てないキャッシュ。read-ahead 分は退避される。
        let cfg = PageConfig {
            page_size: 4096,
            read_ahead_pages: 8,
            cache_bytes: 2 * 4096,
            ..PageConfig::default()
        };
        let zip = deflate_zip(&data);
        let params = BuildParams {
            checkpoint_interval: 16 * 1024,
            ..BuildParams::default()
        };
        let mount = Mount::open_with_page_config(&zip, &params, cfg).expect("open");

        // あちこち読んでも常に原データと一致し、常駐は 2 ページに収まる。
        for &off in &[0u64, 70_000, 5_000, 119_000, 40_000, 100] {
            let len = ((data.len() as u64 - off).min(900)) as usize;
            let got = mount.read("big.bin", off, len).unwrap();
            assert_eq!(got, &data[off as usize..off as usize + len], "at {off}");
            assert!(mount.cache.borrow().len() <= 2, "cache bounded");
        }
    }

    #[test]
    fn store_entry_reads_through_cache() {
        let store = b"hello, stored world, with several pages worth of bytes here.";
        let mut zb = ZipBuilder::new();
        zb.add("notes.txt", 0, store, store.len() as u32);
        let zip = zb.finish();
        let cfg = PageConfig {
            page_size: 16,
            read_ahead_pages: 2,
            cache_bytes: 16 << 20,
            ..PageConfig::default()
        };
        let mount = Mount::open_with_page_config(&zip, &BuildParams::default(), cfg).expect("open");
        assert_eq!(mount.read("notes.txt", 0, store.len()).unwrap(), store);
        assert_eq!(mount.read("notes.txt", 7, 6).unwrap(), b"stored");
        // STORE もページキャッシュ経由（隣接ページが充填される）。
        assert!(mount.cache.borrow().len() >= 2);
    }
}
