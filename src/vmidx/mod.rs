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
//! 現状の実装範囲:
//! - [`Header`] / [`EntryRecord`]: 固定幅構造の encode/decode と CRC-32C 検証
//! - [`EntryIndexBuilder`] / [`EntryIndex`]: NAME HEAP とエントリテーブルの
//!   組み立てと、`name_hash` 二分探索による `path` ルックアップ
//! - [`CheckpointChunk`] / [`Checkpoint`]: CHECKPOINT CHUNK の decode と、
//!   目標オフセット以下で最も近いチェックポイントの探索
//! - [`Advisory`] / [`BlockState`]: 性能ヒントの ADVISORY 領域の
//!   encode/parse とサイズ計算（破棄しても正しさには影響しない）
//!
//! 全整数はリトルエンディアン、全オフセットはファイル先頭からのバイト
//! オフセット。設計: docs `ZIP_Virtual_Memory_Manager_vmidx_Index_Spec`。

mod advisory;
mod checkpoint;
mod entry;
mod header;
mod table;

pub use advisory::{Advisory, BLOCK_STATE_SIZE, BlockState, advisory_size, block_state_flags};
pub use checkpoint::{
    CHUNK_HEADER_SIZE, Checkpoint, CheckpointChunk, nearest_checkpoint,
};
pub use entry::{EntryRecord, ProviderType};
pub use header::Header;
pub use table::{EntryIndex, EntryIndexBuilder, hash_name};

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
    /// CHECKPOINT CHUNK のマジックが一致しない。
    ChunkBadMagic,
    /// CHECKPOINT CHUNK の CRC-32C 不一致。
    ChunkCrcMismatch { stored: u32, computed: u32 },
    /// CHECKPOINT CHUNK がバッファ末尾で切り詰められている。
    ChunkTruncated,
    /// チェックポイント記録形式を持たないプロバイダ（STORE / UNSUPPORTED）の
    /// チャンクが現れた。
    ChunkNoCheckpointFormat(ProviderType),
    /// チャンクチェーンが循環している（不正な `next_chunk_offset`）。
    ChunkChainCycle,
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
            DecodeError::ChunkBadMagic => write!(f, "vmidx: bad checkpoint chunk magic"),
            DecodeError::ChunkCrcMismatch { stored, computed } => write!(
                f,
                "vmidx: checkpoint chunk CRC mismatch (stored {stored:#010x}, computed {computed:#010x})"
            ),
            DecodeError::ChunkTruncated => write!(f, "vmidx: checkpoint chunk truncated"),
            DecodeError::ChunkNoCheckpointFormat(p) => {
                write!(f, "vmidx: provider {p:?} has no checkpoint record format")
            }
            DecodeError::ChunkChainCycle => write!(f, "vmidx: checkpoint chunk chain cycle"),
        }
    }
}

impl std::error::Error for DecodeError {}

#[inline]
pub(crate) fn rd_u16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(b[off..off + 2].try_into().unwrap())
}

#[inline]
pub(crate) fn rd_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}

#[inline]
pub(crate) fn rd_u64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().unwrap())
}
