//! vmdirty ジャーナル（追記専用のバイナリ形式 + 回復読み取り）。
//!
//! Diff Layer Tier 1（メモリ内）から Tier 2（ディスク）へ spill した dirty
//! ページを durable に保持する唯一のコピー。プロセスクラッシュを跨いで生き残り、
//! VMM に「完全で検証可能な回復基盤」を与える。設計: docs
//! `ZIP_Virtual_Memory_Manager_vmdirty_Journal_Spec`。
//!
//! このモジュールが受け持つのは **形式と回復読み取り** のみ:
//! - [`Header`]（FILE HEADER 88B）と 3 種のレコード
//!   （[`encode_data_record`] / [`encode_commit_marker`] /
//!   [`encode_metadata_record`]）の encode、いずれも末尾に CRC-32C。
//! - [`read_vmdirty`]: open() 時に 1 度だけ走る回復読み取り walk（Section 2）。
//!   ヘッダを検証し、レコードを順に辿り、COMMIT MARKER 境界で committed /
//!   uncommitted を分類した [`RecoveryResult`] を返す。
//!
//! まだ持たないのは: 実ファイル I/O（O_DSYNC / fdatasync、`VmdirtyWriter`）、
//! Tier 1↔Tier 2 の spill ポリシー、generation_id の生成（CSPRNG）、compaction。
//! それらは disk / mount 層の配線と一緒に後続の増分で足す。回復読み取りは
//! コーデックと同じく `&[u8]` 上で完結させ（呼び出し側が vmdirty を読み込む）、
//! 本リポの「mmap/バイト列は外から渡す」方針に揃える。
//!
//! 全整数はリトルエンディアン、全オフセットはファイル先頭から。CRC は全フィールド
//! **CRC-32C（Castagnoli）**。commit のエントリ CRC（ISO-HDLC）とは別物。

/// FILE HEADER の固定長（バイト）。
pub const HEADER_SIZE: usize = 88;
/// COMMIT MARKER の固定長（バイト）。
pub const COMMIT_MARKER_SIZE: usize = 40;
/// FILE HEADER 先頭のマジック: `"VMDIRTY\0"`。
pub const MAGIC: [u8; 8] = *b"VMDIRTY\0";
/// 現在サポートする `format_version`。
pub const FORMAT_VERSION: u16 = 1;
/// DATA RECORD 先頭のマジック。
pub const DATA_RECORD_MAGIC: u32 = 0xD1A5_EC0D;
/// METADATA RECORD 先頭のマジック。
pub const METADATA_RECORD_MAGIC: u32 = 0xD1A5_EC1D;
/// COMMIT MARKER 先頭のマジック。
pub const COMMIT_MARKER_MAGIC: u32 = 0xC0FF_EE42;

/// `source_cd_hash` フィールド長（XXH3-128 の 16B + ゼロ詰め 4B）。
const CD_HASH_SIZE: usize = 20;

/// FILE HEADER の `flags` フィールド（16 ビット）のビット定義。
pub mod flags {
    /// `--strict-fingerprint` で開かれた。
    pub const STRICT_FINGERPRINT: u16 = 1 << 0;
    /// `dirty_limit=0`（spill のみ）でマウントされた。
    pub const SPILL_ONLY: u16 = 1 << 1;
    /// 直前のセッションが commit 途中でクラッシュした。回復完了でクリアする。
    pub const RECOVERY_PENDING: u16 = 1 << 2;
}

/// METADATA RECORD の操作種別（`op_code`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpCode {
    /// 新規エントリ（`logical_size = 0`）。
    Create = 1,
    /// エントリ削除（tombstone）。
    Remove = 2,
    /// `truncate()` による論理サイズ変更。
    Resize = 3,
    /// 改名。
    Rename = 4,
}

impl OpCode {
    /// 16 ビットの `op_code` 値。
    pub fn code(self) -> u16 {
        self as u16
    }

    /// `op_code` 値から復元する。未知の値は `None`。
    pub fn from_code(code: u16) -> Option<OpCode> {
        match code {
            1 => Some(OpCode::Create),
            2 => Some(OpCode::Remove),
            3 => Some(OpCode::Resize),
            4 => Some(OpCode::Rename),
            _ => None,
        }
    }
}

