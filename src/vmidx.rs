//! vmidx: 永続シークインデックス。
//!
//! エントリごとのメタデータと展開チェックポイントを保持し、圧縮 ZIP
//! エントリへのランダムアクセスを安価にする。サイドカー
//! (`archive.zip.vmm/vmidx`) に置かれ、マウント時に read-only で mmap される。
//!
//! 最重要の性質: **vmidx はキャッシュである**。ユーザーデータを一切含まず、
//! 全バイトが `archive.zip` から再構築可能。破損・切り詰め・喪失は再構築の
//! コストを生むだけで、データを失わせることはない。検証失敗時の応答は常に
//! 「破棄して再構築」。
//!
//! このモジュールは現状、固定幅構造である FILE HEADER と ENTRY RECORD の
//! エンコード/デコードと CRC-32C 検証を提供する。全整数はリトルエンディアン、
//! 全オフセットはファイル先頭からのバイトオフセット。
//!
//! 設計: docs `ZIP_Virtual_Memory_Manager_vmidx_Index_Spec`。

use std::fmt;

/// FILE HEADER の固定長（バイト）。
pub const HEADER_SIZE: usize = 128;
/// ENTRY RECORD の固定長（バイト）。
pub const ENTRY_SIZE: usize = 96;
/// FILE HEADER 先頭のマジック: `"VMIDX\0\0\0"`。
pub const MAGIC: [u8; 8] = *b"VMIDX\0\0\0";
/// 現在サポートする `format_version`。
pub const FORMAT_VERSION: u16 = 1;

/// FILE HEADER の `flags` フィールド（16 ビット）のビット定義。
pub mod flags {
    /// アーカイブが最後に VMM によって完全に書き直されたことを示す来歴フラグ。
    pub const VMM_GENERATED: u16 = 1 << 0;
    /// 標準 DEFLATE のウィンドウスナップショットが zlib 圧縮で格納されている。
    pub const WINDOWS_COMPRESSED: u16 = 1 << 1;
}

/// ENTRY RECORD の `entry_flags` フィールド（8 ビット）のビット定義。
pub mod entry_flags {
    /// STORE への昇格が行われた。
    pub const STORE_PROMOTED: u8 = 1 << 0;
    /// ZSTD だがシーク不能（seekable frame でない）。
    pub const ZSTD_NON_SEEKABLE: u8 = 1 << 1;
}

/// エントリの圧縮プロバイダ種別（`provider_type` フィールド）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderType {
    Store,
    Deflate,
    DeflateVmm,
    Zstd,
    Unsupported,
}

impl ProviderType {
    /// オンディスクのコード値を返す。
    pub fn code(self) -> u8 {
        match self {
            ProviderType::Store => 0,
            ProviderType::Deflate => 1,
            ProviderType::DeflateVmm => 2,
            ProviderType::Zstd => 3,
            ProviderType::Unsupported => 255,
        }
    }

    /// オンディスクのコード値から変換する。未知のコードは `Unsupported`。
    pub fn from_code(code: u8) -> ProviderType {
        match code {
            0 => ProviderType::Store,
            1 => ProviderType::Deflate,
            2 => ProviderType::DeflateVmm,
            3 => ProviderType::Zstd,
            _ => ProviderType::Unsupported,
        }
    }
}

/// 固定幅構造のデコード失敗。仕様上、いずれの失敗も最終的な応答は
/// 「vmidx を破棄して再構築」だが、診断のため原因を区別する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// FILE HEADER のマジックが一致しない。
    BadMagic,
    /// 未サポートの `format_version`。
    UnsupportedVersion(u16),
    /// FILE HEADER の CRC-32C 不一致。
    HeaderCrcMismatch { stored: u32, computed: u32 },
    /// ENTRY RECORD の CRC-32C 不一致。
    RecordCrcMismatch { stored: u32, computed: u32 },
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::BadMagic => write!(f, "vmidx: bad magic"),
            DecodeError::UnsupportedVersion(v) => {
                write!(f, "vmidx: unsupported format_version {v}")
            }
            DecodeError::HeaderCrcMismatch { stored, computed } => write!(
                f,
                "vmidx: header CRC mismatch (stored {stored:#010x}, computed {computed:#010x})"
            ),
            DecodeError::RecordCrcMismatch { stored, computed } => write!(
                f,
                "vmidx: entry record CRC mismatch (stored {stored:#010x}, computed {computed:#010x})"
            ),
        }
    }
}

