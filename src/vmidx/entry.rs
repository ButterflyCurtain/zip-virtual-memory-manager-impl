//! ENTRY RECORD（固定 96 バイト）と圧縮プロバイダ種別。

use super::{DecodeError, ENTRY_SIZE, rd_u16, rd_u32, rd_u64};

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

/// ENTRY RECORD。エントリテーブルは `name_hash` 昇順（同一ハッシュ内は名前
/// バイト昇順）に並ぶ。
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vmidx::entry_flags;

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
        assert_eq!(ENTRY_SIZE, 96);
    }

    #[test]
    fn roundtrip() {
        let e = sample_entry();
        let bytes = e.encode();
        assert_eq!(bytes.len(), ENTRY_SIZE);
        let decoded = EntryRecord::decode(&bytes).expect("valid record decodes");
        assert_eq!(decoded, e);
    }

    #[test]
    fn crc_mismatch_detected() {
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
