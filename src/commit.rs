//! commit FULL（compaction）パス（設計 commit() FLOW の FULL path）。
//!
//! Diff Layer の dirty ページを反映した**新しい完全な ZIP** をメモリ上に組み立て
//! る。設計の最も単純なクラッシュセーフ書き込み（M2）:
//!
//! - 未変更エントリ → 圧縮ストリームを verbatim でコピー（再圧縮コスト 0）。
//! - 変更エントリ   → Diff ページ + ソースの未変更ページ + ゼロ埋め gap から
//!   論理内容を組み立て直し、元のメソッド（STORE / 標準 DEFLATE）で再圧縮する。
//!
//! 生成物は呼び出し側が `archive.new.zip` に書いて `rename()` する（[`disk`]
//! 層）か、メモリ上のテストで再 open する。バイト列の生成だけを担い、ファイル
//! I/O・rename・vmidx の更新は持たない（設計どおり責務を分離）。
//!
//! 出力 ZIP は正規形の Local File Header / Central Directory ファイルヘッダ
//! （extra field なし、コメントなし）で書き直す。エントリ CRC-32 は ZIP 標準の
//! ISO-HDLC 多項式（zlib `crc32()` 相当。ジャーナルの CRC-32C とは別物
//! ＝IMPLEMENTATION_NOTES の罠）。
//!
//! M2 の制限: Zip64 出力は未対応。いずれかのオフセット・サイズ・件数が 32 ビット
//! に収まらない場合は [`CommitError::TooLarge`] を返す（INCREMENTAL / Dead Space
//! Freelist / journal は M3 以降）。
//!
//! [`disk`]: crate::disk

use crate::archive::{Archive, CdEntry, ZipError};
use crate::difflayer::DiffLayer;
use crate::entrytable::{EntryTable, Kind};
use crate::mount::{read_entry, ReadError};
use crate::page::{page_count, page_extent};
use crate::vmidx::ProviderType;
use libz_rs_sys as z;
use std::collections::HashSet;
use std::fmt;
use std::os::raw::c_int;
use std::sync::OnceLock;

const LFH_SIG: u32 = 0x0403_4b50;
const CDFH_SIG: u32 = 0x0201_4b50;
const EOCD_SIG: u32 = 0x0605_4b50;

/// commit の失敗。
#[derive(Debug)]
pub enum CommitError {
    /// ソース ZIP の parse / エントリ取り出しに失敗した。
    Zip(ZipError),
    /// 変更エントリの未変更ページをソースから読む段で失敗した。
    Read(ReadError),
    /// 再圧縮（DEFLATE）に失敗した。
    Compress(&'static str),
    /// 再圧縮できない圧縮種別（STORE / 標準 DEFLATE 以外）。
    Unsupported(ProviderType),
    /// 出力が 32 ビット ZIP の表現範囲を超える（M2 は Zip64 出力に未対応）。
    TooLarge,
}

impl fmt::Display for CommitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CommitError::Zip(e) => write!(f, "commit: {e}"),
            CommitError::Read(e) => write!(f, "commit: {e}"),
            CommitError::Compress(why) => write!(f, "commit: recompress failed ({why})"),
            CommitError::Unsupported(p) => write!(f, "commit: cannot recompress {p:?}"),
            CommitError::TooLarge => write!(f, "commit: output exceeds 32-bit ZIP limits (zip64 not supported in M2)"),
        }
    }
}

impl std::error::Error for CommitError {}

/// 出力する 1 エントリの中身。`stored` は**圧縮済み**バイト列（未変更エントリの
/// verbatim コピー、または再圧縮の結果）で、`crc32`/`uncomp_size` は**展開後**の
/// 論理内容に対する値。圧縮サイズは `stored.len()` が唯一の真実なので持たない。
struct EntryPayload {
    /// 生の ZIP 圧縮メソッドコード（0=STORE / 8=DEFLATE）。
    method: u16,
    /// 展開後内容の CRC-32（ISO-HDLC。ジャーナルの CRC-32C とは別物）。
    crc32: u32,
    /// 圧縮済みバイト列。
    stored: Vec<u8>,
    /// 展開後の論理サイズ。
    uncomp_size: u64,
}

