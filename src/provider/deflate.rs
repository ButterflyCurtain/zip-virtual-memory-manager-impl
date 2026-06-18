//! 標準（サードパーティ）DEFLATE プロバイダ（メソッド 8）。
//!
//! DEFLATE はビットストリームで、ブロックがバイト境界に揃わない。任意地点から
//! デコードを再開するには、(1) 端数ビットを `inflatePrime` で再注入し、(2) 直前
//! 32 KB の展開出力を `inflateSetDictionary` で辞書として流し込み、(3) raw
//! inflate（`inflateInit2(-15)`）で前進デコードする。これは zlib の `zran.c` と
//! 同じ方式で、`libz-rs-sys`（zlib-rs、zlib C API の純 Rust 実装）の上に組む。
//! 設計の難所であり「最も静かに壊れる」箇所（IMPLEMENTATION_NOTES 参照）なので、
//! unsafe FFI は [`RawInflater`] に隔離し、外には安全な境界だけ出す。
//!
//! 索引構築（[`build_checkpoints`](DeflateProvider::build_checkpoints)）は正しさ
//! を優先し、ストリームを 1 度フル展開しながら DEFLATE ブロック境界で
//! チェックポイントを置く。**展開出力をメモリに保持する**ため巨大エントリでは
//! 重い（スライディングウィンドウ方式への置き換えは後段の最適化）。読み取り
//! （[`read_range`](DeflateProvider::read_range)）は最近チェックポイントから
//! 復元して必要範囲だけ前進デコードするので、メモリも仕事量も範囲に比例する。

use super::{check_range, CompressionProvider, ProviderError};
use crate::vmidx::{Checkpoint, ProviderType, DEFLATE_WINDOW_SIZE};
use libz_rs_sys as z;
use std::os::raw::c_int;

/// 1 回の `inflate`/`deflate` 呼び出しに渡す入力チャンク上限（`avail_in` は
/// 32 ビットなので 4 GiB 未満に刻む）。
const IN_CHUNK: usize = 1 << 30;
/// 前進デコード時の出力スクラッチ長。
const OUT_SCRATCH: usize = 64 * 1024;

/// raw inflate（windowBits = -15）の z_stream を RAII で包む。`Drop` で
/// `inflateEnd` する。`next_in` / `next_out` は呼び出し側が各 `inflate` 前に
/// セットする。
struct RawInflater {
    strm: z::z_stream,
}

impl RawInflater {
    fn new() -> Result<RawInflater, ProviderError> {
        // z_stream::default() が rust-allocator（zalloc/zfree）を設定する。
        let mut strm = z::z_stream::default();
        let ret = unsafe {
            z::inflateInit2_(
                &mut strm,
                -15,
                z::zlibVersion(),
                core::mem::size_of::<z::z_stream>() as c_int,
            )
        };
        if ret != z::Z_OK {
            return Err(ProviderError::CorruptStream("inflateInit2 failed"));
        }
        Ok(RawInflater { strm })
    }

    /// 端数ビットの再注入（再開地点が非バイト境界のとき）。
    fn prime(&mut self, bits: u8, value: u8) -> Result<(), ProviderError> {
        let ret = unsafe { z::inflatePrime(&mut self.strm, bits as c_int, value as c_int) };
        if ret != z::Z_OK {
            return Err(ProviderError::CorruptStream("inflatePrime failed"));
        }
        Ok(())
    }

    /// 直前 32 KB の展開出力を辞書としてセットする（raw inflate では init 直後に
    /// 呼べる）。
    fn set_dictionary(&mut self, dict: &[u8]) -> Result<(), ProviderError> {
        let ret =
            unsafe { z::inflateSetDictionary(&mut self.strm, dict.as_ptr(), dict.len() as _) };
        if ret != z::Z_OK {
            return Err(ProviderError::CorruptStream("inflateSetDictionary failed"));
        }
        Ok(())
    }
}

impl Drop for RawInflater {
    fn drop(&mut self) {
        unsafe {
            z::inflateEnd(&mut self.strm);
        }
    }
}

/// 標準 DEFLATE のプロバイダ。
pub struct DeflateProvider;

