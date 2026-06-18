//! FILE HEADER（固定 128 バイト、オフセット 0）。

use super::{DecodeError, FORMAT_VERSION, HEADER_SIZE, MAGIC, rd_u16, rd_u32, rd_u64};

/// FILE HEADER。
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vmidx::{ENTRY_SIZE, flags};

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

    #[test]
    fn roundtrip() {
        let h = sample_header();
        let bytes = h.encode();
        assert_eq!(bytes.len(), HEADER_SIZE);
        assert_eq!(&bytes[0..8], &MAGIC);
        assert_eq!(rd_u16(&bytes, 8), FORMAT_VERSION);
        let decoded = Header::decode(&bytes).expect("valid header decodes");
        assert_eq!(decoded, h);
    }

    #[test]
    fn bad_magic() {
        let mut bytes = sample_header().encode();
        bytes[0] ^= 0xff;
        assert_eq!(Header::decode(&bytes), Err(DecodeError::BadMagic));
    }

    #[test]
    fn unsupported_version_precedes_crc() {
        let mut bytes = sample_header().encode();
        // CRC を直さずに version を 2 へ。version 検査が CRC 検査より先。
        bytes[8..10].copy_from_slice(&2u16.to_le_bytes());
        assert_eq!(
            Header::decode(&bytes),
            Err(DecodeError::UnsupportedVersion(2))
        );
    }

    #[test]
    fn crc_mismatch_detected() {
        let mut bytes = sample_header().encode();
        bytes[40] ^= 0x01; // source_mtime_ns 内の 1 ビットを反転
        match Header::decode(&bytes) {
            Err(DecodeError::HeaderCrcMismatch { .. }) => {}
            other => panic!("expected HeaderCrcMismatch, got {other:?}"),
        }
    }
}
