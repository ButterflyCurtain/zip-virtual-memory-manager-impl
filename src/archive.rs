//! ZIP アーカイブ I/O レイヤ。
//!
//! Central Directory の読み取り、エントリのローカルヘッダ解決、
//! 圧縮データ先頭オフセットの算出を担当する。アーカイブは呼び出し側が
//! mmap した read-only バイトスライスとして渡される（vmidx と同じ方針、
//! 依存に memmap2 を持ち込まない）。
//!
//! 現状の実装範囲:
//! - [`Archive::parse`]: End Of Central Directory の探索と Central Directory
//!   ファイルヘッダ群の parse（標準 ZIP。Zip64 は未対応で
//!   [`ZipError::Unsupported`]）
//! - [`Archive::cd_block`]: fingerprint 用の Central Directory バイト列の特定
//!   （`vmidx::hash_cd_block` に渡す対象）
//! - [`Archive::data_offset`] / [`Archive::entry_data`]: ローカルヘッダを読んで
//!   圧縮データ先頭オフセットを確定し、圧縮データバイト列を取り出す
//! - Zip64 対応: EOCD の番兵を検出したら Zip64 EOCD ロケータ/レコードを辿り、
//!   CD エントリの実サイズ・オフセットは Zip64 extra field（ID 0x0001）から読む
//!
//! 全整数はリトルエンディアン。設計: docs `ZIP_Virtual_Memory_Manager`
//! および `..._Diff_Layer_Pressure_Integrity_Detection`（cd_hash の定義）。

use crate::vmidx::ProviderType;
use std::fmt;

/// End Of Central Directory レコードのシグネチャ。
const EOCD_SIG: u32 = 0x0605_4b50;
/// Central Directory ファイルヘッダのシグネチャ。
const CDFH_SIG: u32 = 0x0201_4b50;
/// ローカルファイルヘッダのシグネチャ。
const LFH_SIG: u32 = 0x0403_4b50;
/// Zip64 End Of Central Directory レコードのシグネチャ。
const EOCD64_SIG: u32 = 0x0606_4b50;
/// Zip64 EOCD ロケータのシグネチャ。
const EOCD64_LOCATOR_SIG: u32 = 0x0706_4b50;
/// Zip64 EOCD ロケータの固定長。
const EOCD64_LOCATOR_SIZE: usize = 20;
/// Zip64 拡張情報 extra field のヘッダ ID（0x0001）。
const ZIP64_EXTRA_ID: u16 = 0x0001;

/// EOCD レコードの最小長（可変長コメントを除く固定部）。
const EOCD_MIN_SIZE: usize = 22;
/// Central Directory ファイルヘッダの固定部の長さ。
const CDFH_FIXED_SIZE: usize = 46;
/// ローカルファイルヘッダの固定部の長さ。
const LFH_FIXED_SIZE: usize = 30;
/// ZIP のフィールドが Zip64 の実値を別所に持つことを示す 32 ビット番兵。
const ZIP64_U32_SENTINEL: u32 = 0xFFFF_FFFF;
/// 同上の 16 ビット番兵（エントリ数など）。
const ZIP64_U16_SENTINEL: u16 = 0xFFFF;

/// ZIP 構造の parse 失敗。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ZipError {
    /// EOCD が見つからない（ZIP ではない、または末尾が壊れている）。
    NotZip,
    /// 構造がバッファ範囲外を指す（切り詰め・不正オフセット）。
    Truncated,
    /// 期待するシグネチャ（CDFH 等）が一致しない。
    BadSignature,
    /// この実装が未対応の機能（マルチディスク・Zip64 など）。
    Unsupported(&'static str),
}

impl fmt::Display for ZipError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ZipError::NotZip => write!(f, "zip: end-of-central-directory not found"),
            ZipError::Truncated => write!(f, "zip: structure points outside buffer"),
            ZipError::BadSignature => write!(f, "zip: bad central-directory signature"),
            ZipError::Unsupported(what) => write!(f, "zip: unsupported {what}"),
        }
    }
}

impl std::error::Error for ZipError {}