impl DeflateProvider {
    /// `compressed` の `in_pos` 以降を `inf` に供給して 1 回 `inflate(flush)` し、
    /// `(返り値, 生成バイト数, 更新後 in_pos)` を返す。入力を使い切ったら次の
    /// チャンクを継ぎ足す。出力は `scratch` に書かれる。
    fn pump(
        inf: &mut RawInflater,
        compressed: &[u8],
        in_pos: &mut usize,
        scratch: &mut [u8],
        flush: c_int,
    ) -> (c_int, usize) {
        if inf.strm.avail_in == 0 && *in_pos < compressed.len() {
            let chunk = (compressed.len() - *in_pos).min(IN_CHUNK);
            inf.strm.next_in = compressed[*in_pos..].as_ptr();
            inf.strm.avail_in = chunk as _;
            *in_pos += chunk;
        }
        inf.strm.next_out = scratch.as_mut_ptr();
        inf.strm.avail_out = scratch.len() as _;
        let before = inf.strm.avail_out;
        let ret = unsafe { z::inflate(&mut inf.strm, flush) };
        let produced = (before - inf.strm.avail_out) as usize;
        (ret, produced)
    }
}

impl CompressionProvider for DeflateProvider {
    fn provider_type(&self) -> ProviderType {
        ProviderType::Deflate
    }

    fn method_code(&self) -> u16 {
        8
    }

    fn build_checkpoints(
        &self,
        compressed: &[u8],
        uncompressed_size: u64,
        interval: u64,
    ) -> Result<Vec<Checkpoint>, ProviderError> {
        if uncompressed_size == 0 {
            return Ok(Vec::new());
        }
        let interval = interval.max(1);
        let mut inf = RawInflater::new()?;
        // 正しさ優先でフル展開を保持する（索引構築は 1 度きり。zip-bomb ガードと
        // して uncompressed_size を超えたら打ち切る）。
        let cap = uncompressed_size.min(64 << 20) as usize;
        let mut out: Vec<u8> = Vec::with_capacity(cap);
        let mut scratch = vec![0u8; OUT_SCRATCH];
        let mut checkpoints: Vec<Checkpoint> = Vec::new();
        let mut last_cp_out: u64 = 0;
        let mut in_pos = 0usize;

        loop {
            let (ret, produced) =
                Self::pump(&mut inf, compressed, &mut in_pos, &mut scratch, z::Z_BLOCK);
            if out.len() + produced > uncompressed_size as usize {
                return Err(ProviderError::CorruptStream(
                    "inflate produced more than declared size",
                ));
            }
            out.extend_from_slice(&scratch[..produced]);

            match ret {
                z::Z_OK | z::Z_STREAM_END | z::Z_BUF_ERROR => {}
                _ => return Err(ProviderError::CorruptStream("inflate error during build")),
            }

            // data_type: bit7(128)=ブロック末尾にいる, bit6(64)=最終ブロック,
            // bits0..2(&7)=最後のバイトの未使用ビット数（再開用の端数ビット）。
            let at_block = (inf.strm.data_type & 128) != 0;
            let last_block = (inf.strm.data_type & 64) != 0;
            let totout = out.len() as u64;
            let due = checkpoints.is_empty() || totout.saturating_sub(last_cp_out) >= interval;
            if at_block && !last_block && totout > 0 && due {
                // 消費済み圧縮バイト位置を自前で求める（zlib の total_in は
                // Windows では c_ulong=32 ビットで 4 GiB 超で wrap するため使わない）。
                let consumed_in = (in_pos - inf.strm.avail_in as usize) as u64;
                checkpoints.push(make_checkpoint(
                    &out,
                    consumed_in,
                    totout,
                    (inf.strm.data_type & 7) as u8,
                ));
                last_cp_out = totout;
            }

            if ret == z::Z_STREAM_END {
                break;
            }
            // 進展が無く入力も尽きた（切り詰め/壊れ）。これ以上は進めない。
            if produced == 0 && inf.strm.avail_in == 0 && in_pos >= compressed.len() {
                break;
            }
        }
        Ok(checkpoints)
    }