impl std::error::Error for DecodeError {}

/// FILE HEADER（固定 128 バイト、オフセット 0）。
///
/// `magic` / `format_version` / `header_crc32` / パディングは `encode`/`decode`
/// が管理するため、構造体には保持しない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub flags: u16,
    pub page_size: u32,
    pub checkpoint_interval: u64,
    pub source_file_size: u64,
    pub source_inode: u64,
    pub source_mtime_ns: u64,
    /// Central Directory ブロックの XXH3-128（16 バイト）+ 4 バイトのゼロ詰め。
    pub source_cd_hash: [u8; 20],
    pub entry_count: u64,
    pub entry_table_offset: u64,
    pub name_heap_offset: u64,
    pub name_heap_size: u64,
    pub advisory_offset: u64,
    pub advisory_size: u64,
}

impl Header {
    /// 128 バイトの FILE HEADER へエンコードする。`header_crc32` は
    /// バイト `[0..120]` 上で計算して書き込む。
    pub fn encode(&self) -> [u8; HEADER_SIZE] {
        let mut b = [0u8; HEADER_SIZE];
        b[0..8].copy_from_slice(&MAGIC);
        b[8..10].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        b[10..12].copy_from_slice(&self.flags.to_le_bytes());
        b[12..16].copy_from_slice(&self.page_size.to_le_bytes());
        b[16..24].copy_from_slice(&self.checkpoint_interval.to_le_bytes());
        b[24..32].copy_from_slice(&self.source_file_size.to_le_bytes());
        b[32..40].copy_from_slice(&self.source_inode.to_le_bytes());
        b[40..48].copy_from_slice(&self.source_mtime_ns.to_le_bytes());
        b[48..68].copy_from_slice(&self.source_cd_hash);
        // [68..72] reserved = 0
        b[72..80].copy_from_slice(&self.entry_count.to_le_bytes());
        b[80..88].copy_from_slice(&self.entry_table_offset.to_le_bytes());
        b[88..96].copy_from_slice(&self.name_heap_offset.to_le_bytes());
        b[96..104].copy_from_slice(&self.name_heap_size.to_le_bytes());
        b[104..112].copy_from_slice(&self.advisory_offset.to_le_bytes());
        b[112..120].copy_from_slice(&self.advisory_size.to_le_bytes());
        let crc = crc32c::crc32c(&b[0..120]);
        b[120..124].copy_from_slice(&crc.to_le_bytes());
        // [124..128] padding = 0
        b
    }

    /// 128 バイトの FILE HEADER をデコードする。検証順は仕様 Section 7 に従い
    /// magic → format_version → header_crc32。
    pub fn decode(b: &[u8; HEADER_SIZE]) -> Result<Header, DecodeError> {
        if b[0..8] != MAGIC {
            return Err(DecodeError::BadMagic);
        }
        let version = rd_u16(b, 8);
        if version != FORMAT_VERSION {
            return Err(DecodeError::UnsupportedVersion(version));
        }
        let stored = rd_u32(b, 120);
        let computed = crc32c::crc32c(&b[0..120]);
        if stored != computed {
            return Err(DecodeError::HeaderCrcMismatch { stored, computed });
        }
        let mut source_cd_hash = [0u8; 20];
        source_cd_hash.copy_from_slice(&b[48..68]);
        Ok(Header {
            flags: rd_u16(b, 10),
            page_size: rd_u32(b, 12),
            checkpoint_interval: rd_u64(b, 16),
            source_file_size: rd_u64(b, 24),
            source_inode: rd_u64(b, 32),
            source_mtime_ns: rd_u64(b, 40),
            source_cd_hash,
            entry_count: rd_u64(b, 72),
            entry_table_offset: rd_u64(b, 80),
            name_heap_offset: rd_u64(b, 88),
            name_heap_size: rd_u64(b, 96),
            advisory_offset: rd_u64(b, 104),
            advisory_size: rd_u64(b, 112),
        })
    }
}

