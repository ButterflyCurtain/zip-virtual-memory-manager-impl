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
//!
//! 全整数はリトルエンディアン。設計: docs `ZIP_Virtual_Memory_Manager`
//! および `..._Diff_Layer_Pressure_Integrity_Detection`（cd_hash の定義）。

use crate::vmidx::ProviderType;
use std::fmt;

/// End Of Central Directory レコードのシグネチャ。
const EOCD_SIG: u32 = 0x0605_4b50;
/// Central Directory ファイルヘッダのシグネチャ。
const CDFH_SIG: u32 = 0x0201_4b50;

/// EOCD レコードの最小長（可変長コメントを除く固定部）。
const EOCD_MIN_SIZE: usize = 22;
/// Central Directory ファイルヘッダの固定部の長さ。
const CDFH_FIXED_SIZE: usize = 46;
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
        let total_entries = rd_u16(e, 10)?;
        let cd_size = rd_u32(e, 12)?;
        let cd_offset = rd_u32(e, 16)?;

        if disk != 0 || cd_start_disk != 0 || entries_this_disk != total_entries {
            return Err(ZipError::Unsupported("multi-disk archive"));
        }
        if cd_size == ZIP64_U32_SENTINEL
            || cd_offset == ZIP64_U32_SENTINEL
            || total_entries == ZIP64_U16_SENTINEL
        {
            return Err(ZipError::Unsupported("zip64"));
        }

        let cd_offset = cd_offset as u64;
        let cd_size = cd_size as u64;
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
    let mut entries = Vec::with_capacity(count);
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
        let compressed_size = rd_u32(h, 20)?;
        let uncompressed_size = rd_u32(h, 24)?;
        let name_len = rd_u16(h, 28)? as usize;
        let extra_len = rd_u16(h, 30)? as usize;
        let comment_len = rd_u16(h, 32)? as usize;
        let local_header_offset = rd_u32(h, 42)?;

        let var_start = pos + CDFH_FIXED_SIZE;
        let var_end = var_start + name_len + extra_len + comment_len;
        if var_end > end {
            return Err(ZipError::Truncated);
        }

        if compressed_size == ZIP64_U32_SENTINEL
            || uncompressed_size == ZIP64_U32_SENTINEL
            || local_header_offset == ZIP64_U32_SENTINEL
        {
            return Err(ZipError::Unsupported("zip64"));
        }

        let name = data[var_start..var_start + name_len].to_vec();
        entries.push(CdEntry {
            name,
            method_code: method,
            crc32,
            compressed_size: compressed_size as u64,
            uncompressed_size: uncompressed_size as u64,
            local_header_offset: local_header_offset as u64,
            provider_type: provider_for_method(method),
        });
        pos = var_end;
    }
    Ok(entries)
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
            let lho = self.bytes.len() as u32;
            let name_b = name.as_bytes();
            // ローカルファイルヘッダ。
            push_u32(&mut self.bytes, 0x0403_4b50);
            push_u16(&mut self.bytes, 20); // version needed
            push_u16(&mut self.bytes, 0); // flags
            push_u16(&mut self.bytes, 0); // method = STORE
            push_u16(&mut self.bytes, 0); // mod time
            push_u16(&mut self.bytes, 0); // mod date
            push_u32(&mut self.bytes, 0); // crc32（parse では未検証）
            push_u32(&mut self.bytes, data.len() as u32); // comp size
            push_u32(&mut self.bytes, data.len() as u32); // uncomp size
            push_u16(&mut self.bytes, name_b.len() as u16);
            push_u16(&mut self.bytes, 0); // extra len
            self.bytes.extend_from_slice(name_b);
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
    fn method_mapping() {
        assert_eq!(provider_for_method(0), ProviderType::Store);
        assert_eq!(provider_for_method(8), ProviderType::Deflate);
        assert_eq!(provider_for_method(93), ProviderType::Zstd);
        assert_eq!(provider_for_method(12), ProviderType::Unsupported);
    }
}