/// 出力 ZIP に配置し終えた 1 エントリの、Central Directory 用レコード。
///
/// `local_header_offset` は**出力ファイル先頭からの絶対オフセット**。FULL では
/// 組み立て中バッファ内の位置、INCREMENTAL では追記ベース込みの位置、未変更
/// エントリ（[`record_in_place`]）では元アーカイブの値をそのまま引き継ぐ。
struct PlacedEntry {
    local_header_offset: u64,
    method: u16,
    crc32: u32,
    comp_size: u64,
    uncomp_size: u64,
    name: Vec<u8>,
}

impl PlacedEntry {
    /// `lho` に配置した `payload` を、CD 用レコードにする。圧縮サイズは
    /// `payload.stored` の実長から採る。
    fn new(lho: u64, payload: &EntryPayload, name: &[u8]) -> PlacedEntry {
        PlacedEntry {
            local_header_offset: lho,
            method: payload.method,
            crc32: payload.crc32,
            comp_size: payload.stored.len() as u64,
            uncomp_size: payload.uncomp_size,
            name: name.to_vec(),
        }
    }
}

/// Diff Layer + エントリ表を反映した新しい ZIP バイト列を組み立てる（FULL パス）。
///
/// `archive` はソース ZIP、`vmidx_image` はそれに対応する vmidx 像（変更エントリ
/// の未変更ページをソースから読むのに使う）、`diff` は Tier 1 の dirty 状態、
/// `table` はセッション内の構造変更（create / remove）。実効的なエントリ集合は
/// 「vmidx − tombstone ∪ created」で組み立てる:
///
/// - tombstone（remove / rename 元）→ 出力しない。
/// - created（vmidx に同名があれば上書き、無ければ新規）→ Diff から組み立て、
///   既定 DEFLATE で再圧縮して新しい LFH/CD を出す。
/// - 別名（rename ターゲット）→ 現在名で出力し、未変更データはソース名の archive
///   エントリから引く。未 dirty なら verbatim コピー（未対応圧縮種別でも通る）、
///   dirty なら組み立て直してソースの元メソッドで再圧縮する。
/// - それ以外のソースエントリ → dirty なら再圧縮、未変更なら verbatim コピー。
pub fn build_full(
    archive: &[u8],
    vmidx_image: &[u8],
    diff: &DiffLayer,
    table: &EntryTable,
) -> Result<Vec<u8>, CommitError> {
    let ar = Archive::parse(archive).map_err(CommitError::Zip)?;

    let mut body: Vec<u8> = Vec::new();
    // 配置済みエントリを CD 用に控える。
    let mut placed: Vec<PlacedEntry> = Vec::with_capacity(ar.entries().len());
    // vmidx ループで created として出した名前（created ループの重複出力を防ぐ）。
    let mut emitted_created: HashSet<String> = HashSet::new();

    for entry in ar.entries() {
        let name_utf8 = std::str::from_utf8(&entry.name).ok();

        // UTF-8 名のみオーバーレイ対象（非 UTF-8 名は table に入りえない）。
        if let Some(name) = name_utf8 {
            // この vmidx 名が rename ターゲットとして再利用されている場合（別名）は、
            // ここでは出さず後段の別名ループでソースから組み立てる。
            if table.is_aliased(name) {
                continue;
            }
            match table.kind(name, true) {
                Kind::Absent => continue, // tombstone → 出力しない
                Kind::Created => {
                    // 同名を再 create（リスタート）→ created として組み立てる。
                    let payload = build_created(archive, vmidx_image, diff, name)?;
                    place_entry(&mut body, &mut placed, &payload, &entry.name)?;
                    emitted_created.insert(name.to_owned());
                    continue;
                }
                Kind::Source => {}
            }
        }

        // 通常のソースエントリ（Kind::Source または非 UTF-8 名）。
        let dirty_name = name_utf8.filter(|n| diff.is_dirty(n));
        let payload = if let Some(name) = dirty_name {
            let logical = diff.logical_size(name).unwrap_or(entry.uncompressed_size);
            // ソース読み出しの上限は source high-water（truncate-shrink で縮む）。
            let original = diff.source_size(name).unwrap_or(entry.uncompressed_size);
            let content =
                assemble_content(archive, vmidx_image, diff, name, logical, original, Some(name))?;
            let crc = crc32(&content);
            let (method, stored) = match entry.provider_type {
                ProviderType::Store => (0u16, content.clone()),
                ProviderType::Deflate => (8u16, deflate(&content)?),
                other => return Err(CommitError::Unsupported(other)),
            };
            EntryPayload { method, crc32: crc, stored, uncomp_size: content.len() as u64 }
        } else {
            // 未変更: 圧縮ストリームを verbatim でコピー（CRC・サイズは CD から）。
            let stored = ar.entry_data(entry).map_err(CommitError::Zip)?.to_vec();
            EntryPayload {
                method: entry.method_code,
                crc32: entry.crc32,
                stored,
                uncomp_size: entry.uncompressed_size,
            }
        };

        place_entry(&mut body, &mut placed, &payload, &entry.name)?;
    }

    // vmidx に無い created エントリ。
    for name in table.created_names() {
        if emitted_created.contains(name) {
            continue;
        }
        let payload = build_created(archive, vmidx_image, diff, name)?;
        place_entry(&mut body, &mut placed, &payload, name.as_bytes())?;
    }

    // 別名エントリ（rename ターゲット）。現在名で出力し、未変更データはソース名の
    // archive エントリから引く。未 dirty なら圧縮ストリームを verbatim コピー
    // （再圧縮なし＝未対応圧縮種別でも通る）、dirty なら論理内容を組み立てて
    // ソースの元メソッドで再圧縮する。
    for (current, source) in table.aliases() {
        let src_entry = ar
            .entries()
            .iter()
            .find(|e| e.name == source.as_bytes())
            .ok_or(CommitError::Read(ReadError::NotFound))?;
        let payload = if diff.is_dirty(current) {
            let logical = diff
                .logical_size(current)
                .unwrap_or(src_entry.uncompressed_size);
            let original = diff
                .source_size(current)
                .unwrap_or(src_entry.uncompressed_size);
            let content =
                assemble_content(archive, vmidx_image, diff, current, logical, original, Some(source))?;
            let crc = crc32(&content);
            let (method, stored) = match src_entry.provider_type {
                ProviderType::Store => (0u16, content.clone()),
                ProviderType::Deflate => (8u16, deflate(&content)?),
                other => return Err(CommitError::Unsupported(other)),
            };
            EntryPayload { method, crc32: crc, stored, uncomp_size: content.len() as u64 }
        } else {
            // 未変更: ソースの圧縮ストリームを verbatim コピー（CRC・サイズは CD）。
            let stored = ar.entry_data(src_entry).map_err(CommitError::Zip)?.to_vec();
            EntryPayload {
                method: src_entry.method_code,
                crc32: src_entry.crc32,
                stored,
                uncomp_size: src_entry.uncompressed_size,
            }
        };
        place_entry(&mut body, &mut placed, &payload, current.as_bytes())?;
    }

    // Central Directory。
    let cd_offset = body.len() as u64;
    let mut cd: Vec<u8> = Vec::new();
    for entry in &placed {
        write_cdfh(&mut cd, entry);
    }
    let cd_size = cd.len() as u64;
    body.extend_from_slice(&cd);

    let count = placed.len();
    if cd_offset > u32::MAX as u64 || cd_size > u32::MAX as u64 || count > u16::MAX as usize {
        return Err(CommitError::TooLarge);
    }

    // End Of Central Directory。
    push_u32(&mut body, EOCD_SIG);
    push_u16(&mut body, 0); // このディスク番号
    push_u16(&mut body, 0); // CD 開始ディスク
    push_u16(&mut body, count as u16); // このディスクのエントリ数
    push_u16(&mut body, count as u16); // 総エントリ数
    push_u32(&mut body, cd_size as u32);
    push_u32(&mut body, cd_offset as u32);
    push_u16(&mut body, 0); // コメント長

    Ok(body)
}