/// METADATA RECORD のペイロード（操作種別ごと）。`entry_name` はレコード側に
/// 持つので、ここには操作固有のデータのみ入る。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetaOp {
    /// CREATE: ペイロードなし。
    Create,
    /// REMOVE: ペイロードなし（tombstone）。
    Remove,
    /// RESIZE: 新しい論理サイズ。縮小は当該より後ろのページの DATA RECORD を
    /// dead にする（回復時の再生で上書きされる）。
    Resize { new_size: u64 },
    /// RENAME: 新しいエントリ名。
    Rename { new_name: String },
}

impl MetaOp {
    /// この操作の `op_code`。
    pub fn op_code(&self) -> OpCode {
        match self {
            MetaOp::Create => OpCode::Create,
            MetaOp::Remove => OpCode::Remove,
            MetaOp::Resize { .. } => OpCode::Resize,
            MetaOp::Rename { .. } => OpCode::Rename,
        }
    }
}

/// FILE HEADER（88 バイト）の論理表現。`magic` / `format_version` /
/// `header_crc32` / 予約フィールドは encode 時に確定するので持たない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub flags: u16,
    /// セッションを一意に識別する 128 ビット（UUIDv4）。世代の連番ではない。
    pub generation_id: [u8; 16],
    pub source_file_size: u64,
    pub source_inode: u64,
    /// CD ブロックの XXH3-128（先頭 16B）+ ゼロ詰め 4B。
    pub source_cd_hash: [u8; CD_HASH_SIZE],
    /// 壁時計ナノ秒。情報目的のみで検証には使わない。
    pub created_at_ns: u64,
    pub page_size: u32,
}

impl Header {
    /// 88 バイトへ encode する。末尾手前に `header_crc32`（バイト `[0..80)` の
    /// CRC-32C）を書く。
    pub fn encode(&self) -> [u8; HEADER_SIZE] {
        let mut b = [0u8; HEADER_SIZE];
        b[0..8].copy_from_slice(&MAGIC);
        b[8..10].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        b[10..12].copy_from_slice(&self.flags.to_le_bytes());
        b[12..28].copy_from_slice(&self.generation_id);
        b[28..36].copy_from_slice(&self.source_file_size.to_le_bytes());
        b[36..44].copy_from_slice(&self.source_inode.to_le_bytes());
        b[44..64].copy_from_slice(&self.source_cd_hash);
        b[64..72].copy_from_slice(&self.created_at_ns.to_le_bytes());
        b[72..76].copy_from_slice(&self.page_size.to_le_bytes());
        // [76..80] reserved = 0
        let crc = crc32c::crc32c(&b[0..80]);
        b[80..84].copy_from_slice(&crc.to_le_bytes());
        // [84..88] padding = 0
        b
    }

    /// 88 バイトからヘッダを検証・復元する。失敗はステータスで返す（Section 2
    /// STEP 1）。`format_version > FORMAT_VERSION` は `VersionUnsupported`。
    pub fn decode(b: &[u8]) -> Result<Header, RecoveryStatus> {
        if b.len() < HEADER_SIZE {
            return Err(RecoveryStatus::HeaderTruncated);
        }
        if b[0..8] != MAGIC {
            return Err(RecoveryStatus::HeaderMagicBad);
        }
        let stored_crc = rd_u32(b, 80);
        if crc32c::crc32c(&b[0..80]) != stored_crc {
            return Err(RecoveryStatus::HeaderCrcBad);
        }
        if rd_u16(b, 8) > FORMAT_VERSION {
            return Err(RecoveryStatus::VersionUnsupported);
        }
        let mut generation_id = [0u8; 16];
        generation_id.copy_from_slice(&b[12..28]);
        let mut source_cd_hash = [0u8; CD_HASH_SIZE];
        source_cd_hash.copy_from_slice(&b[44..64]);
        Ok(Header {
            flags: rd_u16(b, 10),
            generation_id,
            source_file_size: rd_u64(b, 28),
            source_inode: rd_u64(b, 36),
            source_cd_hash,
            created_at_ns: rd_u64(b, 64),
            page_size: rd_u32(b, 72),
        })
    }
}

