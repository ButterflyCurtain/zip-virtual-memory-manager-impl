//! CHECKPOINT CHUNK（可変長、片方向リンクのチェーン）。
//!
//! エントリの展開チェックポイントは 1 つ以上の CHECKPOINT CHUNK に格納され、
//! `next_chunk_offset` で連結される。EAGER / AOT は 1 エントリ 1 チャンク、
//! LAZY / BACKGROUND / 適応再インデックスはセッション中にチャンクを追記する
//! （仕様 Section 5）。チャンク内のチェックポイントは `uncompressed_offset`
//! 昇順。チェーン全体ではレンジが交差しうるため、リーダはチャンクごとに
//! 二分探索し、全チャンクを通して「≤target の最近チェックポイント」を採る。
//!
//! 全整数はリトルエンディアン、全オフセットはファイル先頭からのバイト
//! オフセット。WINDOWS_COMPRESSED（zlib 圧縮ウィンドウ + 記録オフセット配列）
//! の可変長 DEFLATE 記録は未対応（非圧縮ウィンドウ形式のみ実装）。

use super::{DecodeError, ProviderType, rd_u32, rd_u64};

/// CHECKPOINT CHUNK ヘッダ先頭のマジック。
pub const CHUNK_MAGIC: u32 = 0xCC40_1D01;
/// CHECKPOINT CHUNK ヘッダの固定長（バイト）。可変長のチェックポイント記録
/// 列はこの直後から始まる。
pub const CHUNK_HEADER_SIZE: usize = 32;
/// チャンク末尾の `chunk_crc32` の長さ（バイト）。
const CHUNK_CRC_SIZE: usize = 4;

/// DEFLATE のスライディングウィンドウスナップショット長。
pub const DEFLATE_WINDOW_SIZE: usize = 32_768;
/// DEFLATE チェックポイント記録長（非圧縮ウィンドウ形式）。
/// offsets(16) + bits(1) + window(32768) = 32785。任意ビット境界からの再開
/// （zran 方式）には端数ビット数が要るため、本実装は設計仕様の 32784B に
/// `bits` を 1 バイト足した 32785B を用いる（設計仕様にも反映予定）。
const DEFLATE_RECORD_SIZE: usize = 8 + 8 + 1 + DEFLATE_WINDOW_SIZE;
/// DEFLATE_VMM チェックポイント記録長。
const VMM_RECORD_SIZE: usize = 24;
/// ZSTD チェックポイント記録長。
const ZSTD_RECORD_SIZE: usize = 16;

/// プロバイダのチェックポイント記録長。記録形式を持たない（STORE /
/// UNSUPPORTED）プロバイダは `None`。
fn record_size(provider: ProviderType) -> Option<usize> {
    match provider {
        ProviderType::Deflate => Some(DEFLATE_RECORD_SIZE),
        ProviderType::DeflateVmm => Some(VMM_RECORD_SIZE),
        ProviderType::Zstd => Some(ZSTD_RECORD_SIZE),
        ProviderType::Store | ProviderType::Unsupported => None,
    }
}

/// プロバイダ別のチェックポイント記録。`uncompressed_offset` が並べ替えと
/// ルックアップのキー。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Checkpoint {
    /// 標準サードパーティ DEFLATE ストリーム。
    Deflate {
        /// DEFLATE ストリーム内のバイト位置（端数ビットの「次」のバイト＝
        /// zran の `here->in`）。`bits > 0` のとき実データは `compressed_offset - 1`
        /// から prime して再開する。
        compressed_offset: u64,
        uncompressed_offset: u64,
        /// 再開地点の符号が始まる端数ビット数（0–7）。`inflatePrime` で直前
        /// バイトの下位 `bits` ビットを再注入する（zran と同じ）。
        bits: u8,
        /// 直前の展開出力 32 768 バイト。`inflateSetDictionary` に渡す辞書で、
        /// 圧縮ストリームの一部ではない。先頭が不足する場合は右詰めでゼロ埋め。
        window: Box<[u8; DEFLATE_WINDOW_SIZE]>,
    },
    /// VMM ネイティブ DEFLATE ブロック。ブロックごとに 1 チェックポイントで、
    /// チェーンは in-place commit のブロックマップを兼ねる。
    DeflateVmm {
        compressed_offset: u64,
        uncompressed_offset: u64,
        capacity: u64,
    },
    /// ZSTD seekable frame。フレームごとに 1 チェックポイント。
    Zstd {
        frame_offset: u64,
        uncompressed_offset: u64,
    },
}

