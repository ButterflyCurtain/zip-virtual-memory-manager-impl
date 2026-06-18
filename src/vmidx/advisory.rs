//! ADVISORY REGION（仕様 Section 4）。
//!
//! 性能チューニング用の状態だけを置く領域。失われても性能が変わるだけで、
//! 正しさには影響しない。CRC を持たず、読み出し時に妥当性で打ち切る
//! （sanity-clamp）：不整合な領域は信頼せずゼロにリセットする。
//!
//! レイアウト:
//! ```text
//! [ miss_count        : entry_count × u64 ]  エントリ毎のページキャッシュ
//!                                            ミス数（エントリテーブルと並行）
//! [ block_state_count : u64               ]
//! [ block_state       : count × 24 バイト ]  疎な VMM ネイティブブロック追跡
//! ```
//!
//! このリビジョンは領域全体の encode / parse / サイズ計算（ファイル
//! レイアウト確定に必要な分）を実装する。in-place 更新（close() 時や
//! セッション中の書き戻し、Section 6.1）は別途。

use super::rd_u64;

/// BLOCK STATE RECORD の固定長（バイト）。
pub const BLOCK_STATE_SIZE: usize = 24;

/// BLOCK STATE RECORD の `state_flags`（8 ビット）のビット定義。
pub mod block_state_flags {
    /// 直近のコミットでオーバーフローが起きた。
    pub const OVERFLOW: u8 = 1 << 0;
    /// 容量縮小の候補（連続して低充填）。
    pub const SHRINK_CANDIDATE: u8 = 1 << 1;
}

/// BLOCK STATE RECORD（24 バイト、履歴のあるブロックにのみ存在）。
///
/// VMM ネイティブ DEFLATE のオーバーフロー余裕と容量縮小ポリシ（メイン
/// アーキテクチャ文書 VMM-NATIVE DEFLATE）の入力。チェックポイント記録から
/// 切り離すことで DEFLATE_VMM の 24 バイト固定長を保つ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockState {
    /// エントリテーブル内のインデックス。
    pub entry_index: u64,
    /// エントリ内のブロックインデックス。
    pub block_index: u64,
    pub state_flags: u8,
    /// 直近コミットでのオーバーフロー回数。
    pub overflow_count: u8,
    /// 充填 60% 未満が続いた連続コミット数。
    pub shrink_streak: u8,
}

impl BlockState {
    fn encode_into(&self, dst: &mut [u8]) {
        dst[0..8].copy_from_slice(&self.entry_index.to_le_bytes());
        dst[8..16].copy_from_slice(&self.block_index.to_le_bytes());
        dst[16] = self.state_flags;
        dst[17] = self.overflow_count;
        dst[18] = self.shrink_streak;
        // [19..24] reserved = 0
    }

    fn decode(b: &[u8]) -> BlockState {
        BlockState {
            entry_index: rd_u64(b, 0),
            block_index: rd_u64(b, 8),
            state_flags: b[16],
            overflow_count: b[17],
            shrink_streak: b[18],
        }
    }
}

/// ADVISORY REGION のインメモリ表現。`miss_count` の長さは entry_count に
/// 等しい（エントリテーブルと並行）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Advisory {
    pub miss_count: Vec<u64>,
    pub block_state: Vec<BlockState>,
}

impl Advisory {
    /// 全カウンタをゼロにした空の領域（`entry_count` 個の `miss_count`、
    /// block_state なし）。新規 vmidx と sanity-clamp 失敗時の既定値。
    pub fn zeroed(entry_count: usize) -> Advisory {
        Advisory {
            miss_count: vec![0; entry_count],
            block_state: Vec::new(),
        }
    }

    /// この領域がディスク上で占めるバイト数。FILE HEADER の `advisory_size`。
    pub fn byte_size(&self) -> usize {
        self.miss_count.len() * 8 + 8 + self.block_state.len() * BLOCK_STATE_SIZE
    }

    /// `dst`（長さはちょうど [`byte_size`](Self::byte_size)）へエンコードする。
    pub fn encode_into(&self, dst: &mut [u8]) {
        let mut pos = 0;
        for &m in &self.miss_count {
            dst[pos..pos + 8].copy_from_slice(&m.to_le_bytes());
            pos += 8;
        }
        dst[pos..pos + 8].copy_from_slice(&(self.block_state.len() as u64).to_le_bytes());
        pos += 8;
        for bs in &self.block_state {
            bs.encode_into(&mut dst[pos..pos + BLOCK_STATE_SIZE]);
            pos += BLOCK_STATE_SIZE;
        }
    }