/// INCREMENTAL commit（ADR 0012）。既存アーカイブのバイトは保ったまま、変更/新規/
/// 別名エントリだけを末尾に追記する「追記分バイト列」を返す。呼び出し側はこれを
/// アーカイブ末尾（オフセット `archive.len()`）に書く。
///
/// 返すレイアウト: `[変更/新規/別名の LFH+データ][全 live を指す新 CD][新 EOCD]`。
/// 未変更エントリは元の local header offset のまま新 CD に載る（再圧縮もコピーも
/// せず追記コスト 0）。旧 CD/EOCD・変更前バイトは中間に dead として残り、FULL
/// compaction で回収する（設計 ADR 0011/0012）。
pub fn build_incremental(
    archive: &[u8],
    vmidx_image: &[u8],
    diff: &DiffLayer,
    table: &EntryTable,
) -> Result<Vec<u8>, CommitError> {
    let ar = Archive::parse(archive).map_err(CommitError::Zip)?;
    let base = archive.len() as u64; // 追記開始の絶対オフセット。

    let mut body: Vec<u8> = Vec::new(); // 末尾に追記する LFH+データ（後で CD/EOCD も足す）。
    let mut placed: Vec<PlacedEntry> = Vec::with_capacity(ar.entries().len());
    let mut emitted_created: HashSet<String> = HashSet::new();

    for entry in ar.entries() {
        let name_utf8 = std::str::from_utf8(&entry.name).ok();
        if let Some(name) = name_utf8 {
            // rename ターゲットで再利用された vmidx 名は別名ループで現在名として追記。
            if table.is_aliased(name) {
                continue;
            }
            match table.kind(name, true) {
                Kind::Absent => continue, // tombstone / rename 元 → 新 CD から外す。
                Kind::Created => {
                    let payload = build_created(archive, vmidx_image, diff, name)?;
                    place_appended(&mut body, &mut placed, base, &payload, &entry.name)?;
                    emitted_created.insert(name.to_owned());
                    continue;
                }
                Kind::Source => {}
            }
        }

        // 通常のソースエントリ（非 UTF-8 名はオーバーレイ対象外＝常に未変更）。
        if let Some(name) = name_utf8.filter(|n| diff.is_dirty(n)) {
            // 変更あり → 再材料化して末尾に追記。
            let logical = diff.logical_size(name).unwrap_or(entry.uncompressed_size);
            let original = diff.source_size(name).unwrap_or(entry.uncompressed_size);
            let content =
                assemble_content(archive, vmidx_image, diff, name, logical, original, Some(name))?;
            let crc = crc32(&content);
            let (method, stored) = match entry.provider_type {
                ProviderType::Store => (0u16, content.clone()),
                ProviderType::Deflate => (8u16, deflate(&content)?),
                other => return Err(CommitError::Unsupported(other)),
            };
            let payload =
                EntryPayload { method, crc32: crc, stored, uncomp_size: content.len() as u64 };
            place_appended(&mut body, &mut placed, base, &payload, &entry.name)?;
        } else {
            // 未変更 → 追記せず、元の local header offset で新 CD に載せる。
            record_in_place(&mut placed, entry)?;
        }
    }

    // vmidx に無い created エントリ。
    for name in table.created_names() {
        if emitted_created.contains(name) {
            continue;
        }
        let payload = build_created(archive, vmidx_image, diff, name)?;
        place_appended(&mut body, &mut placed, base, &payload, name.as_bytes())?;
    }

    // 別名（rename ターゲット）。LFH の名前を現在名にするため、未変更でも LFH+データを
    // 追記する（データは verbatim コピー、再圧縮なし＝未対応圧縮種別でも通る）。
    for (current, source) in table.aliases() {
        let src_entry = ar
            .entries()
            .iter()
            .find(|e| e.name == source.as_bytes())
            .ok_or(CommitError::Read(ReadError::NotFound))?;
        let payload = if diff.is_dirty(current) {
            let logical = diff
                .logical_size(current)
                .unwrap_or(src_entry.uncompressed_size);
            let original = diff
                .source_size(current)
                .unwrap_or(src_entry.uncompressed_size);
            let content =
                assemble_content(archive, vmidx_image, diff, current, logical, original, Some(source))?;
            let crc = crc32(&content);
            let (method, stored) = match src_entry.provider_type {
                ProviderType::Store => (0u16, content.clone()),
                ProviderType::Deflate => (8u16, deflate(&content)?),
                other => return Err(CommitError::Unsupported(other)),
            };
            EntryPayload { method, crc32: crc, stored, uncomp_size: content.len() as u64 }
        } else {
            let stored = ar.entry_data(src_entry).map_err(CommitError::Zip)?.to_vec();
            EntryPayload {
                method: src_entry.method_code,
                crc32: src_entry.crc32,
                stored,
                uncomp_size: src_entry.uncompressed_size,
            }
        };
        place_appended(&mut body, &mut placed, base, &payload, current.as_bytes())?;
    }

    // 新 Central Directory（全 live エントリ）。CD は追記分の直後 = base + body.len()。
    let cd_offset = base + body.len() as u64;
    let mut cd: Vec<u8> = Vec::new();
    for entry in &placed {
        write_cdfh(&mut cd, entry);
    }
    let cd_size = cd.len() as u64;
    body.extend_from_slice(&cd);

    let count = placed.len();
    if cd_offset > u32::MAX as u64 || cd_size > u32::MAX as u64 || count > u16::MAX as usize {
        return Err(CommitError::TooLarge);
    }

    // 新 End Of Central Directory（末尾。backscan はこれを最後の EOCD として拾う）。
    push_u32(&mut body, EOCD_SIG);
    push_u16(&mut body, 0);
    push_u16(&mut body, 0);
    push_u16(&mut body, count as u16);
    push_u16(&mut body, count as u16);
    push_u32(&mut body, cd_size as u32);
    push_u32(&mut body, cd_offset as u32);
    push_u16(&mut body, 0);

    Ok(body)
}