/// 生の ZIP 圧縮メソッドコードを VMM の [`ProviderType`] に対応づける。
///
/// STORE / DEFLATE / Zstandard 以外は [`ProviderType::Unsupported`]。なお
/// VMM ネイティブ DEFLATE への昇格はメソッドコードからは判別できず、
/// 構造的書き直し時に決まる（ここでは標準 DEFLATE として扱う）。
pub fn provider_for_method(method: u16) -> ProviderType {
    match method {
        0 => ProviderType::Store,
        8 => ProviderType::Deflate,
        93 => ProviderType::Zstd,
        _ => ProviderType::Unsupported,
    }
}

/// Central Directory の 1 エントリ分のメタデータ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CdEntry {
    /// エントリ名の生バイト（UTF-8 想定、NUL なし）。
    pub name: Vec<u8>,
    /// 生の ZIP 圧縮メソッドコード。
    pub method_code: u16,
    /// CD に記録された CRC-32。
    pub crc32: u32,
    pub compressed_size: u64,
    pub uncompressed_size: u64,
    /// ローカルファイルヘッダ先頭のアーカイブ内オフセット。
    pub local_header_offset: u64,
    /// メソッドコードから導いたプロバイダ種別。
    pub provider_type: ProviderType,
}

/// mmap された ZIP アーカイブの read-only ビュー。
#[derive(Debug)]
pub struct Archive<'a> {
    data: &'a [u8],
    cd_offset: u64,
    cd_size: u64,
    entries: Vec<CdEntry>,
}

impl<'a> Archive<'a> {
    /// アーカイブ全体のバイト列を parse し、Central Directory を読む。
    pub fn parse(data: &'a [u8]) -> Result<Archive<'a>, ZipError> {
        let eocd_off = find_eocd(data)?;
        let e = &data[eocd_off..];

        let disk = rd_u16(e, 4)?;
        let cd_start_disk = rd_u16(e, 6)?;
        let entries_this_disk = rd_u16(e, 8)?;
        let total16 = rd_u16(e, 10)?;
        let cd_size32 = rd_u32(e, 12)?;
        let cd_offset32 = rd_u32(e, 16)?;

        // いずれかのフィールドが番兵なら実値は Zip64 EOCD レコードにある。
        let needs_zip64 = cd_size32 == ZIP64_U32_SENTINEL
            || cd_offset32 == ZIP64_U32_SENTINEL
            || total16 == ZIP64_U16_SENTINEL
            || disk == ZIP64_U16_SENTINEL
            || cd_start_disk == ZIP64_U16_SENTINEL
            || entries_this_disk == ZIP64_U16_SENTINEL;

        let (cd_offset, cd_size, total_entries) = if needs_zip64 {
            read_zip64_eocd(data, eocd_off)?
        } else {
            if disk != 0 || cd_start_disk != 0 || entries_this_disk != total16 {
                return Err(ZipError::Unsupported("multi-disk archive"));
            }
            (cd_offset32 as u64, cd_size32 as u64, total16 as u64)
        };

        let cd_end = cd_offset
            .checked_add(cd_size)
            .ok_or(ZipError::Truncated)?;
        if cd_end > data.len() as u64 {
            return Err(ZipError::Truncated);
        }

        let entries = parse_cd(data, cd_offset as usize, cd_size as usize, total_entries as usize)?;
        Ok(Archive {
            data,
            cd_offset,
            cd_size,
            entries,
        })
    }

    /// Central Directory の全エントリ。CD 上の出現順。
    pub fn entries(&self) -> &[CdEntry] {
        &self.entries
    }

    /// fingerprint（cd_hash）の対象となる Central Directory ブロック。
    /// `vmidx::hash_cd_block` に渡してヘッダの `source_cd_hash` と照合する。
    pub fn cd_block(&self) -> &'a [u8] {
        let start = self.cd_offset as usize;
        let end = start + self.cd_size as usize;
        &self.data[start..end]
    }

    /// 圧縮データ先頭のアーカイブ内オフセットを算出する。
    ///
    /// CD の extra field 長とローカルヘッダの extra field 長は一致しないこと
    /// があるため、必ずローカルファイルヘッダを読んで確定する（CD の値からの
    /// 推測はしない）。`vmidx` の `EntryRecord::data_offset` に入る値。
    pub fn data_offset(&self, entry: &CdEntry) -> Result<u64, ZipError> {
        let lho = entry.local_header_offset as usize;
        let h = self.data.get(lho..).ok_or(ZipError::Truncated)?;
        if rd_u32(h, 0)? != LFH_SIG {
            return Err(ZipError::BadSignature);
        }
        let name_len = rd_u16(h, 26)? as usize;
        let extra_len = rd_u16(h, 28)? as usize;
        let data_off = lho + LFH_FIXED_SIZE + name_len + extra_len;
        let end = data_off
            .checked_add(entry.compressed_size as usize)
            .ok_or(ZipError::Truncated)?;
        if end > self.data.len() {
            return Err(ZipError::Truncated);
        }
        Ok(data_off as u64)
    }

