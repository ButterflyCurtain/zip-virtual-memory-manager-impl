//! commit FULL（compaction）パス（設計 commit() FLOW の FULL path）。
//!
//! Diff Layer の dirty ページを反映した**新しい完全な ZIP** をメモリ上に組み立て
//! る。設計の最も単純なクラッシュセーフ書き込み（M2）:
//!
//! - 未変更エントリ → 圧縮ストリームを verbatim でコピー（再圧縮コスト 0）。
//! - 変更エントリ   → Diff ページ + ソースの未変更ページ + ゼロ埋め gap から
//!   論理内容を組み立て直し、元のメソッド（STORE / 標準 DEFLATE）で再圧縮する。
//!
//! 生成物は呼び出し側が `archive.new.zip` に書いて `rename()` する（[`disk`]
//! 層）か、メモリ上のテストで再 open する。バイト列の生成だけを担い、ファイル
//! I/O・rename・vmidx の更新は持たない（設計どおり責務を分離）。
//!
//! 出力 ZIP は正規形の Local File Header / Central Directory ファイルヘッダ
//! （extra field なし、コメントなし）で書き直す。エントリ CRC-32 は ZIP 標準の
//! ISO-HDLC 多項式（zlib `crc32()` 相当。ジャーナルの CRC-32C とは別物
//! ＝IMPLEMENTATION_NOTES の罠）。
//!
//! M2 の制限: Zip64 出力は未対応。いずれかのオフセット・サイズ・件数が 32 ビット
//! に収まらない場合は [`CommitError::TooLarge`] を返す（INCREMENTAL / Dead Space
//! Freelist / journal は M3 以降）。
//!
//! [`disk`]: crate::disk

use crate::archive::{Archive, ZipError};
use crate::difflayer::DiffLayer;
use crate::entrytable::{EntryTable, Kind};
use crate::mount::{read_entry, ReadError};
use crate::page::{page_count, page_extent};
use crate::vmidx::ProviderType;
use libz_rs_sys as z;
use std::collections::HashSet;
use std::fmt;
use std::os::raw::c_int;
use std::sync::OnceLock;

const LFH_SIG: u32 = 0x0403_4b50;
const CDFH_SIG: u32 = 0x0201_4b50;
const EOCD_SIG: u32 = 0x0605_4b50;

/// commit の失敗。
#[derive(Debug)]
pub enum CommitError {
    /// ソース ZIP の parse / エントリ取り出しに失敗した。
    Zip(ZipError),
    /// 変更エントリの未変更ページをソースから読む段で失敗した。
    Read(ReadError),
    /// 再圧縮（DEFLATE）に失敗した。
    Compress(&'static str),
    /// 再圧縮できない圧縮種別（STORE / 標準 DEFLATE 以外）。
    Unsupported(ProviderType),
    /// 出力が 32 ビット ZIP の表現範囲を超える（M2 は Zip64 出力に未対応）。
    TooLarge,
}

impl fmt::Display for CommitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CommitError::Zip(e) => write!(f, "commit: {e}"),
            CommitError::Read(e) => write!(f, "commit: {e}"),
            CommitError::Compress(why) => write!(f, "commit: recompress failed ({why})"),
            CommitError::Unsupported(p) => write!(f, "commit: cannot recompress {p:?}"),
            CommitError::TooLarge => write!(f, "commit: output exceeds 32-bit ZIP limits (zip64 not supported in M2)"),
        }
    }
}

impl std::error::Error for CommitError {}