/// DATA RECORD を組み立てる（`46 + N + D` バイト）。末尾に
/// `record_crc32`（先頭から data 末尾までの CRC-32C）を付ける。1 回の write で
/// 書き出すための「完成したレコード列」を返す。
pub fn encode_data_record(
    generation_id: &[u8; 16],
    sequence_num: u64,
    entry_name: &str,
    page_index: u64,
    data: &[u8],
) -> Vec<u8> {
    let name = entry_name.as_bytes();
    let mut b = Vec::with_capacity(46 + name.len() + data.len());
    b.extend_from_slice(&DATA_RECORD_MAGIC.to_le_bytes());
    b.extend_from_slice(generation_id);
    b.extend_from_slice(&sequence_num.to_le_bytes());
    b.extend_from_slice(&(name.len() as u16).to_le_bytes());
    b.extend_from_slice(name);
    b.extend_from_slice(&page_index.to_le_bytes());
    b.extend_from_slice(&(data.len() as u32).to_le_bytes());
    b.extend_from_slice(data);
    let crc = crc32c::crc32c(&b);
    b.extend_from_slice(&crc.to_le_bytes());
    b
}

/// COMMIT MARKER を組み立てる（固定 40 バイト）。`commit_sequence` はこの
/// マーカーが含む最後の DATA/METADATA レコードの `sequence_num`。
pub fn encode_commit_marker(
    generation_id: &[u8; 16],
    commit_sequence: u64,
    page_count: u64,
) -> [u8; COMMIT_MARKER_SIZE] {
    let mut b = [0u8; COMMIT_MARKER_SIZE];
    b[0..4].copy_from_slice(&COMMIT_MARKER_MAGIC.to_le_bytes());
    b[4..20].copy_from_slice(generation_id);
    b[20..28].copy_from_slice(&commit_sequence.to_le_bytes());
    b[28..36].copy_from_slice(&page_count.to_le_bytes());
    let crc = crc32c::crc32c(&b[0..36]);
    b[36..40].copy_from_slice(&crc.to_le_bytes());
    b
}

/// METADATA RECORD を組み立てる（`36 + N + P` バイト）。`op` がペイロード長 P を
/// 決める（CREATE/REMOVE=0、RESIZE=8、RENAME=2+new_name_len）。
pub fn encode_metadata_record(
    generation_id: &[u8; 16],
    sequence_num: u64,
    entry_name: &str,
    op: &MetaOp,
) -> Vec<u8> {
    let name = entry_name.as_bytes();
    let mut b = Vec::with_capacity(36 + name.len() + 16);
    b.extend_from_slice(&METADATA_RECORD_MAGIC.to_le_bytes());
    b.extend_from_slice(generation_id);
    b.extend_from_slice(&sequence_num.to_le_bytes());
    b.extend_from_slice(&op.op_code().code().to_le_bytes());
    b.extend_from_slice(&(name.len() as u16).to_le_bytes());
    b.extend_from_slice(name);
    match op {
        MetaOp::Create | MetaOp::Remove => {}
        MetaOp::Resize { new_size } => b.extend_from_slice(&new_size.to_le_bytes()),
        MetaOp::Rename { new_name } => {
            let nn = new_name.as_bytes();
            b.extend_from_slice(&(nn.len() as u16).to_le_bytes());
            b.extend_from_slice(nn);
        }
    }
    let crc = crc32c::crc32c(&b);
    b.extend_from_slice(&crc.to_le_bytes());
    b
}

/// 回復読み取り walk で復元した 1 ページ分の dirty データ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirtyPage {
    pub entry_name: String,
    pub page_index: u64,
    pub data: Vec<u8>,
    pub sequence: u64,
}

/// 回復読み取り walk で復元した 1 件のエントリ操作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryOp {
    pub entry_name: String,
    pub op: MetaOp,
    pub sequence: u64,
}

/// 回復読み取りのステータス（Section 3）。`Ok` 以外はヘッダ段階の失敗で、
/// レコードは 1 件も復元されない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryStatus {
    Ok,
    HeaderTruncated,
    HeaderMagicBad,
    HeaderCrcBad,
    VersionUnsupported,
}