    /// エントリの圧縮データバイト列を返す（`compressed_size` バイト）。
    /// STORE の直接読みや、DEFLATE/Zstd デコーダへの入力に使う。
    pub fn entry_data(&self, entry: &CdEntry) -> Result<&'a [u8], ZipError> {
        let off = self.data_offset(entry)? as usize;
        Ok(&self.data[off..off + entry.compressed_size as usize])
    }
}

/// EOCD レコードの先頭オフセットを末尾から探索する。コメント長を考慮し、
/// 末尾コメント（最大 64KiB）の範囲を後方走査して、コメント長が末尾までの
/// 残りと整合する最初の候補を採る。
fn find_eocd(data: &[u8]) -> Result<usize, ZipError> {
    if data.len() < EOCD_MIN_SIZE {
        return Err(ZipError::NotZip);
    }
    let max_comment = u16::MAX as usize;
    let highest = data.len() - EOCD_MIN_SIZE;
    let lowest = highest.saturating_sub(max_comment);
    for off in (lowest..=highest).rev() {
        if rd_u32(data, off)? == EOCD_SIG {
            let comment_len = rd_u16(data, off + 20)? as usize;
            if off + EOCD_MIN_SIZE + comment_len == data.len() {
                return Ok(off);
            }
        }
    }
    Err(ZipError::NotZip)
}

/// Central Directory のファイルヘッダ群を `count` 件 parse する。
fn parse_cd(
    data: &[u8],
    start: usize,
    size: usize,
    count: usize,
) -> Result<Vec<CdEntry>, ZipError> {
    let end = start + size;
    let mut pos = start;
    // count はアーカイブ申告値。過大値での過剰確保を避けて上限を設ける
    // （実体が無ければループ内の境界チェックで早期に Truncated になる）。
    let mut entries = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        if pos + CDFH_FIXED_SIZE > end {
            return Err(ZipError::Truncated);
        }
        let h = &data[pos..];
        if rd_u32(h, 0)? != CDFH_SIG {
            return Err(ZipError::BadSignature);
        }
        let method = rd_u16(h, 10)?;
        let crc32 = rd_u32(h, 16)?;
        let comp32 = rd_u32(h, 20)?;
        let uncomp32 = rd_u32(h, 24)?;
        let name_len = rd_u16(h, 28)? as usize;
        let extra_len = rd_u16(h, 30)? as usize;
        let comment_len = rd_u16(h, 32)? as usize;
        let lho32 = rd_u32(h, 42)?;

        let var_start = pos + CDFH_FIXED_SIZE;
        let name_end = var_start + name_len;
        let extra_end = name_end + extra_len;
        let var_end = extra_end + comment_len;
        if var_end > end {
            return Err(ZipError::Truncated);
        }

        let mut compressed_size = comp32 as u64;
        let mut uncompressed_size = uncomp32 as u64;
        let mut local_header_offset = lho32 as u64;
        let need_uncomp = uncomp32 == ZIP64_U32_SENTINEL;
        let need_comp = comp32 == ZIP64_U32_SENTINEL;
        let need_lho = lho32 == ZIP64_U32_SENTINEL;
        if need_uncomp || need_comp || need_lho {
            // 番兵が立っているフィールドの実値を Zip64 extra field から読む。
            // 順序は uncompressed → compressed → local_header_offset で固定。
            let extra = &data[name_end..extra_end];
            parse_zip64_extra(
                extra,
                (need_uncomp, &mut uncompressed_size),
                (need_comp, &mut compressed_size),
                (need_lho, &mut local_header_offset),
            )?;
        }

        let name = data[var_start..name_end].to_vec();
        entries.push(CdEntry {
            name,
            method_code: method,
            crc32,
            compressed_size,
            uncompressed_size,
            local_header_offset,
            provider_type: provider_for_method(method),
        });
        pos = var_end;
    }
    Ok(entries)
}