impl Checkpoint {
    /// このチェックポイントが展開ストリーム上で位置する先頭バイト
    /// （`uncompressed_offset`）。チャンク内整列とルックアップのキー。
    pub fn uncompressed_offset(&self) -> u64 {
        match self {
            Checkpoint::Deflate {
                uncompressed_offset,
                ..
            }
            | Checkpoint::DeflateVmm {
                uncompressed_offset,
                ..
            }
            | Checkpoint::Zstd {
                uncompressed_offset,
                ..
            } => *uncompressed_offset,
        }
    }

    /// この記録が属するプロバイダ種別。
    pub fn provider(&self) -> ProviderType {
        match self {
            Checkpoint::Deflate { .. } => ProviderType::Deflate,
            Checkpoint::DeflateVmm { .. } => ProviderType::DeflateVmm,
            Checkpoint::Zstd { .. } => ProviderType::Zstd,
        }
    }

    /// `dst`（長さはちょうど記録長）へエンコードする。
    fn encode_into(&self, dst: &mut [u8]) {
        match self {
            Checkpoint::Deflate {
                compressed_offset,
                uncompressed_offset,
                bits,
                window,
            } => {
                dst[0..8].copy_from_slice(&compressed_offset.to_le_bytes());
                dst[8..16].copy_from_slice(&uncompressed_offset.to_le_bytes());
                dst[16] = *bits;
                dst[17..17 + DEFLATE_WINDOW_SIZE].copy_from_slice(&window[..]);
            }
            Checkpoint::DeflateVmm {
                compressed_offset,
                uncompressed_offset,
                capacity,
            } => {
                dst[0..8].copy_from_slice(&compressed_offset.to_le_bytes());
                dst[8..16].copy_from_slice(&uncompressed_offset.to_le_bytes());
                dst[16..24].copy_from_slice(&capacity.to_le_bytes());
            }
            Checkpoint::Zstd {
                frame_offset,
                uncompressed_offset,
            } => {
                dst[0..8].copy_from_slice(&frame_offset.to_le_bytes());
                dst[8..16].copy_from_slice(&uncompressed_offset.to_le_bytes());
            }
        }
    }

    /// `provider` に応じて記録 1 件をデコードする。
    ///
    /// 呼び出し側の保証（唯一の呼び出し元は [`CheckpointChunk::decode`]）:
    /// 1. `record_size(provider).is_some()` — `decode` は `rsize` を
    ///    [`DecodeError::ChunkNoCheckpointFormat`] で先に弾いてから本関数へ入る。
    ///    ゆえに Store / Unsupported 枝は到達不能で `unreachable!` にしてある。
    /// 2. `b.len() >= rsize` — `decode` が `rsize` 幅で切り出して渡す。
    fn decode_record(provider: ProviderType, b: &[u8]) -> Checkpoint {
        match provider {
            ProviderType::Deflate => {
                let mut window = Box::new([0u8; DEFLATE_WINDOW_SIZE]);
                window.copy_from_slice(&b[17..17 + DEFLATE_WINDOW_SIZE]);
                Checkpoint::Deflate {
                    compressed_offset: rd_u64(b, 0),
                    uncompressed_offset: rd_u64(b, 8),
                    bits: b[16],
                    window,
                }
            }
            ProviderType::DeflateVmm => Checkpoint::DeflateVmm {
                compressed_offset: rd_u64(b, 0),
                uncompressed_offset: rd_u64(b, 8),
                capacity: rd_u64(b, 16),
            },
            ProviderType::Zstd => Checkpoint::Zstd {
                frame_offset: rd_u64(b, 0),
                uncompressed_offset: rd_u64(b, 8),
            },
            // record_size() が None を返すプロバイダはここに来ない（decode が先に弾く）。
            ProviderType::Store | ProviderType::Unsupported => {
                unreachable!("decode_record called for provider without a record format")
            }
        }
    }
}