/// 追記領域へ 1 エントリを書き（LFH+データ）、その**絶対** local header offset
/// （`base + 追記内位置`）を CD 用に記録する。32 ビット超過は [`CommitError::TooLarge`]。
fn place_appended(
    body: &mut Vec<u8>,
    placed: &mut Vec<PlacedEntry>,
    base: u64,
    payload: &EntryPayload,
    name: &[u8],
) -> Result<(), CommitError> {
    write_placed(body, placed, base + body.len() as u64, payload, name)
}

/// 未変更エントリを元の local header offset のまま新 CD に載せる（追記しない＝
/// 既存バイトをそのまま再利用）。
fn record_in_place(placed: &mut Vec<PlacedEntry>, entry: &CdEntry) -> Result<(), CommitError> {
    if entry.local_header_offset > u32::MAX as u64
        || entry.compressed_size > u32::MAX as u64
        || entry.uncompressed_size > u32::MAX as u64
    {
        return Err(CommitError::TooLarge);
    }
    placed.push(PlacedEntry {
        local_header_offset: entry.local_header_offset,
        method: entry.method_code,
        crc32: entry.crc32,
        comp_size: entry.compressed_size,
        uncomp_size: entry.uncompressed_size,
        name: entry.name.clone(),
    });
    Ok(())
}