/// Zip64 EOCD ロケータ（EOCD の直前 20 バイト）と Zip64 EOCD レコードを
/// 読み、(cd_offset, cd_size, total_entries) を返す。
fn read_zip64_eocd(data: &[u8], eocd_off: usize) -> Result<(u64, u64, u64), ZipError> {
    let loc_off = eocd_off
        .checked_sub(EOCD64_LOCATOR_SIZE)
        .ok_or(ZipError::Truncated)?;
    let l = &data[loc_off..];
    if rd_u32(l, 0)? != EOCD64_LOCATOR_SIG {
        return Err(ZipError::Unsupported("zip64 locator missing"));
    }
    let z64_off = rd_u64(l, 8)? as usize;
    let z = data.get(z64_off..).ok_or(ZipError::Truncated)?;
    if rd_u32(z, 0)? != EOCD64_SIG {
        return Err(ZipError::BadSignature);
    }
    let disk = rd_u32(z, 16)?;
    let cd_start_disk = rd_u32(z, 20)?;
    let entries_this_disk = rd_u64(z, 24)?;
    let total_entries = rd_u64(z, 32)?;
    let cd_size = rd_u64(z, 40)?;
    let cd_offset = rd_u64(z, 48)?;
    if disk != 0 || cd_start_disk != 0 || entries_this_disk != total_entries {
        return Err(ZipError::Unsupported("multi-disk archive"));
    }
    Ok((cd_offset, cd_size, total_entries))
}

/// CD エントリの Zip64 拡張情報 extra field（ID 0x0001）を解釈し、番兵が
/// 立っていたフィールドの 64 ビット実値を書き込む。実値は
/// uncompressed → compressed → local_header_offset の順に、番兵が立った
/// ものだけが並ぶ。
fn parse_zip64_extra(
    extra: &[u8],
    uncomp: (bool, &mut u64),
    comp: (bool, &mut u64),
    lho: (bool, &mut u64),
) -> Result<(), ZipError> {
    let mut p = 0;
    while p + 4 <= extra.len() {
        let id = rd_u16(extra, p)?;
        let len = rd_u16(extra, p + 2)? as usize;
        let body_start = p + 4;
        let body_end = body_start + len;
        if body_end > extra.len() {
            return Err(ZipError::Truncated);
        }
        if id == ZIP64_EXTRA_ID {
            let body = &extra[body_start..body_end];
            let mut q = 0usize;
            if uncomp.0 {
                *uncomp.1 = rd_u64(body, q)?;
                q += 8;
            }
            if comp.0 {
                *comp.1 = rd_u64(body, q)?;
                q += 8;
            }
            if lho.0 {
                *lho.1 = rd_u64(body, q)?;
                q += 8;
            }
            let _ = q;
            return Ok(());
        }
        p = body_end;
    }
    // 番兵は立っているのに Zip64 extra が無い ＝ 壊れている。
    Err(ZipError::Truncated)
}

#[inline]
fn rd_u16(b: &[u8], off: usize) -> Result<u16, ZipError> {
    b.get(off..off + 2)
        .map(|s| u16::from_le_bytes(s.try_into().unwrap()))
        .ok_or(ZipError::Truncated)
}

#[inline]
fn rd_u32(b: &[u8], off: usize) -> Result<u32, ZipError> {
    b.get(off..off + 4)
        .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
        .ok_or(ZipError::Truncated)
}