    fn read_range(
        &self,
        compressed: &[u8],
        from: Option<&Checkpoint>,
        offset: u64,
        len: usize,
        uncompressed_size: u64,
    ) -> Result<Vec<u8>, ProviderError> {
        check_range(offset, len, uncompressed_size)?;
        if len == 0 {
            return Ok(Vec::new());
        }

        // 起点（base_out=その地点の展開オフセット, in_start=圧縮バイト位置,
        // bits=端数ビット, dict=辞書）を決める。
        let (base_out, in_start, bits, window): (u64, usize, u8, Option<&[u8]>) = match from {
            None => (0, 0, 0, None),
            Some(Checkpoint::Deflate {
                compressed_offset,
                uncompressed_offset,
                bits,
                window,
            }) => {
                if *uncompressed_offset > offset {
                    return Err(ProviderError::CorruptStream("checkpoint past target offset"));
                }
                (
                    *uncompressed_offset,
                    *compressed_offset as usize,
                    *bits,
                    Some(&window[..]),
                )
            }
            Some(other) => {
                return Err(ProviderError::CheckpointMismatch {
                    expected: ProviderType::Deflate,
                    found: other.provider(),
                });
            }
        };

        let mut inf = RawInflater::new()?;
        if bits > 0 {
            // 直前バイトの下位 `bits` ビットを注入する（zran と同じ。in_start は
            // 端数ビットの「次」のバイトを指すので、その 1 つ前を読む）。
            let prev_idx = in_start
                .checked_sub(1)
                .ok_or(ProviderError::CorruptStream("prime needs a preceding byte"))?;
            let prev = *compressed
                .get(prev_idx)
                .ok_or(ProviderError::CorruptStream("prime byte out of range"))?;
            inf.prime(bits, prev >> (8 - bits))?;
        }
        if let Some(w) = window {
            // 有効辞書長 = min(32768, base_out)。window は右詰めなので末尾を採る。
            let valid = (DEFLATE_WINDOW_SIZE as u64).min(base_out) as usize;
            inf.set_dictionary(&w[DEFLATE_WINDOW_SIZE - valid..])?;
        }

        let mut to_skip = (offset - base_out) as usize;
        let mut result: Vec<u8> = Vec::with_capacity(len);
        let mut scratch = vec![0u8; OUT_SCRATCH];
        let mut in_pos = in_start;

        loop {
            let (ret, produced) =
                Self::pump(&mut inf, compressed, &mut in_pos, &mut scratch, z::Z_NO_FLUSH);

            let mut chunk: &[u8] = &scratch[..produced];
            if to_skip > 0 {
                let s = to_skip.min(chunk.len());
                chunk = &chunk[s..];
                to_skip -= s;
            }
            if !chunk.is_empty() && result.len() < len {
                let take = (len - result.len()).min(chunk.len());
                result.extend_from_slice(&chunk[..take]);
            }
            if result.len() >= len {
                return Ok(result);
            }

            match ret {
                z::Z_OK => {}
                z::Z_STREAM_END => {
                    return Err(ProviderError::CorruptStream(
                        "stream ended before target range was satisfied",
                    ));
                }
                z::Z_BUF_ERROR => {
                    if produced == 0 && inf.strm.avail_in == 0 && in_pos >= compressed.len() {
                        return Err(ProviderError::CorruptStream(
                            "compressed stream ended before target range",
                        ));
                    }
                }
                z::Z_NEED_DICT => {
                    return Err(ProviderError::CorruptStream("unexpected need-dict"));
                }
                _ => return Err(ProviderError::CorruptStream("inflate error")),
            }
        }
    }
}