/// [`read_vmdirty`] の結果（Section 3）。データ安全性の判断（discard /
/// recover_committed / recover_all / abort）は呼び出し側に委ねる。本構造体は
/// committed と uncommitted を分類して提示するだけ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryResult {
    pub status: RecoveryStatus,
    /// このファイルを書いたセッションの generation_id（ヘッダ失敗時はゼロ）。
    pub generation_id: [u8; 16],
    /// `sequence ≤ last_commit_seq` のページ。
    pub committed_pages: Vec<DirtyPage>,
    /// `sequence > last_commit_seq` のページ。
    pub uncommitted_pages: Vec<DirtyPage>,
    /// `sequence ≤ last_commit_seq` のエントリ操作。
    pub committed_ops: Vec<EntryOp>,
    /// `sequence > last_commit_seq` のエントリ操作。
    pub uncommitted_ops: Vec<EntryOp>,
    /// 最後に見た COMMIT MARKER の `commit_sequence`。0 = マーカー無し。
    pub last_commit_seq: u64,
    /// 切り詰め/破損を検出したバイトオフセット。`None` = 末尾までクリーン。
    pub truncation_point: Option<u64>,
    /// CRC 検証を通った最大の `sequence_num`。
    pub valid_through_seq: u64,
}

impl RecoveryResult {
    /// ヘッダ段階の失敗を表す空の結果を作る。
    fn header_failure(status: RecoveryStatus) -> RecoveryResult {
        RecoveryResult {
            status,
            generation_id: [0u8; 16],
            committed_pages: Vec::new(),
            uncommitted_pages: Vec::new(),
            committed_ops: Vec::new(),
            uncommitted_ops: Vec::new(),
            last_commit_seq: 0,
            truncation_point: None,
            valid_through_seq: 0,
        }
    }

    /// dirty なページ/操作が 1 件も無いか（commit マーカーも無い空ファイル＝
    /// stale として silently discard できる状態。Section 3 の判定木）。
    pub fn is_empty(&self) -> bool {
        self.last_commit_seq == 0
            && self.committed_pages.is_empty()
            && self.uncommitted_pages.is_empty()
            && self.committed_ops.is_empty()
            && self.uncommitted_ops.is_empty()
    }
}

/// 中間バッファ: walk 中はまだ committed / uncommitted を分けられない（後続の
/// COMMIT MARKER が分類を変えるため）。全レコードを sequence と一緒に貯め、
/// walk 完了後に `last_commit_seq` で 1 度に振り分ける。
enum Record {
    Page(DirtyPage),
    Op(EntryOp),
}

