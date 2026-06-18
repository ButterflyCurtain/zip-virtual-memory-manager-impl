//! STORE プロバイダ（メソッド 0、無圧縮）。
//!
//! 展開データは圧縮データそのもの。ランダムアクセスは `data_offset` からの
//! 直接アドレス計算で、チェックポイントは持たない（設計: 本体 STORE 節、
//! vmidx Section 5 の STORE）。

use super::{check_range, CompressionProvider, ProviderError};
use crate::vmidx::{Checkpoint, ProviderType};

/// 無圧縮エントリのプロバイダ。
pub struct StoreProvider;

impl CompressionProvider for StoreProvider {
    fn provider_type(&self) -> ProviderType {
        ProviderType::Store
    }

    fn method_code(&self) -> u16 {
        0
    }

    /// STORE はチェックポイントを持たない（`chunk_head_offset = 0` のまま）。
    fn build_checkpoints(
        &self,
        _compressed: &[u8],
        _uncompressed_size: u64,
        _interval: u64,
    ) -> Result<Vec<Checkpoint>, ProviderError> {
        Ok(Vec::new())
    }

    /// 圧縮データ＝展開データなので `[offset, offset + len)` を直接切り出す。
    /// `from` は STORE では常に `None`（チェックポイントが無い）。
    fn read_range(
        &self,
        compressed: &[u8],
        from: Option<&Checkpoint>,
        offset: u64,
        len: usize,
        uncompressed_size: u64,
    ) -> Result<Vec<u8>, ProviderError> {
        if let Some(cp) = from {
            return Err(ProviderError::CheckpointMismatch {
                expected: ProviderType::Store,
                found: cp.provider(),
            });
        }
        check_range(offset, len, uncompressed_size)?;
        // STORE では compressed.len() == uncompressed_size のはずだが、ストリーム
        // 側の切り詰めにも備えて境界を確認する。
        let start = offset as usize;
        let end = start
            .checked_add(len)
            .filter(|&e| e <= compressed.len())
            .ok_or(ProviderError::CorruptStream("stored data shorter than entry size"))?;
        Ok(compressed[start..end].to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata() {
        let p = StoreProvider;
        assert_eq!(p.provider_type(), ProviderType::Store);
        assert_eq!(p.method_code(), 0);
    }

    #[test]
    fn no_checkpoints() {
        let p = StoreProvider;
        let cps = p.build_checkpoints(b"anything", 8, 1 << 20).unwrap();
        assert!(cps.is_empty());
    }

    #[test]
    fn reads_full_and_subranges() {
        let p = StoreProvider;
        let data = b"hello world";
        assert_eq!(
            p.read_range(data, None, 0, data.len(), data.len() as u64)
                .unwrap(),
            data
        );
        assert_eq!(
            p.read_range(data, None, 6, 5, data.len() as u64).unwrap(),
            b"world"
        );
        assert_eq!(
            p.read_range(data, None, 0, 0, data.len() as u64).unwrap(),
            b""
        );
    }

    #[test]
    fn out_of_range_is_rejected() {
        let p = StoreProvider;
        let data = b"abcd";
        match p.read_range(data, None, 2, 5, data.len() as u64) {
            Err(ProviderError::OutOfRange { .. }) => {}
            other => panic!("expected OutOfRange, got {other:?}"),
        }
    }

    #[test]
    fn checkpoint_for_store_is_rejected() {
        let p = StoreProvider;
        let cp = Checkpoint::Zstd {
            frame_offset: 0,
            uncompressed_offset: 0,
        };
        match p.read_range(b"abcd", Some(&cp), 0, 1, 4) {
            Err(ProviderError::CheckpointMismatch { .. }) => {}
            other => panic!("expected CheckpointMismatch, got {other:?}"),
        }
    }

    #[test]
    fn truncated_store_stream_is_corrupt() {
        // uncompressed_size はエントリ申告値だが、実バイトが足りない場合。
        let p = StoreProvider;
        let data = b"abc"; // 3 バイトしかない
        match p.read_range(data, None, 0, 5, 5) {
            // check_range は size=5 を通すが、compressed が 3 バイトで足りない。
            Err(ProviderError::CorruptStream(_)) => {}
            other => panic!("expected CorruptStream, got {other:?}"),
        }
    }
}