/// エントリの論理内容（長さ `logical`）を組み立てる。各ページは Diff Layer 優先、
/// 無ければソース（`source` = vmidx 名、`None` = created）の未変更ページ、ソース
/// 範囲を超える分とソース無しはゼロ（implicit extension の gap / created の未書き
/// 込み）。`name` は Diff Layer のキー（現在名）。
#[allow(clippy::too_many_arguments)]
fn assemble_content(
    archive: &[u8],
    vmidx_image: &[u8],
    diff: &DiffLayer,
    name: &str,
    logical: u64,
    original_size: u64,
    source: Option<&str>,
) -> Result<Vec<u8>, CommitError> {
    let ps = diff.page_size();
    let mut content = Vec::with_capacity(logical as usize);
    for page in 0..page_count(logical, ps) {
        let (start, len) = page_extent(logical, page, ps);
        if len == 0 {
            continue;
        }
        if let Some(p) = diff.page(name, page) {
            content.extend_from_slice(&p[..len]);
        } else if let Some(src) = source.filter(|_| start < original_size) {
            // 未変更ページ: ソースから読む。論理ページがソース末尾を跨ぐ場合は
            // 残りをゼロで埋める（短い末尾ページ + gap）。
            let avail = ((original_size - start) as usize).min(len);
            let chunk =
                read_entry(archive, vmidx_image, src, start, avail).map_err(CommitError::Read)?;
            content.extend_from_slice(&chunk);
            if avail < len {
                content.resize(content.len() + (len - avail), 0);
            }
        } else {
            content.resize(content.len() + len, 0);
        }
    }
    Ok(content)
}