/// vmdirty ファイル全体（`&[u8]`）に対して回復読み取り walk を実行する
/// （Section 2）。最初の失敗（CRC 不一致 / generation_id 不一致 / 切り詰め /
/// 未知マジック）で walk を止め、それ以降は一切信用しない。ヘッダ段階の失敗は
/// `status` だけを立てた空の結果を返す。
pub fn read_vmdirty(bytes: &[u8]) -> RecoveryResult {
    // ── STEP 1: ヘッダ検証 ──
    let header = match Header::decode(bytes) {
        Ok(h) => h,
        Err(status) => return RecoveryResult::header_failure(status),
    };
    let gen_id = header.generation_id;
    let page_size = header.page_size as usize;

    // ── STEP 2: レコード walk ──
    let mut records: Vec<Record> = Vec::new();
    let mut last_commit_seq: u64 = 0;
    let mut truncation_point: Option<u64> = None;
    let mut pos = HEADER_SIZE;

    loop {
        let start = pos;
        // マジック 4B。残量 0 ならクリーン EOF。
        let remaining = bytes.len() - pos;
        if remaining == 0 {
            break;
        }
        let Some(magic_buf) = take(bytes, &mut pos, 4) else {
            truncation_point = Some(start as u64);
            break;
        };
        let magic = u32::from_le_bytes(magic_buf.try_into().unwrap());

        match magic {
            DATA_RECORD_MAGIC => {
                // gen(16) + seq(8) + name_len(2)
                let Some(fixed) = take(bytes, &mut pos, 26) else {
                    truncation_point = Some(start as u64);
                    break;
                };
                if fixed[0..16] != gen_id {
                    truncation_point = Some(start as u64);
                    break;
                }
                let seq_num = rd_u64(fixed, 16);
                let name_len = rd_u16(fixed, 24) as usize;
                // name(N) + page_index(8) + data_len(4)
                let Some(rest) = take(bytes, &mut pos, name_len + 12) else {
                    truncation_point = Some(start as u64);
                    break;
                };
                let name = &rest[0..name_len];
                let page_index = rd_u64(rest, name_len);
                let data_len = rd_u32(rest, name_len + 8) as usize;
                if data_len > page_size {
                    truncation_point = Some(start as u64);
                    break;
                }
                let Some(data) = take(bytes, &mut pos, data_len) else {
                    truncation_point = Some(start as u64);
                    break;
                };
                let Some(crc_buf) = take(bytes, &mut pos, 4) else {
                    truncation_point = Some(start as u64);
                    break;
                };
                if crc32c::crc32c(&bytes[start..pos - 4]) != u32::from_le_bytes(crc_buf.try_into().unwrap()) {
                    truncation_point = Some(start as u64);
                    break;
                }
                records.push(Record::Page(DirtyPage {
                    entry_name: decode_name(name),
                    page_index,
                    data: data.to_vec(),
                    sequence: seq_num,
                }));
            }
            METADATA_RECORD_MAGIC => {
                // gen(16) + seq(8) + op(2) + name_len(2)
                let Some(fixed) = take(bytes, &mut pos, 28) else {
                    truncation_point = Some(start as u64);
                    break;
                };
                if fixed[0..16] != gen_id {
                    truncation_point = Some(start as u64);
                    break;
                }
                let seq_num = rd_u64(fixed, 16);
                let Some(op_code) = OpCode::from_code(rd_u16(fixed, 24)) else {
                    truncation_point = Some(start as u64);
                    break;
                };
                let name_len = rd_u16(fixed, 26) as usize;
                let Some(name_buf) = take(bytes, &mut pos, name_len) else {
                    truncation_point = Some(start as u64);
                    break;
                };
                let name = decode_name(name_buf);
                // ペイロード（op_code で長さが決まる。RENAME は先頭 2B を読んでから）。
                let op = match op_code {
                    OpCode::Create => MetaOp::Create,
                    OpCode::Remove => MetaOp::Remove,
                    OpCode::Resize => {
                        let Some(p) = take(bytes, &mut pos, 8) else {
                            truncation_point = Some(start as u64);
                            break;
                        };
                        MetaOp::Resize {
                            new_size: rd_u64(p, 0),
                        }
                    }
                    OpCode::Rename => {
                        let Some(len_buf) = take(bytes, &mut pos, 2) else {
                            truncation_point = Some(start as u64);
                            break;
                        };
                        let new_name_len = u16::from_le_bytes(len_buf.try_into().unwrap()) as usize;
                        let Some(nn) = take(bytes, &mut pos, new_name_len) else {
                            truncation_point = Some(start as u64);
                            break;
                        };
                        MetaOp::Rename {
                            new_name: decode_name(nn),
                        }
                    }
                };
                let Some(crc_buf) = take(bytes, &mut pos, 4) else {
                    truncation_point = Some(start as u64);
                    break;
                };
                if crc32c::crc32c(&bytes[start..pos - 4]) != u32::from_le_bytes(crc_buf.try_into().unwrap()) {
                    truncation_point = Some(start as u64);
                    break;
                }
                records.push(Record::Op(EntryOp {
                    entry_name: name,
                    op,
                    sequence: seq_num,
                }));
            }
            COMMIT_MARKER_MAGIC => {
                // gen(16) + commit_seq(8) + page_count(8) + crc(4)
                let Some(rest) = take(bytes, &mut pos, 36) else {
                    truncation_point = Some(start as u64);
                    break;
                };
                if rest[0..16] != gen_id {
                    truncation_point = Some(start as u64);
                    break;
                }
                let commit_seq = rd_u64(rest, 16);
                if crc32c::crc32c(&bytes[start..pos - 4]) != rd_u32(rest, 32) {
                    truncation_point = Some(start as u64);
                    break;
                }
                last_commit_seq = commit_seq;
            }
            _ => {
                truncation_point = Some(start as u64);
                break;
            }
        }
    }

    // ── STEP 3: 分類 ──
    let valid_through_seq = records
        .iter()
        .map(|r| match r {
            Record::Page(p) => p.sequence,
            Record::Op(o) => o.sequence,
        })
        .max()
        .unwrap_or(0);

    let mut committed_pages = Vec::new();
    let mut uncommitted_pages = Vec::new();
    let mut committed_ops = Vec::new();
    let mut uncommitted_ops = Vec::new();
    for r in records {
        match r {
            Record::Page(p) => {
                if p.sequence <= last_commit_seq {
                    committed_pages.push(p);
                } else {
                    uncommitted_pages.push(p);
                }
            }
            Record::Op(o) => {
                if o.sequence <= last_commit_seq {
                    committed_ops.push(o);
                } else {
                    uncommitted_ops.push(o);
                }
            }
        }
    }

    RecoveryResult {
        status: RecoveryStatus::Ok,
        generation_id: gen_id,
        committed_pages,
        uncommitted_pages,
        committed_ops,
        uncommitted_ops,
        last_commit_seq,
        truncation_point,
        valid_through_seq,
    }
}

