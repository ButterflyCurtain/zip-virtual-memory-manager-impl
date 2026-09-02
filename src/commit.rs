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
    record: PlacedRecord,
}

/// CD レコードの出し方。
///
/// **commit はエントリを「再配置」するのであって「書き直す」のではない。**
/// 内容が変わっていないエントリは、置かれる場所以外そのまま出す。
enum PlacedRecord {
    /// 正規形で合成する（新規 / 変更 / 改名エントリ）。内容が変わっているか
    /// 名前が変わっているので、レコードを作り直すしかない。
    Synth {
        method: u16,
        crc32: u32,
        comp_size: u64,
        uncomp_size: u64,
        name: Vec<u8>,
    },
    /// ソースの CD レコードをそのまま運ぶ（未変更エントリ）。
    ///
    /// **列挙ではなく複写である理由**: ZIP の extra field は開かれた拡張点
    /// （UT / NTFS のタイムスタンプ、Unicode パス、…）。「保存するフィールドの
    /// 一覧」を書くと、一覧に無いものを黙って捨てる。レコードごと運べば、
    /// 実装が知らないものも保存される。時刻・パーミッション・コメント・
    /// version made by も同じ理由でここに含まれる。
    Verbatim(Vec<u8>),
}

impl PlacedEntry {
    /// `lho` に配置した `payload` を、合成レコードとして控える。圧縮サイズは
    /// `payload.stored` の実長から採る。
    fn synth(lho: u64, payload: &EntryPayload, name: &[u8]) -> PlacedEntry {
        PlacedEntry {
            local_header_offset: lho,
            record: PlacedRecord::Synth {
                method: payload.method,
                crc32: payload.crc32,
                comp_size: payload.stored.len() as u64,
                uncomp_size: payload.uncomp_size,
                name: name.to_vec(),
            },
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

        // 未変更なら再配置で済ませる。合成し直さないので、時刻・パーミッション・
        // コメント・extra field が保たれる。
        if dirty_name.is_none() && verbatim_eligible(entry) {
            place_verbatim(archive, &ar, &mut body, &mut placed, entry)?;
            continue;
        }

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
            record_in_place(archive, &mut placed, entry)?;
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

/// [`build_incremental`] が返した追記バイト列から、新しい Central Directory の
/// バイト列を切り出す。`base` は追記開始オフセット（＝旧アーカイブ長）。
///
/// commit **後**のアーカイブの fingerprint（`cd_hash`）を、書き込む**前**に算出する
/// ために使う（ADR 0017 の commit intent）。追記領域の末尾には正規形の EOCD が
/// 22 バイトで入っているので、そこから CD の位置と長さを読む。INCREMENTAL では
/// 新 CD 全体が追記領域に収まるので、旧アーカイブのバイト列は要らない。
///
/// 形が合わなければ `None`（この関数が見るのは自分で組み立てた直後のバイト列なので、
/// `None` は入力不正ではなく内部の不整合を意味する）。
pub fn appended_cd_block(appended: &[u8], base: u64) -> Option<&[u8]> {
    const EOCD_LEN: usize = 22;
    let eocd = appended.len().checked_sub(EOCD_LEN)?;
    let sig = u32::from_le_bytes(appended[eocd..eocd + 4].try_into().ok()?);
    if sig != EOCD_SIG {
        return None;
    }
    let cd_size = u32::from_le_bytes(appended[eocd + 12..eocd + 16].try_into().ok()?) as usize;
    let cd_offset = u32::from_le_bytes(appended[eocd + 16..eocd + 20].try_into().ok()?) as u64;
    let start = cd_offset.checked_sub(base)? as usize;
    let end = start.checked_add(cd_size)?;
    appended.get(start..end)
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
fn record_in_place(
    archive: &[u8],
    placed: &mut Vec<PlacedEntry>,
    entry: &CdEntry,
) -> Result<(), CommitError> {
    if entry.local_header_offset > u32::MAX as u64
        || entry.compressed_size > u32::MAX as u64
        || entry.uncompressed_size > u32::MAX as u64
    {
        return Err(CommitError::TooLarge);
    }
    // 追記コミットでは未変更エントリは 1 バイトも動かない。CD レコードも
    // そのまま運ぶ（offset の差し替えは同じ値の書き戻しになる）。
    let rec_at = entry.cd_record_offset as usize;
    let rec_end = rec_at
        .checked_add(entry.cd_record_len as usize)
        .filter(|&e| e <= archive.len())
        .ok_or(CommitError::Zip(ZipError::Truncated))?;
    placed.push(PlacedEntry {
        local_header_offset: entry.local_header_offset,
        record: PlacedRecord::Verbatim(archive[rec_at..rec_end].to_vec()),
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
    let entry = PlacedEntry::synth(lho, payload, name);
    let PlacedRecord::Synth { comp_size, uncomp_size, .. } = &entry.record else {
        unreachable!("synth() always builds PlacedRecord::Synth")
    };
    if entry.local_header_offset > u32::MAX as u64
        || *comp_size > u32::MAX as u64
        || *uncomp_size > u32::MAX as u64
    {
        return Err(CommitError::TooLarge);
    }
    write_lfh(body, &entry);
    body.extend_from_slice(&payload.stored);
    placed.push(entry);
    Ok(())
}

/// 未変更エントリを**再配置**する: LFH と圧縮データをバイト範囲のまま複写し、
/// CD レコードも複写して、置かれた場所だけ差し替える。合成しないので、時刻・
/// パーミッション・コメント・extra field・version made by が失われない。
///
/// 呼び出す前に [`verbatim_eligible`] で適格性を確かめること。
fn place_verbatim(
    archive: &[u8],
    ar: &Archive<'_>,
    body: &mut Vec<u8>,
    placed: &mut Vec<PlacedEntry>,
    entry: &CdEntry,
) -> Result<(), CommitError> {
    let rec_at = entry.cd_record_offset as usize;
    let rec_end = rec_at
        .checked_add(entry.cd_record_len as usize)
        .filter(|&e| e <= archive.len())
        .ok_or(CommitError::Zip(ZipError::Truncated))?;
    let rec = &archive[rec_at..rec_end];

    // CD の 32 ビット offset 欄が Zip64 番兵だと、差し替え先を書けない
    // （実値は extra field 側）。Zip64 出力は未対応なので素直に断る。
    if u32::from_le_bytes(rec[42..46].try_into().unwrap()) == u32::MAX {
        return Err(CommitError::TooLarge);
    }

    let lfh_start = entry.local_header_offset as usize;
    let data_start = ar.data_offset(entry).map_err(CommitError::Zip)? as usize;
    let data_end = data_start
        .checked_add(entry.compressed_size as usize)
        .filter(|&e| e <= archive.len() && e >= lfh_start)
        .ok_or(CommitError::Zip(ZipError::Truncated))?;

    let lho = body.len() as u64;
    if lho > u32::MAX as u64 {
        return Err(CommitError::TooLarge);
    }
    body.extend_from_slice(&archive[lfh_start..data_end]);
    placed.push(PlacedEntry {
        local_header_offset: lho,
        record: PlacedRecord::Verbatim(rec.to_vec()),
    });
    Ok(())
}

/// 未変更エントリをレコードごと運べるか。
///
/// 汎用目的フラグ bit 3（データディスクリプタ）が立っていると、圧縮データの
/// 後ろに 12 / 16 バイトのディスクリプタが続き、範囲の終端が CD のサイズからは
/// 決まらない。その場合は合成側へ倒す（合成は実サイズを LFH に書き、フラグを
/// 落とすので、出力としては正しくなる）。
fn verbatim_eligible(entry: &CdEntry) -> bool {
    entry.flags & 0x0008 == 0
}

/// 正規形のローカルファイルヘッダ（extra field なし）を書く。合成レコード専用。
fn write_lfh(out: &mut Vec<u8>, e: &PlacedEntry) {
    let PlacedRecord::Synth { method, crc32, comp_size, uncomp_size, name } = &e.record else {
        unreachable!("verbatim entries copy their local header instead")
    };
    push_u32(out, LFH_SIG);
    push_u16(out, 20); // version needed
    push_u16(out, 0); // flags
    push_u16(out, *method);
    push_u16(out, 0); // mod time
    push_u16(out, 0); // mod date
    push_u32(out, *crc32);
    push_u32(out, *comp_size as u32);
    push_u32(out, *uncomp_size as u32);
    push_u16(out, name.len() as u16);
    push_u16(out, 0); // extra len
    out.extend_from_slice(name);
}

/// Central Directory ファイルヘッダを書く。
///
/// 未変更エントリはソースのレコードをそのまま置き、**置かれた場所だけ**を
/// 差し替える。変更 / 新規 / 改名エントリは正規形で合成する。
fn write_cdfh(out: &mut Vec<u8>, e: &PlacedEntry) {
    match &e.record {
        PlacedRecord::Verbatim(rec) => {
            let at = out.len();
            out.extend_from_slice(rec);
            // 変わったのは配置だけ。offset 欄（+42）以外は 1 バイトも触らない。
            out[at + 42..at + 46]
                .copy_from_slice(&(e.local_header_offset as u32).to_le_bytes());
        }
        PlacedRecord::Synth { method, crc32, comp_size, uncomp_size, name } => {
            push_u32(out, CDFH_SIG);
            push_u16(out, 20); // version made by
            push_u16(out, 20); // version needed
            push_u16(out, 0); // flags
            push_u16(out, *method);
            push_u16(out, 0); // mod time
            push_u16(out, 0); // mod date
            push_u32(out, *crc32);
            push_u32(out, *comp_size as u32);
            push_u32(out, *uncomp_size as u32);
            push_u16(out, name.len() as u16);
            push_u16(out, 0); // extra len
            push_u16(out, 0); // comment len
            push_u16(out, 0); // disk start
            push_u16(out, 0); // internal attrs
            push_u32(out, 0); // external attrs
            push_u32(out, e.local_header_offset as u32);
            out.extend_from_slice(name);
        }
    }
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
pub(crate) fn crc32(data: &[u8]) -> u32 {
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

    use crate::difflayer::DiffLayer;
    use crate::entrytable::EntryTable;

    // ───────────────────── フィクスチャ ─────────────────────
    //
    // `build_full` / `build_incremental` を**直に**叩くための ZIP 組み立て。
    // これまでこのモジュールの単体テストは `crc32` と `deflate` のヘルパだけで、
    // 出力の組み立て自体は `disk.rs` 経由の間接カバレッジしか無かった。実際に
    // 欠陥が出た場所でもあるので、契約をここで直接押さえる。

    /// フィクスチャの 1 エントリ。既定は「メタデータの無い STORE エントリ」。
    struct Fx {
        name: Vec<u8>,
        data: Vec<u8>,
        mod_time: u16,
        mod_date: u16,
        external_attr: u32,
        extra: Vec<u8>,
        comment: Vec<u8>,
        /// 汎用目的フラグ。bit 3 を立てるとデータディスクリプタ付きになる。
        flags: u16,
        /// CD の local header offset 欄を Zip64 番兵にし、実値を extra へ入れる。
        zip64_offset: bool,
    }

    impl Fx {
        fn new(name: &[u8], data: &[u8]) -> Fx {
            Fx {
                name: name.to_vec(),
                data: data.to_vec(),
                mod_time: 0,
                mod_date: 0,
                external_attr: 0,
                extra: Vec::new(),
                comment: Vec::new(),
                flags: 0,
                zip64_offset: false,
            }
        }
        fn rich(mut self) -> Fx {
            self.mod_time = 0x6C21;
            self.mod_date = 0x5921;
            self.external_attr = 0o100_644 << 16;
            // UT extended timestamp
            self.extra = vec![0x55, 0x54, 0x05, 0x00, 0x01, 0x40, 0xF3, 0xD3, 0x66];
            self.comment = b"carried through".to_vec();
            self
        }
    }

    /// STORE のみの ZIP を組む。CRC-32 は必ず本物を入れる（0 を書くと第三者の
    /// リーダに弾かれる。`disk.rs` の `store_zip` で実際に踏んだ罠）。
    fn build_zip(entries: &[Fx]) -> Vec<u8> {
        let mut body = Vec::new();
        let mut cd = Vec::new();
        for e in entries {
            let lho = body.len() as u32;
            let crc = crc32(&e.data);
            let dd = e.flags & 0x0008 != 0;

            push_u32(&mut body, LFH_SIG);
            push_u16(&mut body, 20);
            push_u16(&mut body, e.flags);
            push_u16(&mut body, 0); // STORE
            push_u16(&mut body, e.mod_time);
            push_u16(&mut body, e.mod_date);
            // データディスクリプタ付きなら LFH の crc / サイズは 0。
            push_u32(&mut body, if dd { 0 } else { crc });
            push_u32(&mut body, if dd { 0 } else { e.data.len() as u32 });
            push_u32(&mut body, if dd { 0 } else { e.data.len() as u32 });
            push_u16(&mut body, e.name.len() as u16);
            push_u16(&mut body, 0); // LFH extra は空（CD と食い違わせる）
            body.extend_from_slice(&e.name);
            body.extend_from_slice(&e.data);
            if dd {
                push_u32(&mut body, 0x0807_4b50); // ディスクリプタ署名
                push_u32(&mut body, crc);
                push_u32(&mut body, e.data.len() as u32);
                push_u32(&mut body, e.data.len() as u32);
            }

            let mut extra = e.extra.clone();
            if e.zip64_offset {
                // Zip64 extended information: offset のみ番兵にする。
                extra.extend_from_slice(&0x0001u16.to_le_bytes());
                extra.extend_from_slice(&8u16.to_le_bytes());
                extra.extend_from_slice(&(lho as u64).to_le_bytes());
            }
            push_u32(&mut cd, CDFH_SIG);
            push_u16(&mut cd, 0x031E); // version made by: Unix
            push_u16(&mut cd, 20);
            push_u16(&mut cd, e.flags);
            push_u16(&mut cd, 0);
            push_u16(&mut cd, e.mod_time);
            push_u16(&mut cd, e.mod_date);
            push_u32(&mut cd, crc);
            push_u32(&mut cd, e.data.len() as u32);
            push_u32(&mut cd, e.data.len() as u32);
            push_u16(&mut cd, e.name.len() as u16);
            push_u16(&mut cd, extra.len() as u16);
            push_u16(&mut cd, e.comment.len() as u16);
            push_u16(&mut cd, 0);
            push_u16(&mut cd, 0);
            push_u32(&mut cd, e.external_attr);
            push_u32(&mut cd, if e.zip64_offset { u32::MAX } else { lho });
            cd.extend_from_slice(&e.name);
            cd.extend_from_slice(&extra);
            cd.extend_from_slice(&e.comment);
        }
        let cd_offset = body.len() as u32;
        let cd_size = cd.len() as u32;
        body.extend_from_slice(&cd);
        push_u32(&mut body, EOCD_SIG);
        push_u16(&mut body, 0);
        push_u16(&mut body, 0);
        push_u16(&mut body, entries.len() as u16);
        push_u16(&mut body, entries.len() as u16);
        push_u32(&mut body, cd_size);
        push_u32(&mut body, cd_offset);
        push_u16(&mut body, 0);
        body
    }

    /// CD レコードを、名前をキーに「オフセット欄を除いたバイト列」で取り出す。
    /// 再配置の検証は「置かれた場所以外は 1 バイトも変わらない」なので、
    /// 比較からオフセットだけ抜く。
    fn cd_record_without_offset(zip: &[u8], name: &[u8]) -> Vec<u8> {
        let ar = Archive::parse(zip).expect("valid zip");
        let e = ar.entries().iter().find(|e| e.name == name).expect("entry");
        let at = e.cd_record_offset as usize;
        let mut rec = zip[at..at + e.cd_record_len as usize].to_vec();
        rec[42..46].fill(0);
        rec
    }

    /// エントリの LFH からデータ末尾までのバイト列。
    fn lfh_and_data(zip: &[u8], name: &[u8]) -> Vec<u8> {
        let ar = Archive::parse(zip).expect("valid zip");
        let e = ar.entries().iter().find(|e| e.name == name).expect("entry");
        let start = e.local_header_offset as usize;
        let end = ar.data_offset(e).unwrap() as usize + e.compressed_size as usize;
        zip[start..end].to_vec()
    }

    fn clean() -> (DiffLayer, EntryTable) {
        (DiffLayer::new(8), EntryTable::new())
    }

    // ───────────────────── 再配置の契約 ─────────────────────

    /// **commit はエントリを再配置するのであって書き直すのではない。**
    /// dirty が無ければ、全エントリの CD レコードと LFH+データは、置かれる場所を
    /// 除いてバイト単位で一致しなければならない。
    #[test]
    fn full_commit_relocates_records_byte_for_byte() {
        let src = build_zip(&[
            Fx::new(b"a.bin", b"first entry").rich(),
            Fx::new(b"b.bin", b"second entry"),
            Fx::new(b"c.bin", b"third entry").rich(),
        ]);
        let (diff, table) = clean();
        let out = build_full(&src, &[], &diff, &table).expect("build_full");

        for name in [&b"a.bin"[..], b"b.bin", b"c.bin"] {
            assert_eq!(
                cd_record_without_offset(&out, name),
                cd_record_without_offset(&src, name),
                "{}: central directory record was rewritten",
                String::from_utf8_lossy(name)
            );
            assert_eq!(
                lfh_and_data(&out, name),
                lfh_and_data(&src, name),
                "{}: local header or data was rewritten",
                String::from_utf8_lossy(name)
            );
        }
    }

    /// エントリ順序はソースの順を保つ。順序が保たれることが、clean な
    /// アーカイブに対する `compact()` の再現性の前提になる。
    #[test]
    fn full_commit_keeps_source_entry_order() {
        let src = build_zip(&[
            Fx::new(b"z.bin", b"z"),
            Fx::new(b"a.bin", b"a"),
            Fx::new(b"m.bin", b"m"),
        ]);
        let (diff, table) = clean();
        let out = build_full(&src, &[], &diff, &table).expect("build_full");

        let ar = Archive::parse(&out).unwrap();
        let names: Vec<&[u8]> = ar.entries().iter().map(|e| e.name.as_slice()).collect();
        assert_eq!(names, vec![&b"z.bin"[..], b"a.bin", b"m.bin"]);
    }

    /// clean なアーカイブへの FULL commit は冪等 —— 2 回通しても同じバイト列。
    /// 再配置が「置き場所以外触らない」なら、これは自動的に成り立つ。
    #[test]
    fn full_commit_of_a_clean_archive_is_idempotent() {
        let src = build_zip(&[Fx::new(b"a.bin", b"one").rich(), Fx::new(b"b.bin", b"two")]);
        let (diff, table) = clean();
        let once = build_full(&src, &[], &diff, &table).expect("first");
        let twice = build_full(&once, &[], &diff, &table).expect("second");
        assert_eq!(once, twice, "compaction of a clean archive must be stable");
    }

    /// 非 UTF-8 のエントリ名（CP437 など）はオーバーレイの対象外だが、
    /// 再配置は通らなければならない。名前も中身も変わらないこと。
    #[test]
    fn full_commit_carries_non_utf8_entry_names() {
        let name: &[u8] = &[0x83, 0x86, 0x81, 0x5B, 0x83, 0x55, 0x2E, 0x62, 0x69, 0x6E];
        assert!(std::str::from_utf8(name).is_err(), "fixture name must not be UTF-8");
        let src = build_zip(&[Fx::new(name, b"shift-jis named entry").rich()]);
        let (diff, table) = clean();
        let out = build_full(&src, &[], &diff, &table).expect("build_full");

        assert_eq!(cd_record_without_offset(&out, name), cd_record_without_offset(&src, name));
        assert_eq!(lfh_and_data(&out, name), lfh_and_data(&src, name));
    }

    // ───────────────────── 再配置できない場合 ─────────────────────

    /// データディスクリプタ付き（汎用フラグ bit 3）は範囲コピーの終端が CD から
    /// 決まらないので合成へ倒れる。**出力は妥当でなければならない**: サイズと
    /// CRC が実体と合い、bit 3 は落ちていること。
    #[test]
    fn data_descriptor_entry_falls_back_to_synthesis() {
        let mut fx = Fx::new(b"dd.bin", b"written with a data descriptor");
        fx.flags = 0x0008;
        let src = build_zip(&[fx]);
        let (diff, table) = clean();
        let out = build_full(&src, &[], &diff, &table).expect("build_full");

        let ar = Archive::parse(&out).unwrap();
        let e = ar.entries().iter().find(|e| e.name == b"dd.bin").expect("entry");
        assert_eq!(e.flags & 0x0008, 0, "data descriptor flag must be cleared");
        assert_eq!(e.uncompressed_size, b"written with a data descriptor".len() as u64);
        assert_eq!(e.crc32, crc32(b"written with a data descriptor"));
        assert_eq!(
            ar.entry_data(e).unwrap(),
            b"written with a data descriptor",
            "the descriptor bytes must not leak into the entry data"
        );
    }

    /// CD の 32 ビット offset 欄が Zip64 番兵のエントリは、差し替え先を書けない
    /// （実値は extra field 側）。Zip64 出力は未対応なので素直に断る。
    #[test]
    fn zip64_offset_sentinel_is_rejected_rather_than_corrupted() {
        let mut fx = Fx::new(b"big.bin", b"pretends to live past 4 GiB");
        fx.zip64_offset = true;
        let src = build_zip(&[fx]);
        // フィクスチャ自体は読める（番兵は extra から解決される）。
        assert!(Archive::parse(&src).is_ok());

        let (diff, table) = clean();
        match build_full(&src, &[], &diff, &table) {
            Err(CommitError::TooLarge) => {}
            other => panic!("expected TooLarge, got {:?}", other.map(|v| v.len())),
        }
    }

    // ───────────────────── INCREMENTAL ─────────────────────

    /// 追記コミットでは未変更エントリは 1 バイトも動かない。dirty が無ければ
    /// 追記されるのは新しい CD と EOCD だけで、レコードはそのまま運ばれる。
    #[test]
    fn incremental_appends_only_a_directory_when_nothing_is_dirty() {
        let src = build_zip(&[Fx::new(b"a.bin", b"one").rich(), Fx::new(b"b.bin", b"two")]);
        let (diff, table) = clean();
        let appended = build_incremental(&src, &[], &diff, &table).expect("build_incremental");

        let mut out = src.clone();
        out.extend_from_slice(&appended);
        for name in [&b"a.bin"[..], b"b.bin"] {
            assert_eq!(
                cd_record_without_offset(&out, name),
                cd_record_without_offset(&src, name),
                "{}: record was rewritten by an append-only commit",
                String::from_utf8_lossy(name)
            );
            let ar_out = Archive::parse(&out).unwrap();
            let ar_src = Archive::parse(&src).unwrap();
            let eo = ar_out.entries().iter().find(|e| e.name == name).unwrap();
            let es = ar_src.entries().iter().find(|e| e.name == name).unwrap();
            assert_eq!(eo.local_header_offset, es.local_header_offset, "entry moved");
        }
    }

    /// `appended_cd_block` は追記領域から新しい Central Directory を切り出す。
    /// commit intent の post 指紋を「書く前に」算出するのに使う（ADR 0017）。
    #[test]
    fn appended_cd_block_locates_the_new_directory() {
        let src = build_zip(&[Fx::new(b"a.bin", b"one"), Fx::new(b"b.bin", b"two")]);
        let (diff, table) = clean();
        let appended = build_incremental(&src, &[], &diff, &table).expect("build_incremental");

        let cd = appended_cd_block(&appended, src.len() as u64).expect("cd block");

        // 切り出した範囲が、実際に出来上がるアーカイブの CD と一致すること。
        let mut out = src.clone();
        out.extend_from_slice(&appended);
        let ar = Archive::parse(&out).unwrap();
        assert_eq!(cd, ar.cd_block());
    }

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