/// デコード済みの 1 チャンク。`checkpoints` は `uncompressed_offset` 昇順で、
/// 全要素が `provider` と同じプロバイダ種別であることが不変条件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointChunk {
    /// エントリテーブル内のインデックス。
    pub entry_index: u64,
    pub provider: ProviderType,
    /// チェーン上の次チャンクへのファイルオフセット。0 = 末尾。
    pub next_chunk_offset: u64,
    pub checkpoints: Vec<Checkpoint>,
}

impl CheckpointChunk {
    /// エンコード後の総バイト数（ヘッダ + 記録列 + CRC）。
    pub fn encoded_len(&self) -> usize {
        let rsize = record_size(self.provider).expect("provider has a checkpoint record format");
        CHUNK_HEADER_SIZE + self.checkpoints.len() * rsize + CHUNK_CRC_SIZE
    }

    /// 1 チャンクへエンコードする。`chunk_crc32` はバイト `[0 .. header+V)` 上で
    /// 計算して末尾に書き込む。`provider` がチェックポイント記録形式を持たない
    /// 場合はパニックする（そうしたチャンクは生成しないため）。
    pub fn encode(&self) -> Vec<u8> {
        let rsize = record_size(self.provider).expect("provider has a checkpoint record format");
        let v = self.checkpoints.len() * rsize;
        let mut b = vec![0u8; CHUNK_HEADER_SIZE + v + CHUNK_CRC_SIZE];
        b[0..4].copy_from_slice(&CHUNK_MAGIC.to_le_bytes());
        b[4..12].copy_from_slice(&self.entry_index.to_le_bytes());
        b[12] = self.provider.code();
        // [13..16] reserved = 0
        b[16..24].copy_from_slice(&(self.checkpoints.len() as u64).to_le_bytes());
        b[24..32].copy_from_slice(&self.next_chunk_offset.to_le_bytes());
        let mut pos = CHUNK_HEADER_SIZE;
        for c in &self.checkpoints {
            debug_assert_eq!(c.provider(), self.provider, "checkpoint provider mismatch");
            c.encode_into(&mut b[pos..pos + rsize]);
            pos += rsize;
        }
        let crc = crc32c::crc32c(&b[0..CHUNK_HEADER_SIZE + v]);
        b[CHUNK_HEADER_SIZE + v..].copy_from_slice(&crc.to_le_bytes());
        b
    }

    /// `b` の先頭から 1 チャンクをデコードする。`b` はチャンク以降を含む
    /// （ファイル末尾までの）スライスでよく、チャンク長は自己記述で求める。
    /// 検証順は magic → 長さ → `chunk_crc32`。
    pub fn decode(b: &[u8]) -> Result<CheckpointChunk, DecodeError> {
        if b.len() < CHUNK_HEADER_SIZE {
            return Err(DecodeError::ChunkTruncated);
        }
        if rd_u32(b, 0) != CHUNK_MAGIC {
            return Err(DecodeError::ChunkBadMagic);
        }
        let entry_index = rd_u64(b, 4);
        let provider = ProviderType::from_code(b[12]);
        let count = rd_u64(b, 16);
        let next_chunk_offset = rd_u64(b, 24);

        let rsize = record_size(provider).ok_or(DecodeError::ChunkNoCheckpointFormat(provider))?;
        let count = usize::try_from(count).map_err(|_| DecodeError::ChunkTruncated)?;
        let v = count
            .checked_mul(rsize)
            .ok_or(DecodeError::ChunkTruncated)?;
        let crc_pos = CHUNK_HEADER_SIZE
            .checked_add(v)
            .ok_or(DecodeError::ChunkTruncated)?;
        let end = crc_pos
            .checked_add(CHUNK_CRC_SIZE)
            .ok_or(DecodeError::ChunkTruncated)?;
        if b.len() < end {
            return Err(DecodeError::ChunkTruncated);
        }

        let stored = rd_u32(b, crc_pos);
        let computed = crc32c::crc32c(&b[0..crc_pos]);
        if stored != computed {
            return Err(DecodeError::ChunkCrcMismatch { stored, computed });
        }

        let mut checkpoints = Vec::with_capacity(count);
        let mut pos = CHUNK_HEADER_SIZE;
        for _ in 0..count {
            checkpoints.push(Checkpoint::decode_record(provider, &b[pos..pos + rsize]));
            pos += rsize;
        }
        Ok(CheckpointChunk {
            entry_index,
            provider,
            next_chunk_offset,
            checkpoints,
        })
    }