/// エントリ名のデコード。不正 UTF-8 は Section 2 どおり置換（U+FFFD）する。
fn decode_name(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// `pos` から `n` バイトを切り出して `pos` を進める。残量不足は `None`。
#[inline]
fn take<'a>(b: &'a [u8], pos: &mut usize, n: usize) -> Option<&'a [u8]> {
    let end = pos.checked_add(n)?;
    if end > b.len() {
        return None;
    }
    let s = &b[*pos..end];
    *pos = end;
    Some(s)
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

    const GEN: [u8; 16] = [
        0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x47, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF,
        0x00,
    ];

    fn header() -> Header {
        Header {
            flags: 0,
            generation_id: GEN,
            source_file_size: 4096,
            source_inode: 7,
            source_cd_hash: [0xAB; CD_HASH_SIZE],
            created_at_ns: 1_700_000_000_000_000_000,
            page_size: 4096,
        }
    }

    /// ヘッダ + 任意のレコード列から完全な vmdirty バイト列を組む。
    fn build(records: &[Vec<u8>]) -> Vec<u8> {
        let mut f = header().encode().to_vec();
        for r in records {
            f.extend_from_slice(r);
        }
        f
    }

    #[test]
    fn header_roundtrip() {
        let h = header();
        let bytes = h.encode();
        assert_eq!(bytes.len(), HEADER_SIZE);
        assert_eq!(&bytes[0..8], &MAGIC);
        assert_eq!(Header::decode(&bytes).unwrap(), h);
    }

    #[test]
    fn header_rejects_truncation_magic_crc_version() {
        let h = header();
        let bytes = h.encode();
        // 切り詰め
        assert_eq!(Header::decode(&bytes[..80]), Err(RecoveryStatus::HeaderTruncated));
        // マジック破壊
        let mut bad = bytes;
        bad[1] ^= 0xff;
        assert_eq!(Header::decode(&bad), Err(RecoveryStatus::HeaderMagicBad));
        // CRC 破壊（マジック以外のフィールドを 1 ビット反転）
        let mut bad = h.encode();
        bad[30] ^= 0x01;
        assert_eq!(Header::decode(&bad), Err(RecoveryStatus::HeaderCrcBad));
        // 未来のバージョン
        let mut bad = h.encode();
        bad[8..10].copy_from_slice(&(FORMAT_VERSION + 1).to_le_bytes());
        let crc = crc32c::crc32c(&bad[0..80]);
        bad[80..84].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(Header::decode(&bad), Err(RecoveryStatus::VersionUnsupported));
    }

    #[test]
    fn read_vmdirty_header_failure_propagates() {
        let mut bad = header().encode();
        bad[0] ^= 0xff;
        let r = read_vmdirty(&bad);
        assert_eq!(r.status, RecoveryStatus::HeaderMagicBad);
        assert!(r.is_empty());
    }

    #[test]
    fn empty_file_is_clean_and_stale() {
        let f = build(&[]);
        let r = read_vmdirty(&f);
        assert_eq!(r.status, RecoveryStatus::Ok);
        assert_eq!(r.truncation_point, None);
        assert_eq!(r.last_commit_seq, 0);
        assert_eq!(r.valid_through_seq, 0);
        assert!(r.is_empty());
        assert_eq!(r.generation_id, GEN);
    }

    #[test]
    fn data_record_roundtrips_through_walk() {
        let page = vec![0x5A; 4096];
        let f = build(&[encode_data_record(&GEN, 1, "dir/file.bin", 3, &page)]);
        let r = read_vmdirty(&f);
        assert_eq!(r.status, RecoveryStatus::Ok);
        assert_eq!(r.truncation_point, None);
        // commit マーカー無し → uncommitted。
        assert!(r.committed_pages.is_empty());
        assert_eq!(r.uncommitted_pages.len(), 1);
        let p = &r.uncommitted_pages[0];
        assert_eq!(p.entry_name, "dir/file.bin");
        assert_eq!(p.page_index, 3);
        assert_eq!(p.sequence, 1);
        assert_eq!(p.data, page);
        assert_eq!(r.valid_through_seq, 1);
    }

    #[test]
    fn short_last_page_smaller_than_page_size() {
        let tail = vec![0x01; 10];
        let f = build(&[encode_data_record(&GEN, 1, "a", 0, &tail)]);
        let r = read_vmdirty(&f);
        assert_eq!(r.uncommitted_pages[0].data.len(), 10);
    }

    #[test]
    fn commit_marker_splits_committed_and_uncommitted() {
        // seq 1..=3 を commit、seq 4..=5 は未 commit、seq 6 は途中で千切れる。
        let page = vec![0u8; 4096];
        let mut recs = Vec::new();
        for seq in 1..=3u64 {
            recs.push(encode_data_record(&GEN, seq, "e", seq, &page));
        }
        recs.push(encode_commit_marker(&GEN, 3, 3).to_vec());
        recs.push(encode_data_record(&GEN, 4, "e", 4, &page));
        recs.push(encode_data_record(&GEN, 5, "e", 5, &page));
        let mut partial = encode_data_record(&GEN, 6, "e", 6, &page);
        partial.truncate(partial.len() - 100); // 途中で千切れた write
        recs.push(partial);
        let f = build(&recs);

        let r = read_vmdirty(&f);
        assert_eq!(r.status, RecoveryStatus::Ok);
        assert_eq!(r.last_commit_seq, 3);
        let committed: Vec<u64> = r.committed_pages.iter().map(|p| p.sequence).collect();
        let uncommitted: Vec<u64> = r.uncommitted_pages.iter().map(|p| p.sequence).collect();
        assert_eq!(committed, vec![1, 2, 3]);
        assert_eq!(uncommitted, vec![4, 5]);
        // seq 6 は CRC/切り詰めで捨てられ、truncation_point が立つ。
        assert!(r.truncation_point.is_some());
        assert_eq!(r.valid_through_seq, 5);
    }

    #[test]
    fn walk_stops_at_corrupt_crc() {
        let page = vec![0u8; 16];
        let mut r1 = encode_data_record(&GEN, 1, "a", 0, &page);
        let r2 = encode_data_record(&GEN, 2, "a", 1, &page);
        // r1 の data 部を 1 ビット反転 → CRC 不一致。先頭レコードで止まる。
        let last = r1.len() - 5;
        r1[last] ^= 0x01;
        let f = build(&[r1, r2]);
        let r = read_vmdirty(&f);
        assert!(r.committed_pages.is_empty());
        assert!(r.uncommitted_pages.is_empty());
        assert_eq!(r.truncation_point, Some(HEADER_SIZE as u64));
    }

    #[test]
    fn walk_stops_at_generation_mismatch() {
        let page = vec![0u8; 16];
        let mut other = GEN;
        other[0] ^= 0xff;
        let f = build(&[
            encode_data_record(&GEN, 1, "a", 0, &page),
            encode_data_record(&other, 2, "a", 1, &page),
        ]);
        let r = read_vmdirty(&f);
        // 1 件目は有効、2 件目で gen 不一致 → 停止。
        assert_eq!(r.uncommitted_pages.len(), 1);
        assert_eq!(r.uncommitted_pages[0].sequence, 1);
        assert!(r.truncation_point.is_some());
    }

    #[test]
    fn walk_stops_at_unknown_magic() {
        let page = vec![0u8; 16];
        let mut junk = encode_data_record(&GEN, 1, "a", 0, &page);
        junk[0..4].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
        let f = build(&[junk]);
        let r = read_vmdirty(&f);
        assert!(r.uncommitted_pages.is_empty());
        assert_eq!(r.truncation_point, Some(HEADER_SIZE as u64));
    }

    #[test]
    fn data_len_exceeding_page_size_is_rejected() {
        // page_size=4096 のヘッダに対し data_len=5000 を手で仕込む。
        let oversized = vec![0u8; 5000];
        // encode はそのまま 5000B 書くが、walk が page_size 超で弾く。
        let f = build(&[encode_data_record(&GEN, 1, "a", 0, &oversized)]);
        let r = read_vmdirty(&f);
        assert!(r.uncommitted_pages.is_empty());
        assert_eq!(r.truncation_point, Some(HEADER_SIZE as u64));
    }

    #[test]
    fn metadata_records_all_ops_roundtrip() {
        let ops = [
            MetaOp::Create,
            MetaOp::Remove,
            MetaOp::Resize { new_size: 123_456 },
            MetaOp::Rename {
                new_name: "renamed/path.txt".to_string(),
            },
        ];
        let mut recs = Vec::new();
        for (i, op) in ops.iter().enumerate() {
            recs.push(encode_metadata_record(&GEN, (i + 1) as u64, "orig", op));
        }
        recs.push(encode_commit_marker(&GEN, 4, 0).to_vec());
        let f = build(&recs);

        let r = read_vmdirty(&f);
        assert_eq!(r.status, RecoveryStatus::Ok);
        assert_eq!(r.truncation_point, None);
        assert_eq!(r.committed_ops.len(), 4);
        assert!(r.uncommitted_ops.is_empty());
        assert_eq!(r.committed_ops[0].op, MetaOp::Create);
        assert_eq!(r.committed_ops[1].op, MetaOp::Remove);
        assert_eq!(r.committed_ops[2].op, MetaOp::Resize { new_size: 123_456 });
        assert_eq!(
            r.committed_ops[3].op,
            MetaOp::Rename {
                new_name: "renamed/path.txt".to_string()
            }
        );
        // ops のみのセッションも回復可能な dirty セッション。
        assert!(!r.is_empty());
    }

    #[test]
    fn pages_and_ops_interleave_in_sequence() {
        let page = vec![0u8; 8];
        let f = build(&[
            encode_metadata_record(&GEN, 1, "new", &MetaOp::Create),
            encode_data_record(&GEN, 2, "new", 0, &page),
            encode_commit_marker(&GEN, 2, 1).to_vec(),
            encode_metadata_record(&GEN, 3, "new", &MetaOp::Resize { new_size: 8 }),
        ]);
        let r = read_vmdirty(&f);
        assert_eq!(r.committed_ops.len(), 1);
        assert_eq!(r.committed_ops[0].sequence, 1);
        assert_eq!(r.committed_pages.len(), 1);
        assert_eq!(r.committed_pages[0].sequence, 2);
        assert_eq!(r.uncommitted_ops.len(), 1);
        assert_eq!(r.uncommitted_ops[0].sequence, 3);
        assert_eq!(r.valid_through_seq, 3);
    }

    #[test]
    fn commit_marker_with_bad_crc_stops_walk() {
        let page = vec![0u8; 8];
        let mut marker = encode_commit_marker(&GEN, 1, 0).to_vec();
        marker[20] ^= 0x01; // commit_sequence を改竄
        let f = build(&[
            encode_data_record(&GEN, 1, "a", 0, &page),
            marker,
            encode_data_record(&GEN, 2, "a", 1, &page),
        ]);
        let r = read_vmdirty(&f);
        // マーカーで止まる → seq1 は uncommitted、seq2 は未読。
        assert_eq!(r.last_commit_seq, 0);
        assert_eq!(r.uncommitted_pages.len(), 1);
        assert_eq!(r.uncommitted_pages[0].sequence, 1);
        assert!(r.truncation_point.is_some());
    }
}