/// created エントリ `name` を Diff から組み立て、既定 DEFLATE で圧縮する。
/// ソースは無い（未書き込みページはゼロ）。
fn build_created(
    archive: &[u8],
    vmidx_image: &[u8],
    diff: &DiffLayer,
    name: &str,
) -> Result<EntryPayload, CommitError> {
    let logical = diff.logical_size(name).unwrap_or(0);
    let content = assemble_content(archive, vmidx_image, diff, name, logical, 0, None)?;
    let crc = crc32(&content);
    let stored = deflate(&content)?;
    Ok(EntryPayload { method: 8, crc32: crc, stored, uncomp_size: content.len() as u64 })
}

/// 1 エントリを body へ書き（LFH + データ）、CD 用情報を `placed` に積む。
/// 32 ビット ZIP の表現範囲を超えたら [`CommitError::TooLarge`]。
fn place_entry(
    body: &mut Vec<u8>,
    placed: &mut Vec<PlacedEntry>,
    payload: &EntryPayload,
    name: &[u8],
) -> Result<(), CommitError> {
    write_placed(body, placed, body.len() as u64, payload, name)
}

/// [`place_entry`] / [`place_appended`] の共通部。`lho`（出力先頭からの絶対
/// オフセット）に LFH + データを書き、CD 用レコードを積む。両者の違いは
/// INCREMENTAL が追記ベースを足すかどうかだけ。
fn write_placed(
    body: &mut Vec<u8>,
    placed: &mut Vec<PlacedEntry>,
    lho: u64,
    payload: &EntryPayload,
    name: &[u8],
) -> Result<(), CommitError> {
    let entry = PlacedEntry::new(lho, payload, name);
    if entry.local_header_offset > u32::MAX as u64
        || entry.comp_size > u32::MAX as u64
        || entry.uncomp_size > u32::MAX as u64
    {
        return Err(CommitError::TooLarge);
    }
    write_lfh(body, &entry);
    body.extend_from_slice(&payload.stored);
    placed.push(entry);
    Ok(())
}

/// 正規形のローカルファイルヘッダ（extra field なし）を書く。
fn write_lfh(out: &mut Vec<u8>, e: &PlacedEntry) {
    push_u32(out, LFH_SIG);
    push_u16(out, 20); // version needed
    push_u16(out, 0); // flags
    push_u16(out, e.method);
    push_u16(out, 0); // mod time
    push_u16(out, 0); // mod date
    push_u32(out, e.crc32);
    push_u32(out, e.comp_size as u32);
    push_u32(out, e.uncomp_size as u32);
    push_u16(out, e.name.len() as u16);
    push_u16(out, 0); // extra len
    out.extend_from_slice(&e.name);
}