    /// ADVISORY REGION をパースする。CRC を持たない領域なので、サイズや
    /// `block_state_count` が領域に収まらないなど不整合があれば、信頼せずに
    /// [`Advisory::zeroed`] を返す（Section 4 の sanity-clamp）。
    pub fn parse(region: &[u8], entry_count: usize) -> Advisory {
        let miss_bytes = entry_count * 8;
        // miss_count 配列 + block_state_count が収まらなければ信頼しない。
        if region.len() < miss_bytes + 8 {
            return Advisory::zeroed(entry_count);
        }
        let mut miss_count = Vec::with_capacity(entry_count);
        for i in 0..entry_count {
            miss_count.push(rd_u64(region, i * 8));
        }
        let count = rd_u64(region, miss_bytes);
        let Ok(count) = usize::try_from(count) else {
            return Advisory::zeroed(entry_count);
        };
        let need = match count.checked_mul(BLOCK_STATE_SIZE) {
            Some(n) => miss_bytes + 8 + n,
            None => return Advisory::zeroed(entry_count),
        };
        if region.len() < need {
            return Advisory::zeroed(entry_count);
        }
        let mut block_state = Vec::with_capacity(count);
        let mut pos = miss_bytes + 8;
        for _ in 0..count {
            block_state.push(BlockState::decode(&region[pos..pos + BLOCK_STATE_SIZE]));
            pos += BLOCK_STATE_SIZE;
        }
        Advisory {
            miss_count,
            block_state,
        }
    }
}

/// entry_count と block_state 件数から ADVISORY REGION のサイズを求める
/// （[`Advisory`] を組み立てずにレイアウト計算したいとき用）。
pub fn advisory_size(entry_count: usize, block_state_count: usize) -> usize {
    entry_count * 8 + 8 + block_state_count * BLOCK_STATE_SIZE
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Advisory {
        Advisory {
            miss_count: vec![0, 5, 9_999, u64::MAX, 1],
            block_state: vec![
                BlockState {
                    entry_index: 1,
                    block_index: 0,
                    state_flags: block_state_flags::OVERFLOW,
                    overflow_count: 3,
                    shrink_streak: 0,
                },
                BlockState {
                    entry_index: 3,
                    block_index: 7,
                    state_flags: block_state_flags::SHRINK_CANDIDATE,
                    overflow_count: 0,
                    shrink_streak: 12,
                },
            ],
        }
    }

    #[test]
    fn block_state_size_matches_spec() {
        assert_eq!(BLOCK_STATE_SIZE, 24);
    }

    #[test]
    fn roundtrip() {
        let a = sample();
        let mut buf = vec![0u8; a.byte_size()];
        a.encode_into(&mut buf);
        assert_eq!(buf.len(), advisory_size(5, 2));
        let decoded = Advisory::parse(&buf, 5);
        assert_eq!(decoded, a);
    }

    #[test]
    fn zeroed_has_entry_count_misses_and_no_blocks() {
        let a = Advisory::zeroed(4);
        assert_eq!(a.miss_count, vec![0; 4]);
        assert!(a.block_state.is_empty());
        // 空でも block_state_count フィールドぶんの 8 バイトは占める。
        assert_eq!(a.byte_size(), 4 * 8 + 8);
    }

    #[test]
    fn parse_clamps_truncated_region_to_zero() {
        let a = sample();
        let mut buf = vec![0u8; a.byte_size()];
        a.encode_into(&mut buf);
        // block_state の途中で切る → 信頼せずゼロへ。
        let truncated = &buf[..buf.len() - 4];
        assert_eq!(Advisory::parse(truncated, 5), Advisory::zeroed(5));
    }

    #[test]
    fn parse_clamps_implausible_block_count() {
        // miss_count(entry_count=2)=16B のあと、巨大な block_state_count。
        let mut buf = vec![0u8; 2 * 8 + 8];
        buf[16..24].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(Advisory::parse(&buf, 2), Advisory::zeroed(2));
    }

    #[test]
    fn miss_count_parallels_entry_table() {
        // miss_count の長さは entry_count に等しい（エントリテーブルと並行)。
        let a = Advisory::zeroed(3);
        assert_eq!(a.miss_count.len(), 3);
    }
}