    /// このチャンク内で `uncompressed_offset ≤ target` の最大の記録を返す。
    /// `checkpoints` の昇順整列を前提に二分探索する。割り当てなし。
    pub fn nearest_at_or_below(&self, target: u64) -> Option<&Checkpoint> {
        let i = self
            .checkpoints
            .partition_point(|c| c.uncompressed_offset() <= target);
        if i == 0 {
            None
        } else {
            Some(&self.checkpoints[i - 1])
        }
    }
}

/// `head_offset` から始まるチェーンを辿り、全チャンクを通して
/// `uncompressed_offset ≤ target` の最近チェックポイントを返す（仕様 Section 5、
/// read(path) 手順 4）。`file` はファイル全体（または少なくともチェックポイント
/// ゾーンを含む先頭からの）スライス。`head_offset = 0` はチェックポイント未生成。
///
/// 不正な `next_chunk_offset` による循環は、チャンク数の上界を超えた時点で
/// [`DecodeError::ChunkChainCycle`] として検出する（割り当てなしのガード）。
pub fn nearest_checkpoint(
    file: &[u8],
    head_offset: u64,
    target: u64,
) -> Result<Option<Checkpoint>, DecodeError> {
    // 整形式ファイルでは各チャンクは重ならず最小 CHUNK_HEADER_SIZE + CRC バイトを
    // 占めるので、非循環チェーンのチャンク数はこの上界を超えない。
    let max_chunks = file.len() / (CHUNK_HEADER_SIZE + CHUNK_CRC_SIZE) + 1;
    let mut best: Option<Checkpoint> = None;
    let mut off = head_offset;
    let mut seen = 0usize;
    while off != 0 {
        seen += 1;
        if seen > max_chunks {
            return Err(DecodeError::ChunkChainCycle);
        }
        let start = usize::try_from(off).map_err(|_| DecodeError::ChunkTruncated)?;
        if start >= file.len() {
            return Err(DecodeError::ChunkTruncated);
        }
        let chunk = CheckpointChunk::decode(&file[start..])?;
        if let Some(c) = chunk.nearest_at_or_below(target) {
            let better = best
                .as_ref()
                .is_none_or(|b| c.uncompressed_offset() >= b.uncompressed_offset());
            if better {
                best = Some(c.clone());
            }
        }
        off = chunk.next_chunk_offset;
    }
    Ok(best)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vmm(uncompressed_offset: u64) -> Checkpoint {
        Checkpoint::DeflateVmm {
            compressed_offset: uncompressed_offset / 2,
            uncompressed_offset,
            capacity: 4096,
        }
    }

    fn zstd(uncompressed_offset: u64) -> Checkpoint {
        Checkpoint::Zstd {
            frame_offset: uncompressed_offset / 3,
            uncompressed_offset,
        }
    }

    fn deflate(uncompressed_offset: u64, fill: u8) -> Checkpoint {
        Checkpoint::Deflate {
            compressed_offset: uncompressed_offset / 2,
            uncompressed_offset,
            bits: (uncompressed_offset % 8) as u8,
            window: Box::new([fill; DEFLATE_WINDOW_SIZE]),
        }
    }

    #[test]
    fn record_sizes_match_spec() {
        assert_eq!(CHUNK_HEADER_SIZE, 32);
        assert_eq!(DEFLATE_RECORD_SIZE, 32_785);
        assert_eq!(VMM_RECORD_SIZE, 24);
        assert_eq!(ZSTD_RECORD_SIZE, 16);
    }

    #[test]
    fn vmm_chunk_roundtrip() {
        let chunk = CheckpointChunk {
            entry_index: 7,
            provider: ProviderType::DeflateVmm,
            next_chunk_offset: 123_456,
            checkpoints: vec![vmm(0), vmm(4096), vmm(8192)],
        };
        let bytes = chunk.encode();
        assert_eq!(bytes.len(), chunk.encoded_len());
        assert_eq!(bytes.len(), CHUNK_HEADER_SIZE + 3 * VMM_RECORD_SIZE + 4);
        let decoded = CheckpointChunk::decode(&bytes).expect("valid chunk decodes");
        assert_eq!(decoded, chunk);
    }

    #[test]
    fn zstd_chunk_roundtrip() {
        let chunk = CheckpointChunk {
            entry_index: 0,
            provider: ProviderType::Zstd,
            next_chunk_offset: 0,
            checkpoints: vec![zstd(0), zstd(1 << 20)],
        };
        let decoded = CheckpointChunk::decode(&chunk.encode()).expect("valid chunk decodes");
        assert_eq!(decoded, chunk);
    }

    #[test]
    fn deflate_chunk_roundtrip_preserves_window() {
        let chunk = CheckpointChunk {
            entry_index: 3,
            provider: ProviderType::Deflate,
            next_chunk_offset: 0,
            checkpoints: vec![deflate(0, 0xAB), deflate(65_536, 0xCD)],
        };
        let bytes = chunk.encode();
        assert_eq!(bytes.len(), CHUNK_HEADER_SIZE + 2 * DEFLATE_RECORD_SIZE + 4);
        let decoded = CheckpointChunk::decode(&bytes).expect("valid chunk decodes");
        assert_eq!(decoded, chunk);
    }

    #[test]
    fn decode_rejects_bad_magic() {
        let mut bytes = CheckpointChunk {
            entry_index: 1,
            provider: ProviderType::Zstd,
            next_chunk_offset: 0,
            checkpoints: vec![zstd(0)],
        }
        .encode();
        bytes[0] ^= 0xff;
        assert_eq!(
            CheckpointChunk::decode(&bytes),
            Err(DecodeError::ChunkBadMagic)
        );
    }

    #[test]
    fn decode_detects_crc_mismatch() {
        let mut bytes = CheckpointChunk {
            entry_index: 1,
            provider: ProviderType::Zstd,
            next_chunk_offset: 0,
            checkpoints: vec![zstd(42)],
        }
        .encode();
        bytes[CHUNK_HEADER_SIZE] ^= 0x01; // 最初の記録の 1 ビットを反転
        match CheckpointChunk::decode(&bytes) {
            Err(DecodeError::ChunkCrcMismatch { .. }) => {}
            other => panic!("expected ChunkCrcMismatch, got {other:?}"),
        }
    }

    #[test]
    fn decode_rejects_store_provider() {
        // provider_type = STORE(0) のチャンクを手で組み立て、記録形式なしとして
        // 弾かれることを確認する。
        let mut b = vec![0u8; CHUNK_HEADER_SIZE + CHUNK_CRC_SIZE];
        b[0..4].copy_from_slice(&CHUNK_MAGIC.to_le_bytes());
        b[12] = ProviderType::Store.code();
        // count = 0、CRC を正しく付ける（弾きは記録形式の不在で起きるべき）。
        let crc = crc32c::crc32c(&b[0..CHUNK_HEADER_SIZE]);
        b[CHUNK_HEADER_SIZE..].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(
            CheckpointChunk::decode(&b),
            Err(DecodeError::ChunkNoCheckpointFormat(ProviderType::Store))
        );
    }

    #[test]
    fn decode_detects_truncation() {
        let bytes = CheckpointChunk {
            entry_index: 1,
            provider: ProviderType::Zstd,
            next_chunk_offset: 0,
            checkpoints: vec![zstd(0), zstd(1)],
        }
        .encode();
        // CRC の手前で切る。
        let truncated = &bytes[..bytes.len() - 2];
        assert_eq!(
            CheckpointChunk::decode(truncated),
            Err(DecodeError::ChunkTruncated)
        );
    }

    #[test]
    fn nearest_at_or_below_within_chunk() {
        let chunk = CheckpointChunk {
            entry_index: 0,
            provider: ProviderType::DeflateVmm,
            next_chunk_offset: 0,
            checkpoints: vec![vmm(0), vmm(100), vmm(200), vmm(300)],
        };
        // target 未満が無い。
        assert_eq!(chunk.nearest_at_or_below(0).unwrap().uncompressed_offset(), 0);
        // ちょうど一致。
        assert_eq!(
            chunk.nearest_at_or_below(200).unwrap().uncompressed_offset(),
            200
        );
        // 間の値は直前の記録。
        assert_eq!(
            chunk.nearest_at_or_below(250).unwrap().uncompressed_offset(),
            200
        );
        // 末尾より大きい値は最後の記録。
        assert_eq!(
            chunk.nearest_at_or_below(9999).unwrap().uncompressed_offset(),
            300
        );
    }

    #[test]
    fn nearest_at_or_below_below_first_is_none() {
        let chunk = CheckpointChunk {
            entry_index: 0,
            provider: ProviderType::DeflateVmm,
            next_chunk_offset: 0,
            checkpoints: vec![vmm(100), vmm(200)],
        };
        assert!(chunk.nearest_at_or_below(50).is_none());
    }

    #[test]
    fn chain_walk_takes_nearest_across_interleaved_chunks() {
        // 先頭パディング、chunk_a、chunk_b の順にファイルを組み、ヘッドは
        // chunk_b。チェーンは chunk_b → chunk_a。レンジは交差:
        // chunk_a = {0, 200}, chunk_b = {100, 300}。
        let mut file = vec![0u8; 8]; // オフセットが先頭でないことを確かめる前置パディング

        let off_a = file.len() as u64;
        let chunk_a = CheckpointChunk {
            entry_index: 0,
            provider: ProviderType::DeflateVmm,
            next_chunk_offset: 0, // チェーン末尾
            checkpoints: vec![vmm(0), vmm(200)],
        };
        file.extend_from_slice(&chunk_a.encode());

        let off_b = file.len() as u64;
        let chunk_b = CheckpointChunk {
            entry_index: 0,
            provider: ProviderType::DeflateVmm,
            next_chunk_offset: off_a, // chunk_a を指す
            checkpoints: vec![vmm(100), vmm(300)],
        };
        file.extend_from_slice(&chunk_b.encode());

        // target=250: chunk_b の 100 と chunk_a の 200 → 200 を採る。
        let got = nearest_checkpoint(&file, off_b, 250)
            .expect("walk ok")
            .expect("some checkpoint");
        assert_eq!(got.uncompressed_offset(), 200);

        // target=120: chunk_b の 100 と chunk_a の 0 → 100 を採る。
        let got = nearest_checkpoint(&file, off_b, 120)
            .expect("walk ok")
            .expect("some checkpoint");
        assert_eq!(got.uncompressed_offset(), 100);

        // head=0 はチェックポイント無し。
        assert_eq!(nearest_checkpoint(&file, 0, 999).expect("walk ok"), None);
    }

    #[test]
    fn chain_walk_detects_cycle() {
        // next が自分自身を指す自己ループ。上界ガードで検出されるべき。
        let mut file = vec![0u8; 4];
        let off = file.len() as u64;
        let chunk = CheckpointChunk {
            entry_index: 0,
            provider: ProviderType::Zstd,
            next_chunk_offset: off, // 自己ループ
            checkpoints: vec![zstd(0)],
        };
        file.extend_from_slice(&chunk.encode());
        assert_eq!(
            nearest_checkpoint(&file, off, 100),
            Err(DecodeError::ChunkChainCycle)
        );
    }
}