/// 正規形の Central Directory ファイルヘッダ（extra / comment なし）を書く。
fn write_cdfh(out: &mut Vec<u8>, e: &PlacedEntry) {
    push_u32(out, CDFH_SIG);
    push_u16(out, 20); // version made by
    push_u16(out, 20); // version needed
    push_u16(out, 0); // flags
    push_u16(out, e.method);
    push_u16(out, 0); // mod time
    push_u16(out, 0); // mod date
    push_u32(out, e.crc32);
    push_u32(out, e.comp_size as u32);
    push_u32(out, e.uncomp_size as u32);
    push_u16(out, e.name.len() as u16);
    push_u16(out, 0); // extra len
    push_u16(out, 0); // comment len
    push_u16(out, 0); // disk start
    push_u16(out, 0); // internal attrs
    push_u32(out, 0); // external attrs
    push_u32(out, e.local_header_offset as u32);
    out.extend_from_slice(&e.name);
}

#[inline]
fn push_u16(v: &mut Vec<u8>, x: u16) {
    v.extend_from_slice(&x.to_le_bytes());
}

#[inline]
fn push_u32(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_le_bytes());
}

/// 生バイト列を raw DEFLATE（windowBits = -15、ZIP メソッド 8）で圧縮する。
/// `Z_FINISH` で一括圧縮し、`deflateBound` 相当の余裕を取った 1 バッファに収める。
fn deflate(data: &[u8]) -> Result<Vec<u8>, CommitError> {
    let mut strm = z::z_stream::default();
    // deflateBound 相当の上限（壊滅的非圧縮でも収まる余裕）。
    let cap = data.len() + data.len() / 2 + 1024;
    let mut out = vec![0u8; cap];
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
        if r != z::Z_OK {
            return Err(CommitError::Compress("deflateInit2 failed"));
        }
        strm.next_in = data.as_ptr();
        strm.avail_in = data.len() as _;
        strm.next_out = out.as_mut_ptr();
        strm.avail_out = out.len() as _;
        let r = z::deflate(&mut strm, z::Z_FINISH);
        if r != z::Z_STREAM_END {
            z::deflateEnd(&mut strm);
            return Err(CommitError::Compress("deflate did not finish in one shot"));
        }
        let produced = out.len() - strm.avail_out as usize;
        out.truncate(produced);
        z::deflateEnd(&mut strm);
    }
    Ok(out)
}

/// ZIP 標準のエントリ CRC-32（ISO-HDLC、反転多項式 0xEDB88320）。zlib の
/// `crc32()` と一致する。**ジャーナル / vmidx の CRC-32C（Castagnoli）とは別物**
/// なので混同しないこと（IMPLEMENTATION_NOTES）。
fn crc32(data: &[u8]) -> u32 {
    let table = crc_table();
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc = table[((crc ^ b as u32) & 0xff) as usize] ^ (crc >> 8);
    }
    !crc
}

fn crc_table() -> &'static [u32; 256] {
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        let mut n = 0usize;
        while n < 256 {
            let mut c = n as u32;
            let mut k = 0;
            while k < 8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
                k += 1;
            }
            t[n] = c;
            n += 1;
        }
        t
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_known_vector() {
        // "123456789" の CRC-32 (ISO-HDLC) は 0xCBF43926（標準チェック値）。
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn deflate_round_trips_via_inflate() {
        let data: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        let comp = deflate(&data).unwrap();
        // libz-rs-sys で raw inflate して戻す。
        let mut strm = z::z_stream::default();
        let mut out = vec![0u8; data.len()];
        unsafe {
            let r = z::inflateInit2_(
                &mut strm,
                -15,
                z::zlibVersion(),
                core::mem::size_of::<z::z_stream>() as c_int,
            );
            assert_eq!(r, z::Z_OK);
            strm.next_in = comp.as_ptr();
            strm.avail_in = comp.len() as _;
            strm.next_out = out.as_mut_ptr();
            strm.avail_out = out.len() as _;
            let r = z::inflate(&mut strm, z::Z_FINISH);
            assert_eq!(r, z::Z_STREAM_END);
            z::inflateEnd(&mut strm);
        }
        assert_eq!(out, data);
    }

    #[test]
    fn deflate_handles_empty_input() {
        let comp = deflate(b"").unwrap();
        assert!(!comp.is_empty(), "empty deflate stream still has a terminator");
    }
}