/// ENTRY RECORD（固定 96 バイト）。エントリテーブルは `name_hash` 昇順。
///
/// `reserved` / `record_crc32` / パディングは `encode`/`decode` が管理する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryRecord {
    /// エントリ名の XXH3-64。
    pub name_hash: u64,
    /// NAME HEAP 内へのオフセット。
    pub name_offset: u64,
    pub name_len: u16,
    pub provider_type: ProviderType,
    pub entry_flags: u8,
    /// 生の ZIP 圧縮メソッド。
    pub method_code: u16,
    pub local_header_offset: u64,
    /// 圧縮データの先頭バイト。
    pub data_offset: u64,
    pub compressed_size: u64,
    pub uncompressed_size: u64,
    /// このエントリの最初の CHECKPOINT CHUNK。0 = 未生成。
    pub chunk_head_offset: u64,
    pub checkpoint_count: u64,
    pub commit_count_for_entry: u64,
}

impl EntryRecord {
    /// 96 バイトの ENTRY RECORD へエンコードする。`record_crc32` は
    /// バイト `[0..88]` 上で計算して書き込む。
    pub fn encode(&self) -> [u8; ENTRY_SIZE] {
        let mut b = [0u8; ENTRY_SIZE];
        b[0..8].copy_from_slice(&self.name_hash.to_le_bytes());
        b[8..16].copy_from_slice(&self.name_offset.to_le_bytes());
        b[16..18].copy_from_slice(&self.name_len.to_le_bytes());
        b[18] = self.provider_type.code();
        b[19] = self.entry_flags;
        b[20..22].copy_from_slice(&self.method_code.to_le_bytes());
        // [22..24] reserved = 0
        b[24..32].copy_from_slice(&self.local_header_offset.to_le_bytes());
        b[32..40].copy_from_slice(&self.data_offset.to_le_bytes());
        b[40..48].copy_from_slice(&self.compressed_size.to_le_bytes());
        b[48..56].copy_from_slice(&self.uncompressed_size.to_le_bytes());
        b[56..64].copy_from_slice(&self.chunk_head_offset.to_le_bytes());
        b[64..72].copy_from_slice(&self.checkpoint_count.to_le_bytes());
        b[72..80].copy_from_slice(&self.commit_count_for_entry.to_le_bytes());
        // [80..88] reserved2 = 0
        let crc = crc32c::crc32c(&b[0..88]);
        b[88..92].copy_from_slice(&crc.to_le_bytes());
        // [92..96] padding = 0
        b
    }

    /// 96 バイトの ENTRY RECORD をデコードする。`record_crc32` を先に検証する。
    pub fn decode(b: &[u8; ENTRY_SIZE]) -> Result<EntryRecord, DecodeError> {
        let stored = rd_u32(b, 88);
        let computed = crc32c::crc32c(&b[0..88]);
        if stored != computed {
            return Err(DecodeError::RecordCrcMismatch { stored, computed });
        }
        Ok(EntryRecord {
            name_hash: rd_u64(b, 0),
            name_offset: rd_u64(b, 8),
            name_len: rd_u16(b, 16),
            provider_type: ProviderType::from_code(b[18]),
            entry_flags: b[19],
            method_code: rd_u16(b, 20),
            local_header_offset: rd_u64(b, 24),
            data_offset: rd_u64(b, 32),
            compressed_size: rd_u64(b, 40),
            uncompressed_size: rd_u64(b, 48),
            chunk_head_offset: rd_u64(b, 56),
            checkpoint_count: rd_u64(b, 64),
            commit_count_for_entry: rd_u64(b, 72),
        })
    }
}

#[inline]
fn rd_u16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(b[off..off + 2].try_into().unwrap())
}

#[inline]
fn rd_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}