/// Diff Layer + エントリ表を反映した新しい ZIP バイト列を組み立てる（FULL パス）。
///
/// `archive` はソース ZIP、`vmidx_image` はそれに対応する vmidx 像（変更エントリ
/// の未変更ページをソースから読むのに使う）、`diff` は Tier 1 の dirty 状態、
/// `table` はセッション内の構造変更（create / remove）。実効的なエントリ集合は
/// 「vmidx − tombstone ∪ created」で組み立てる:
///
/// - tombstone → 出力しない。
/// - created（vmidx に同名があれば上書き、無ければ新規）→ Diff から組み立て、
///   既定 DEFLATE で再圧縮して新しい LFH/CD を出す。
/// - それ以外のソースエントリ → dirty なら再圧縮、未変更なら verbatim コピー。
pub fn build_full(
    archive: &[u8],
    vmidx_image: &[u8],
    diff: &DiffLayer,
    table: &EntryTable,
) -> Result<Vec<u8>, CommitError> {
    let ar = Archive::parse(archive).map_err(CommitError::Zip)?;

    let mut body: Vec<u8> = Vec::new();
    // (local_header_offset, method, crc, comp_size, uncomp_size, name) を CD 用に控える。
    let mut placed: Vec<(u64, u16, u32, u64, u64, Vec<u8>)> = Vec::with_capacity(ar.entries().len());
    // vmidx ループで created として出した名前（created ループの重複出力を防ぐ）。
    let mut emitted_created: HashSet<String> = HashSet::new();

    for entry in ar.entries() {
        let name_utf8 = std::str::from_utf8(&entry.name).ok();

        // UTF-8 名のみオーバーレイ対象（非 UTF-8 名は table に入りえない）。
        if let Some(name) = name_utf8 {
            match table.kind(name, true) {
                Kind::Absent => continue, // tombstone → 出力しない
                Kind::Created => {
                    // 同名を再 create（リスタート）→ created として組み立てる。
                    let (method, crc, stored, uncomp) =
                        build_created(archive, vmidx_image, diff, name)?;
                    place_entry(&mut body, &mut placed, method, crc, &stored, uncomp, &entry.name)?;
                    emitted_created.insert(name.to_owned());
                    continue;
                }
                Kind::Source => {}
            }
        }

        // 通常のソースエントリ（Kind::Source または非 UTF-8 名）。
        let dirty_name = name_utf8.filter(|n| diff.is_dirty(n));
        let (method, crc, stored, uncomp_size) = if let Some(name) = dirty_name {
            let logical = diff.logical_size(name).unwrap_or(entry.uncompressed_size);
            // ソース読み出しの上限は source high-water（truncate-shrink で縮む）。
            let original = diff.source_size(name).unwrap_or(entry.uncompressed_size);
            let content =
                assemble_content(archive, vmidx_image, diff, name, logical, original, Some(name))?;
            let crc = crc32(&content);
            let (method, stored) = match entry.provider_type {
                ProviderType::Store => (0u16, content.clone()),
                ProviderType::Deflate => (8u16, deflate(&content)?),
                other => return Err(CommitError::Unsupported(other)),
            };
            let uncomp = content.len() as u64;
            (method, crc, stored, uncomp)
        } else {
            // 未変更: 圧縮ストリームを verbatim でコピー（CRC・サイズは CD から）。
            let stored = ar.entry_data(entry).map_err(CommitError::Zip)?.to_vec();
            (entry.method_code, entry.crc32, stored, entry.uncompressed_size)
        };

        place_entry(&mut body, &mut placed, method, crc, &stored, uncomp_size, &entry.name)?;
    }

    // vmidx に無い created エントリ。
    for name in table.created_names() {
        if emitted_created.contains(name) {
            continue;
        }
        let (method, crc, stored, uncomp) = build_created(archive, vmidx_image, diff, name)?;
        place_entry(&mut body, &mut placed, method, crc, &stored, uncomp, name.as_bytes())?;
    }

    // Central Directory。
    let cd_offset = body.len() as u64;
    let mut cd: Vec<u8> = Vec::new();
    for (lho, method, crc, comp_size, uncomp_size, name) in &placed {
        write_cdfh(&mut cd, *method, *crc, *comp_size, *uncomp_size, *lho, name);
    }
    let cd_size = cd.len() as u64;
    body.extend_from_slice(&cd);

    let count = placed.len();
    if cd_offset > u32::MAX as u64 || cd_size > u32::MAX as u64 || count > u16::MAX as usize {
        return Err(CommitError::TooLarge);
    }

    // End Of Central Directory。
    push_u32(&mut body, EOCD_SIG);
    push_u16(&mut body, 0); // このディスク番号
    push_u16(&mut body, 0); // CD 開始ディスク
    push_u16(&mut body, count as u16); // このディスクのエントリ数
    push_u16(&mut body, count as u16); // 総エントリ数
    push_u32(&mut body, cd_size as u32);
    push_u32(&mut body, cd_offset as u32);
    push_u16(&mut body, 0); // コメント長

    Ok(body)
}