#[inline]
fn rd_u64(b: &[u8], off: usize) -> Result<u64, ZipError> {
    b.get(off..off + 8)
        .map(|s| u64::from_le_bytes(s.try_into().unwrap()))
        .ok_or(ZipError::Truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// テスト用の最小 ZIP ビルダ。STORE エントリのみ（圧縮しない）。
    struct ZipFixture {
        bytes: Vec<u8>,
        cd_records: Vec<u8>,
        cd_offset: u32,
        count: u16,
    }

    impl ZipFixture {
        fn new() -> ZipFixture {
            ZipFixture {
                bytes: Vec::new(),
                cd_records: Vec::new(),
                cd_offset: 0,
                count: 0,
            }
        }

        /// STORE エントリを 1 件追加する（LFH + データ。CD レコードは後で連結）。
        fn add_store(&mut self, name: &str, data: &[u8]) -> &mut Self {
            self.add_store_lfh_extra(name, data, b"")
        }

        /// STORE エントリを追加するが、ローカルヘッダだけに extra field を持た
        /// せる（CD 側は extra なし）。data_offset 算出が LFH を読むことの検証用。
        fn add_store_lfh_extra(&mut self, name: &str, data: &[u8], lfh_extra: &[u8]) -> &mut Self {
            let lho = self.bytes.len() as u32;
            let name_b = name.as_bytes();
            // ローカルファイルヘッダ。
            push_u32(&mut self.bytes, LFH_SIG);
            push_u16(&mut self.bytes, 20); // version needed
            push_u16(&mut self.bytes, 0); // flags
            push_u16(&mut self.bytes, 0); // method = STORE
            push_u16(&mut self.bytes, 0); // mod time
            push_u16(&mut self.bytes, 0); // mod date
            push_u32(&mut self.bytes, 0); // crc32（parse では未検証）
            push_u32(&mut self.bytes, data.len() as u32); // comp size
            push_u32(&mut self.bytes, data.len() as u32); // uncomp size
            push_u16(&mut self.bytes, name_b.len() as u16);
            push_u16(&mut self.bytes, lfh_extra.len() as u16); // extra len
            self.bytes.extend_from_slice(name_b);
            self.bytes.extend_from_slice(lfh_extra);
            self.bytes.extend_from_slice(data);

            // Central Directory ファイルヘッダ。
            push_u32(&mut self.cd_records, CDFH_SIG);
            push_u16(&mut self.cd_records, 20); // version made by
            push_u16(&mut self.cd_records, 20); // version needed
            push_u16(&mut self.cd_records, 0); // flags
            push_u16(&mut self.cd_records, 0); // method
            push_u16(&mut self.cd_records, 0); // mod time
            push_u16(&mut self.cd_records, 0); // mod date
            push_u32(&mut self.cd_records, 0); // crc32
            push_u32(&mut self.cd_records, data.len() as u32);
            push_u32(&mut self.cd_records, data.len() as u32);
            push_u16(&mut self.cd_records, name_b.len() as u16);
            push_u16(&mut self.cd_records, 0); // extra len
            push_u16(&mut self.cd_records, 0); // comment len
            push_u16(&mut self.cd_records, 0); // disk start
            push_u16(&mut self.cd_records, 0); // internal attrs
            push_u32(&mut self.cd_records, 0); // external attrs
            push_u32(&mut self.cd_records, lho);
            self.cd_records.extend_from_slice(name_b);
            self.count += 1;
            self
        }

        /// CD と EOCD を連結して完成バイト列を返す。`comment` は末尾コメント。
        fn finish(mut self, comment: &[u8]) -> Vec<u8> {
            self.cd_offset = self.bytes.len() as u32;
            let cd_size = self.cd_records.len() as u32;
            self.bytes.extend_from_slice(&self.cd_records);
            push_u32(&mut self.bytes, EOCD_SIG);
            push_u16(&mut self.bytes, 0); // disk
            push_u16(&mut self.bytes, 0); // cd start disk
            push_u16(&mut self.bytes, self.count); // entries this disk
            push_u16(&mut self.bytes, self.count); // total entries
            push_u32(&mut self.bytes, cd_size);
            push_u32(&mut self.bytes, self.cd_offset);
            push_u16(&mut self.bytes, comment.len() as u16);
            self.bytes.extend_from_slice(comment);
            self.bytes
        }
    }

    fn push_u16(v: &mut Vec<u8>, x: u16) {
        v.extend_from_slice(&x.to_le_bytes());
    }
    fn push_u32(v: &mut Vec<u8>, x: u32) {
        v.extend_from_slice(&x.to_le_bytes());
    }

    fn two_entry_zip() -> Vec<u8> {
        let mut f = ZipFixture::new();
        f.add_store("hello.txt", b"hi");
        f.add_store("dir/second.bin", b"payload");
        f.finish(b"")
    }

    #[test]
    fn parses_central_directory() {
        let zip = two_entry_zip();
        let ar = Archive::parse(&zip).expect("valid zip parses");
        let e = ar.entries();
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].name, b"hello.txt");
        assert_eq!(e[0].compressed_size, 2);
        assert_eq!(e[0].uncompressed_size, 2);
        assert_eq!(e[0].provider_type, ProviderType::Store);
        assert_eq!(e[1].name, b"dir/second.bin");
        assert_eq!(e[1].compressed_size, 7);
    }

    #[test]
    fn cd_block_matches_eocd_region() {
        let zip = two_entry_zip();
        let ar = Archive::parse(&zip).expect("valid zip parses");
        let cd = ar.cd_block();
        // CD ブロックの先頭は CDFH シグネチャで始まる。
        assert_eq!(&cd[0..4], &CDFH_SIG.to_le_bytes());
        // CD は 2 エントリ分の固定部 + 名前長から成る。
        let expected = 2 * CDFH_FIXED_SIZE + b"hello.txt".len() + b"dir/second.bin".len();
        assert_eq!(cd.len(), expected);
    }

    #[test]
    fn finds_eocd_past_trailing_comment() {
        let mut f = ZipFixture::new();
        f.add_store("a", b"x");
        let zip = f.finish(b"this is a trailing archive comment");
        let ar = Archive::parse(&zip).expect("zip with comment parses");
        assert_eq!(ar.entries().len(), 1);
        assert_eq!(ar.entries()[0].name, b"a");
    }

    #[test]
    fn not_a_zip_is_rejected() {
        let junk = vec![0u8; 100];
        assert!(matches!(Archive::parse(&junk), Err(ZipError::NotZip)));
        assert!(matches!(Archive::parse(b"short"), Err(ZipError::NotZip)));
    }

    #[test]
    fn resolves_data_offset_and_reads_store_payload() {
        let zip = two_entry_zip();
        let ar = Archive::parse(&zip).expect("valid zip parses");
        let e = ar.entries().to_vec();
        // 最初のエントリ: lho=0, LFH 固定 30 + 名前 9 + extra 0 = 39。
        assert_eq!(ar.data_offset(&e[0]).unwrap(), 39);
        assert_eq!(ar.entry_data(&e[0]).unwrap(), b"hi");
        assert_eq!(ar.entry_data(&e[1]).unwrap(), b"payload");
    }

    #[test]
    fn data_offset_honors_local_header_extra() {
        // CD には extra を載せず、LFH にだけ 6 バイトの extra を持たせる。
        let mut f = ZipFixture::new();
        f.add_store_lfh_extra("x.bin", b"DATA", &[1, 2, 3, 4, 5, 6]);
        let zip = f.finish(b"");
        let ar = Archive::parse(&zip).expect("valid zip parses");
        let e = ar.entries().to_vec();
        // CD の extra(0) を信じると 30+5=35 になるが、正しくは LFH extra(6) を
        // 足した 30+5+6=41。
        assert_eq!(ar.data_offset(&e[0]).unwrap(), 41);
        assert_eq!(ar.entry_data(&e[0]).unwrap(), b"DATA");
    }

    /// 1 エントリの Zip64 アーカイブを手組みする。CD レコードはサイズと
    /// local_header_offset を番兵にし、実値を Zip64 extra field に置く。EOCD は
    /// 番兵を立て、Zip64 EOCD レコード/ロケータを経由させる。
    fn build_zip64_single() -> Vec<u8> {
        let name: &[u8] = b"z64.bin";
        let data: &[u8] = b"Z64";
        let mut b = Vec::new();

        // ローカルファイルヘッダ（サイズは u32 に収まるので番兵にしない）。
        push_u32(&mut b, LFH_SIG);
        push_u16(&mut b, 45);
        push_u16(&mut b, 0);
        push_u16(&mut b, 0); // method = STORE
        push_u16(&mut b, 0);
        push_u16(&mut b, 0);
        push_u32(&mut b, 0); // crc
        push_u32(&mut b, data.len() as u32);
        push_u32(&mut b, data.len() as u32);
        push_u16(&mut b, name.len() as u16);
        push_u16(&mut b, 0); // extra len
        b.extend_from_slice(name);
        b.extend_from_slice(data);

        // Zip64 extra field: uncompressed → compressed → local_header_offset。
        let mut z64extra = Vec::new();
        push_u16(&mut z64extra, ZIP64_EXTRA_ID);
        push_u16(&mut z64extra, 24); // body len = 3 × u64
        z64extra.extend_from_slice(&(data.len() as u64).to_le_bytes());
        z64extra.extend_from_slice(&(data.len() as u64).to_le_bytes());
        z64extra.extend_from_slice(&0u64.to_le_bytes()); // lho = 0

        let cd_offset = b.len() as u64;
        push_u32(&mut b, CDFH_SIG);
        push_u16(&mut b, 45);
        push_u16(&mut b, 45);
        push_u16(&mut b, 0);
        push_u16(&mut b, 0); // method
        push_u16(&mut b, 0);
        push_u16(&mut b, 0);
        push_u32(&mut b, 0); // crc
        push_u32(&mut b, ZIP64_U32_SENTINEL); // comp size 番兵
        push_u32(&mut b, ZIP64_U32_SENTINEL); // uncomp size 番兵
        push_u16(&mut b, name.len() as u16);
        push_u16(&mut b, z64extra.len() as u16);
        push_u16(&mut b, 0); // comment len
        push_u16(&mut b, 0); // disk start
        push_u16(&mut b, 0); // internal attrs
        push_u32(&mut b, 0); // external attrs
        push_u32(&mut b, ZIP64_U32_SENTINEL); // lho 番兵
        b.extend_from_slice(name);
        b.extend_from_slice(&z64extra);
        let cd_size = b.len() as u64 - cd_offset;

        // Zip64 EOCD レコード。
        let z64_eocd_off = b.len() as u64;
        push_u32(&mut b, EOCD64_SIG);
        b.extend_from_slice(&44u64.to_le_bytes()); // size of record（未検証）
        push_u16(&mut b, 45);
        push_u16(&mut b, 45);
        push_u32(&mut b, 0); // disk
        push_u32(&mut b, 0); // cd start disk
        b.extend_from_slice(&1u64.to_le_bytes()); // entries this disk
        b.extend_from_slice(&1u64.to_le_bytes()); // total entries
        b.extend_from_slice(&cd_size.to_le_bytes());
        b.extend_from_slice(&cd_offset.to_le_bytes());

        // Zip64 EOCD ロケータ。
        push_u32(&mut b, EOCD64_LOCATOR_SIG);
        push_u32(&mut b, 0); // disk of zip64 eocd
        b.extend_from_slice(&z64_eocd_off.to_le_bytes());
        push_u32(&mut b, 1); // total disks

        // EOCD（番兵）。
        push_u32(&mut b, EOCD_SIG);
        push_u16(&mut b, 0);
        push_u16(&mut b, 0);
        push_u16(&mut b, ZIP64_U16_SENTINEL); // entries this disk
        push_u16(&mut b, ZIP64_U16_SENTINEL); // total
        push_u32(&mut b, ZIP64_U32_SENTINEL); // cd size
        push_u32(&mut b, ZIP64_U32_SENTINEL); // cd offset
        push_u16(&mut b, 0); // comment len
        b
    }

    #[test]
    fn parses_zip64_via_eocd_and_extra_field() {
        let zip = build_zip64_single();
        let ar = Archive::parse(&zip).expect("zip64 archive parses");
        let e = ar.entries();
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].name, b"z64.bin");
        // 実値は Zip64 extra field 由来（番兵ではない）。
        assert_eq!(e[0].compressed_size, 3);
        assert_eq!(e[0].uncompressed_size, 3);
        assert_eq!(e[0].local_header_offset, 0);
        // data_offset 解決とペイロード読みも通る。
        assert_eq!(ar.data_offset(&e[0].clone()).unwrap(), 30 + 7);
        assert_eq!(ar.entry_data(&e[0].clone()).unwrap(), b"Z64");
    }

    #[test]
    fn zip64_sentinel_without_extra_is_truncated() {
        // CD の comp サイズだけ番兵にし、Zip64 extra を付けない壊れたケース。
        let mut f = ZipFixture::new();
        f.add_store("broken", b"abc");
        let mut zip = f.finish(b"");
        // finish 後の CD レコード内 comp size（CDFH 先頭 +20）を番兵に潰す。
        // CD は LFH(30+6) + data(3) = 39 から始まる。
        let cd_start = 30 + 6 + 3;
        let comp_field = cd_start + 20;
        zip[comp_field..comp_field + 4].copy_from_slice(&ZIP64_U32_SENTINEL.to_le_bytes());
        assert!(matches!(Archive::parse(&zip), Err(ZipError::Truncated)));
    }

    #[test]
    fn method_mapping() {
        assert_eq!(provider_for_method(0), ProviderType::Store);
        assert_eq!(provider_for_method(8), ProviderType::Deflate);
        assert_eq!(provider_for_method(93), ProviderType::Zstd);
        assert_eq!(provider_for_method(12), ProviderType::Unsupported);
    }
}