#[inline]
fn rd_u64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_header() -> Header {
        Header {
            flags: flags::VMM_GENERATED | flags::WINDOWS_COMPRESSED,
            page_size: 4096,
            checkpoint_interval: 1_048_576,
            source_file_size: 100 * 1024 * 1024 * 1024,
            source_inode: 0x1234_5678_9abc_def0,
            source_mtime_ns: 1_700_000_000_000_000_000,
            source_cd_hash: [
                1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 0, 0, 0, 0,
            ],
            entry_count: 42,
            entry_table_offset: 128,
            name_heap_offset: 128 + 42 * ENTRY_SIZE as u64,
            name_heap_size: 1024,
            advisory_offset: 9999,
            advisory_size: 4242,
        }
    }

    fn sample_entry() -> EntryRecord {
        EntryRecord {
            name_hash: 0xdead_beef_cafe_f00d,
            name_offset: 256,
            name_len: 17,
            provider_type: ProviderType::DeflateVmm,
            entry_flags: entry_flags::STORE_PROMOTED,
            method_code: 8,
            local_header_offset: 512,
            data_offset: 600,
            compressed_size: 4096,
            uncompressed_size: 8192,
            chunk_head_offset: 70_000,
            checkpoint_count: 3,
            commit_count_for_entry: 5,
        }
    }

    #[test]
    fn sizes_match_spec() {
        assert_eq!(HEADER_SIZE, 128);
        assert_eq!(ENTRY_SIZE, 96);
    }

    #[test]
    fn header_roundtrip() {
        let h = sample_header();
        let bytes = h.encode();
        assert_eq!(bytes.len(), HEADER_SIZE);
        assert_eq!(&bytes[0..8], &MAGIC);
        assert_eq!(rd_u16(&bytes, 8), FORMAT_VERSION);
        let decoded = Header::decode(&bytes).expect("valid header decodes");
        assert_eq!(decoded, h);
    }

    #[test]
    fn header_bad_magic() {
        let mut bytes = sample_header().encode();
        bytes[0] ^= 0xff;
        assert_eq!(Header::decode(&bytes), Err(DecodeError::BadMagic));
    }

    #[test]
    fn header_unsupported_version_precedes_crc() {
        let mut bytes = sample_header().encode();
        // CRC を直さずに version を 2 へ。version 検査が CRC 検査より先。
        bytes[8..10].copy_from_slice(&2u16.to_le_bytes());
        assert_eq!(
            Header::decode(&bytes),
            Err(DecodeError::UnsupportedVersion(2))
        );
    }

    #[test]
    fn header_crc_mismatch_detected() {
        let mut bytes = sample_header().encode();
        bytes[40] ^= 0x01; // source_mtime_ns 内の 1 ビットを反転
        match Header::decode(&bytes) {
            Err(DecodeError::HeaderCrcMismatch { .. }) => {}
            other => panic!("expected HeaderCrcMismatch, got {other:?}"),
        }
    }

    #[test]
    fn entry_roundtrip() {
        let e = sample_entry();
        let bytes = e.encode();
        assert_eq!(bytes.len(), ENTRY_SIZE);
        let decoded = EntryRecord::decode(&bytes).expect("valid record decodes");
        assert_eq!(decoded, e);
    }

    #[test]
    fn entry_crc_mismatch_detected() {
        let mut bytes = sample_entry().encode();
        bytes[0] ^= 0x01; // name_hash の 1 ビットを反転
        match EntryRecord::decode(&bytes) {
            Err(DecodeError::RecordCrcMismatch { .. }) => {}
            other => panic!("expected RecordCrcMismatch, got {other:?}"),
        }
    }

    #[test]
    fn provider_type_roundtrips() {
        for p in [
            ProviderType::Store,
            ProviderType::Deflate,
            ProviderType::DeflateVmm,
            ProviderType::Zstd,
            ProviderType::Unsupported,
        ] {
            assert_eq!(ProviderType::from_code(p.code()), p);
        }
        // 未知のコードは Unsupported に落ちる。
        assert_eq!(ProviderType::from_code(7), ProviderType::Unsupported);
    }
}