/// エントリの論理内容（長さ `logical`）を組み立てる。各ページは Diff Layer 優先、
/// 無ければソース（`source` = vmidx 名、`None` = created）の未変更ページ、ソース
/// 範囲を超える分とソース無しはゼロ（implicit extension の gap / created の未書き
/// 込み）。`name` は Diff Layer のキー（現在名）。
#[allow(clippy::too_many_arguments)]
fn assemble_content(
    archive: &[u8],
    vmidx_image: &[u8],
    diff: &DiffLayer,
    name: &str,
    logical: u64,
    original_size: u64,
    source: Option<&str>,
) -> Result<Vec<u8>, CommitError> {
    let ps = diff.page_size();
    let mut content = Vec::with_capacity(logical as usize);
    for page in 0..page_count(logical, ps) {
        let (start, len) = page_extent(logical, page, ps);
        if len == 0 {
            continue;
        }
        if let Some(p) = diff.page(name, page) {
            content.extend_from_slice(&p[..len]);
        } else if let Some(src) = source.filter(|_| start < original_size) {
            // 未変更ページ: ソースから読む。論理ページがソース末尾を跨ぐ場合は
            // 残りをゼロで埋める（短い末尾ページ + gap）。
            let avail = ((original_size - start) as usize).min(len);
            let chunk =
                read_entry(archive, vmidx_image, src, start, avail).map_err(CommitError::Read)?;
            content.extend_from_slice(&chunk);
            if avail < len {
                content.resize(content.len() + (len - avail), 0);
            }
        } else {
            content.resize(content.len() + len, 0);
        }
    }
    Ok(content)
}

/// created エントリ `name` を Diff から組み立て、既定 DEFLATE で圧縮する。
/// ソースは無い（未書き込みページはゼロ）。戻り値 (method, crc, stored, uncomp)。
fn build_created(
    archive: &[u8],
    vmidx_image: &[u8],
    diff: &DiffLayer,
    name: &str,
) -> Result<(u16, u32, Vec<u8>, u64), CommitError> {
    let logical = diff.logical_size(name).unwrap_or(0);
    let content = assemble_content(archive, vmidx_image, diff, name, logical, 0, None)?;
    let crc = crc32(&content);
    let stored = deflate(&content)?;
    Ok((8, crc, stored, content.len() as u64))
}

/// 1 エントリを body へ書き（LFH + データ）、CD 用情報を `placed` に積む。
/// 32 ビット ZIP の表現範囲を超えたら [`CommitError::TooLarge`]。
#[allow(clippy::too_many_arguments)]
fn place_entry(
    body: &mut Vec<u8>,
    placed: &mut Vec<(u64, u16, u32, u64, u64, Vec<u8>)>,
    method: u16,
    crc: u32,
    stored: &[u8],
    uncomp_size: u64,
    name: &[u8],
) -> Result<(), CommitError> {
    let comp_size = stored.len() as u64;
    let lho = body.len() as u64;
    if lho > u32::MAX as u64 || comp_size > u32::MAX as u64 || uncomp_size > u32::MAX as u64 {
        return Err(CommitError::TooLarge);
    }
    write_lfh(body, method, crc, comp_size, uncomp_size, name);
    body.extend_from_slice(stored);
    placed.push((lho, method, crc, comp_size, uncomp_size, name.to_vec()));
    Ok(())
}

/// 正規形のローカルファイルヘッダ（extra field なし）を書く。
fn write_lfh(out: &mut Vec<u8>, method: u16, crc: u32, comp: u64, uncomp: u64, name: &[u8]) {
    push_u32(out, LFH_SIG);
    push_u16(out, 20); // version needed
    push_u16(out, 0); // flags
    push_u16(out, method);
    push_u16(out, 0); // mod time
    push_u16(out, 0); // mod date
    push_u32(out, crc);
    push_u32(out, comp as u32);
    push_u32(out, uncomp as u32);
    push_u16(out, name.len() as u16);
    push_u16(out, 0); // extra len
    out.extend_from_slice(name);
}