/// `out`（先頭からの全展開出力）と現在地から DEFLATE チェックポイントを作る。
/// window は直前 min(32768, totout) バイトを右詰め（先頭ゼロ埋め）で格納する。
fn make_checkpoint(out: &[u8], total_in: u64, totout: u64, bits: u8) -> Checkpoint {
    let mut window = Box::new([0u8; DEFLATE_WINDOW_SIZE]);
    let valid = DEFLATE_WINDOW_SIZE.min(totout as usize);
    window[DEFLATE_WINDOW_SIZE - valid..].copy_from_slice(&out[totout as usize - valid..]);
    Checkpoint::Deflate {
        compressed_offset: total_in,
        uncompressed_offset: totout,
        bits,
        window,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// libz-rs-sys で生バイト列を raw DEFLATE 圧縮する（テスト用フィクスチャ）。
    fn raw_deflate(data: &[u8]) -> Vec<u8> {
        let mut strm = z::z_stream::default();
        let mut out = vec![0u8; data.len() + data.len() / 2 + 1024];
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
            assert_eq!(r, z::Z_OK, "deflateInit2 failed");
            strm.next_in = data.as_ptr();
            strm.avail_in = data.len() as _;
            strm.next_out = out.as_mut_ptr();
            strm.avail_out = out.len() as _;
            let r = z::deflate(&mut strm, z::Z_FINISH);
            assert_eq!(r, z::Z_STREAM_END, "deflate did not finish in one shot");
            let produced = out.len() - strm.avail_out as usize;
            out.truncate(produced);
            z::deflateEnd(&mut strm);
        }
        out
    }

    /// 圧縮可能だが変化のあるデータ（複数の DEFLATE ブロックを生む）。
    fn sample_data(n: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(n);
        let mut state: u32 = 0x1234_5678;
        for i in 0..n {
            // LCG で擬似乱数だが、値域を絞って圧縮可能にする。
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let byte = ((state >> 24) as u8 & 0x1f) ^ (i as u8 & 0x07);
            v.push(byte);
        }
        v
    }

    /// `offset` 以下で最大の uncompressed_offset を持つチェックポイントを採る。
    fn nearest<'a>(cps: &'a [Checkpoint], offset: u64) -> Option<&'a Checkpoint> {
        cps.iter()
            .filter(|c| c.uncompressed_offset() <= offset)
            .max_by_key(|c| c.uncompressed_offset())
    }

    #[test]
    fn metadata() {
        let p = DeflateProvider;
        assert_eq!(p.provider_type(), ProviderType::Deflate);
        assert_eq!(p.method_code(), 8);
    }

    #[test]
    fn read_from_start_matches_original() {
        let data = sample_data(200_000);
        let comp = raw_deflate(&data);
        let p = DeflateProvider;
        // from=None（先頭から）で全域・部分域が一致する。
        assert_eq!(
            p.read_range(&comp, None, 0, data.len(), data.len() as u64)
                .unwrap(),
            data
        );
        assert_eq!(
            p.read_range(&comp, None, 1000, 500, data.len() as u64).unwrap(),
            &data[1000..1500]
        );
    }

    #[test]
    fn builds_multiple_checkpoints() {
        let data = sample_data(200_000);
        let comp = raw_deflate(&data);
        let p = DeflateProvider;
        let cps = p
            .build_checkpoints(&comp, data.len() as u64, 16 * 1024)
            .unwrap();
        // 複数ブロックにまたがるはずで、少なくとも 1 つは置けている。
        assert!(!cps.is_empty(), "expected at least one checkpoint");
        for c in &cps {
            assert!(matches!(c, Checkpoint::Deflate { .. }));
            assert!(c.uncompressed_offset() < data.len() as u64);
        }
    }

    #[test]
    fn seek_via_checkpoint_matches_original() {
        let data = sample_data(300_000);
        let comp = raw_deflate(&data);
        let p = DeflateProvider;
        let cps = p
            .build_checkpoints(&comp, data.len() as u64, 8 * 1024)
            .unwrap();
        assert!(cps.len() >= 2, "need several checkpoints to exercise seek");

        // 端数ビット（bits > 0）を持つチェックポイントが含まれることを確かめる
        // （prime 経路を確実に踏む）。
        assert!(
            cps.iter().any(|c| matches!(c, Checkpoint::Deflate { bits, .. } if *bits > 0)),
            "expected at least one non-byte-aligned checkpoint"
        );

        // 各チェックポイント近傍・任意オフセットで読み出しが原データに一致する。
        for &offset in &[0u64, 5_000, 33_000, 100_000, 200_001, 299_000] {
            let len = ((data.len() as u64 - offset).min(1234)) as usize;
            let cp = nearest(&cps, offset);
            let got = p
                .read_range(&comp, cp, offset, len, data.len() as u64)
                .unwrap_or_else(|e| panic!("read at {offset} failed: {e}"));
            assert_eq!(
                got,
                &data[offset as usize..offset as usize + len],
                "mismatch at offset {offset} via checkpoint {:?}",
                cp.map(|c| c.uncompressed_offset())
            );
        }
    }

    #[test]
    fn out_of_range_rejected() {
        let data = sample_data(1000);
        let comp = raw_deflate(&data);
        let p = DeflateProvider;
        match p.read_range(&comp, None, 900, 200, data.len() as u64) {
            Err(ProviderError::OutOfRange { .. }) => {}
            other => panic!("expected OutOfRange, got {other:?}"),
        }
    }

    #[test]
    fn wrong_checkpoint_kind_rejected() {
        let data = sample_data(1000);
        let comp = raw_deflate(&data);
        let p = DeflateProvider;
        let cp = Checkpoint::Zstd {
            frame_offset: 0,
            uncompressed_offset: 0,
        };
        match p.read_range(&comp, Some(&cp), 0, 10, data.len() as u64) {
            Err(ProviderError::CheckpointMismatch { .. }) => {}
            other => panic!("expected CheckpointMismatch, got {other:?}"),
        }
    }
}