/// 正規形の Central Directory ファイルヘッダ（extra / comment なし）を書く。
#[allow(clippy::too_many_arguments)]
fn write_cdfh(out: &mut Vec<u8>, method: u16, crc: u32, comp: u64, uncomp: u64, lho: u64, name: &[u8]) {
    push_u32(out, CDFH_SIG);
    push_u16(out, 20); // version made by
    push_u16(out, 20); // version needed
    push_u16(out, 0); // flags
    push_u16(out, method);
    push_u16(out, 0); // mod time
    push_u16(out, 0); // mod date
    push_u32(out, crc);
    push_u32(out, comp as u32);
    push_u32(out, uncomp as u32);
    push_u16(out, name.len() as u16);
    push_u16(out, 0); // extra len
    push_u16(out, 0); // comment len
    push_u16(out, 0); // disk start
    push_u16(out, 0); // internal attrs
    push_u32(out, 0); // external attrs
    push_u32(out, lho as u32);
    out.extend_from_slice(name);
}

#[inline]
fn push_u16(v: &mut Vec<u8>, x: u16) {
    v.extend_from_slice(&x.to_le_bytes());
}

#[inline]
fn push_u32(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_le_bytes());
}

/// 生バイト列を raw DEFLATE（windowBits = -15、ZIP メソッド 8）で圧縮する。
/// `Z_FINISH` で一括圧縮し、`deflateBound` 相当の余裕を取った 1 バッファに収める。
fn deflate(data: &[u8]) -> Result<Vec<u8>, CommitError> {
    let mut strm = z::z_stream::default();
    // deflateBound 相当の上限（壊滅的非圧縮でも収まる余裕）。
    let cap = data.len() + data.len() / 2 + 1024;
    let mut out = vec![0u8; cap];
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
        if r != z::Z_OK {
            return Err(CommitError::Compress("deflateInit2 failed"));
        }
        strm.next_in = data.as_ptr();
        strm.avail_in = data.len() as _;
        strm.next_out = out.as_mut_ptr();
        strm.avail_out = out.len() as _;
        let r = z::deflate(&mut strm, z::Z_FINISH);
        if r != z::Z_STREAM_END {
            z::deflateEnd(&mut strm);
            return Err(CommitError::Compress("deflate did not finish in one shot"));
        }
        let produced = out.len() - strm.avail_out as usize;
        out.truncate(produced);
        z::deflateEnd(&mut strm);
    }
    Ok(out)
}

/// ZIP 標準のエントリ CRC-32（ISO-HDLC、反転多項式 0xEDB88320）。zlib の
/// `crc32()` と一致する。**ジャーナル / vmidx の CRC-32C（Castagnoli）とは別物**
/// なので混同しないこと（IMPLEMENTATION_NOTES）。
fn crc32(data: &[u8]) -> u32 {
    let table = crc_table();
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc = table[((crc ^ b as u32) & 0xff) as usize] ^ (crc >> 8);
    }
    !crc
}

fn crc_table() -> &'static [u32; 256] {
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        let mut n = 0usize;
        while n < 256 {
            let mut c = n as u32;
            let mut k = 0;
            while k < 8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
                k += 1;
            }
            t[n] = c;
            n += 1;
        }
        t
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_known_vector() {
        // "123456789" の CRC-32 (ISO-HDLC) は 0xCBF43926（標準チェック値）。
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn deflate_round_trips_via_inflate() {
        let data: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        let comp = deflate(&data).unwrap();
        // libz-rs-sys で raw inflate して戻す。
        let mut strm = z::z_stream::default();
        let mut out = vec![0u8; data.len()];
        unsafe {
            let r = z::inflateInit2_(
                &mut strm,
                -15,
                z::zlibVersion(),
                core::mem::size_of::<z::z_stream>() as c_int,
            );
            assert_eq!(r, z::Z_OK);
            strm.next_in = comp.as_ptr();
            strm.avail_in = comp.len() as _;
            strm.next_out = out.as_mut_ptr();
            strm.avail_out = out.len() as _;
            let r = z::inflate(&mut strm, z::Z_FINISH);
            assert_eq!(r, z::Z_STREAM_END);
            z::inflateEnd(&mut strm);
        }
        assert_eq!(out, data);
    }

    #[test]
    fn deflate_handles_empty_input() {
        let comp = deflate(b"").unwrap();
        assert!(!comp.is_empty(), "empty deflate stream still has a terminator");
    }
}
