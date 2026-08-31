//! マウント / 読み取り経路（設計 READ PATH と FIRST-OPEN の最小核）。
//!
//! [`Mount`] は archive.zip のバイト列（呼び出し側が mmap 済み）と、それに対応
//! する vmidx 像、[`PageCache`] を束ね、`read(path, offset, len)` を提供する。
//! 読み取りは設計 READ PATH の三段:「Tier 1 (Diff Layer) → Tier 2 (vmdirty) →
//! ソース ZIP (ページキャッシュ経由)」。書き込み経路は [`write_into`]、その
//! 読み戻しは [`read_dirty`] が担う:
//!
//! - [`read_cached`] — 要求範囲の各ページをキャッシュから取り、ミスしたら
//!   [`fill_run`] で目標ページ + read-ahead ページ分をまとめて展開・充填する。
//! - [`fill_run`]（キャッシュミス時）:
//!   1. vmidx を `lookup(path)` してエントリを引く（呼び出し側で済ませて渡す）
//!   2. `provider_type` から [`provider`](crate::provider) を選ぶ
//!   3. 目標ページから連続する未常駐ページのランを決める（read-ahead）
//!   4. `nearest_checkpoint(record, run_start)` を起点に `provider.read_range` で
//!      ラン全体を 1 回で展開し、ページに切ってキャッシュへ入れる
//!
//! ラン一括展開により、read-ahead は「目標 + N ページ」を 1 回の checkpoint 復元
//! + 前進デコードで賄う（設計 READ PATH の read-ahead 償却。デコーダ状態を跨いで
//! 持ち回る最適化はさらに後段）。[`read_entry`] は索引だけを使う無キャッシュの
//! 下位プリミティブとして残す。
//!
//! vmidx 像は所有し（`Vec<u8>`）、`read` のたびに [`Vmidx::parse`] で軽量ビューを
//! 作る（自己参照構造を避けるため。parse はヘッダ 128 バイトの decode と領域境界
//! 検査のみで安価）。[`Mount`] はページキャッシュを `RefCell` で内部可変に持つ
//! ため、`read(&self)` のまま使えるが `Sync` ではない（並行アクセスは lock 層
//! ＝後段の担当）。

use crate::archive::{Archive, ZipError};
use crate::commit::{build_full, build_incremental, CommitError};
use crate::difflayer::DiffLayer;
use crate::entrytable::{EntryTable, Kind};
use crate::index_build::{build_vmidx_eager, BuildError, BuildParams};
use crate::page::{page_count, page_extent, PageCache, PageConfig, PageKey};
use crate::provider::{builtin_provider, check_range, ProviderError};
use crate::tier2::Tier2;
use crate::vmdirty::MetaOp;
use crate::vmidx::{
    hash_cd_block, DecodeError, EntryRecord, FingerprintVerdict, ProviderType, SourceStat, Vmidx,
};
use std::cell::RefCell;
use std::fmt;

/// マウントを開く際の失敗。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenError {
    /// ソース ZIP の parse に失敗した。
    Zip(ZipError),
    /// vmidx 像の構築に失敗した。
    Build(BuildError),
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OpenError::Zip(e) => write!(f, "mount open: {e}"),
            OpenError::Build(e) => write!(f, "mount open: {e}"),
        }
    }
}

impl std::error::Error for OpenError {}

/// `read()` の失敗。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    /// 指定の `path` がアーカイブに無い。
    NotFound,
    /// プロバイダを持たない圧縮種別（未対応メソッド）。
    Unsupported(ProviderType),
    /// vmidx の decode 失敗（破損 → 本来は再構築の合図）。
    Vmidx(DecodeError),
    /// プロバイダの解凍失敗。
    Provider(ProviderError),
    /// レコードの `data_offset` / `compressed_size` がソース ZIP の範囲外。
    DataOutOfRange,
    /// open 後にソース archive.zip が外部から変更された（設計 SNAPSHOT
    /// CONSISTENCY の ESTALE）。検出後マウントは STALE になり、以降の read は
    /// 一律これを返す（close + reopen が必要）。
    Stale,
    /// Tier 2（vmdirty）からページを読み戻す段の I/O 失敗。
    Tier2(String),
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadError::NotFound => write!(f, "read: entry not found"),
            ReadError::Unsupported(p) => write!(f, "read: unsupported provider {p:?}"),
            ReadError::Vmidx(e) => write!(f, "read: {e}"),
            ReadError::Provider(e) => write!(f, "read: {e}"),
            ReadError::DataOutOfRange => write!(f, "read: entry data outside archive"),
            ReadError::Stale => write!(f, "read: archive changed since mount (ESTALE)"),
            ReadError::Tier2(e) => write!(f, "read: tier 2 spill read failed: {e}"),
        }
    }
}

impl std::error::Error for ReadError {}

/// `write()` の失敗（設計 WRITE PATH）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteError {
    /// 指定の `path` がアーカイブに無い（M2 は既存エントリの変更のみ。create は M2+）。
    NotFound,
    /// COW で書けない圧縮種別（STORE / 標準 DEFLATE 以外）。
    Unsupported(ProviderType),
    /// vmidx の decode 失敗。
    Vmidx(DecodeError),
    /// COW の元ページをソースから読む段で失敗した。
    Read(Box<ReadError>),
    /// Tier 2（vmdirty）への spill / write-hit 書き出しの I/O 失敗。
    Spill(String),
}

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WriteError::NotFound => write!(f, "write: entry not found"),
            WriteError::Unsupported(p) => write!(f, "write: unsupported provider {p:?}"),
            WriteError::Vmidx(e) => write!(f, "write: {e}"),
            WriteError::Read(e) => write!(f, "write: copy-on-write read failed: {e}"),
            WriteError::Spill(e) => write!(f, "write: tier 2 spill failed: {e}"),
        }
    }
}

impl std::error::Error for WriteError {}

/// エントリ表 + vmidx からの解決失敗（read / write / entry op の共通入口）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// エントリが存在しない（tombstone または vmidx に無い）。
    NotFound,
    /// ソースが COW / 再圧縮できない圧縮種別（STORE / 標準 DEFLATE 以外）。
    Unsupported(ProviderType),
    /// vmidx の decode 失敗。
    Vmidx(DecodeError),
}

impl From<ResolveError> for ReadError {
    fn from(e: ResolveError) -> ReadError {
        match e {
            ResolveError::NotFound => ReadError::NotFound,
            ResolveError::Unsupported(p) => ReadError::Unsupported(p),
            ResolveError::Vmidx(d) => ReadError::Vmidx(d),
        }
    }
}

impl From<ResolveError> for WriteError {
    fn from(e: ResolveError) -> WriteError {
        match e {
            ResolveError::NotFound => WriteError::NotFound,
            ResolveError::Unsupported(p) => WriteError::Unsupported(p),
            ResolveError::Vmidx(d) => WriteError::Vmidx(d),
        }
    }
}

/// エントリの「未変更データの出どころ」と元サイズ（エントリ表 + vmidx の解決結果）。
pub struct ResolvedEntry {
    /// 未変更ページを読むソース vmidx 名。`None` = created（ソース無し）。
    /// ④a では Source の場合は現在名に一致する（rename で分岐するのは ④b）。
    pub source: Option<String>,
    /// ソースの元 `uncompressed_size`（created は 0）。
    pub original_size: u64,
}

/// エントリ操作（create / remove / truncate）の失敗（設計 ENTRY OPERATIONS）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryError {
    /// create 先が既に存在する（EEXIST）。
    Exists,
    /// 対象が存在しない（ENOENT）。
    NotFound,
    /// 再圧縮できない圧縮種別の既存エントリを truncate しようとした。
    Unsupported(ProviderType),
    /// vmidx の decode 失敗。
    Vmidx(DecodeError),
    /// vmdirty への METADATA 追記（journaling）の I/O 失敗。
    Journal(String),
}

impl From<ResolveError> for EntryError {
    fn from(e: ResolveError) -> EntryError {
        match e {
            ResolveError::NotFound => EntryError::NotFound,
            ResolveError::Unsupported(p) => EntryError::Unsupported(p),
            ResolveError::Vmidx(d) => EntryError::Vmidx(d),
        }
    }
}

impl fmt::Display for EntryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EntryError::Exists => write!(f, "entry op: already exists"),
            EntryError::NotFound => write!(f, "entry op: not found"),
            EntryError::Unsupported(p) => write!(f, "entry op: unsupported provider {p:?}"),
            EntryError::Vmidx(e) => write!(f, "entry op: {e}"),
            EntryError::Journal(e) => write!(f, "entry op: journal write failed: {e}"),
        }
    }
}

impl std::error::Error for EntryError {}

/// 1 つのアーカイブに対するマウント。読み取りに加え、Diff Layer Tier 1 を介した
/// 書き込み（[`write`](Mount::write)）と FULL [`commit_full`](Mount::commit_full) を提供する
/// （設計 WRITE PATH / commit() FLOW の M2 最小形）。ソース ZIP は commit まで
/// 一切書き換えない。
pub struct Mount<'a> {
    archive: &'a [u8],
    vmidx_image: Vec<u8>,
    cfg: PageConfig,
    cache: RefCell<PageCache>,
    /// 未コミットの dirty ページ（Tier 1）。read は Diff Layer を最優先で見る。
    diff: RefCell<DiffLayer>,
    /// セッション内の構造変更（create / remove）を vmidx に被せる表。
    entries: RefCell<EntryTable>,
}

impl<'a> Mount<'a> {
    /// archive バイト列から EAGER 索引を構築して開く（vmidx が無い「コールド
    /// オープン」相当）。`params` の stat 値が fingerprint に入る。ページ設定は
    /// 既定（[`PageConfig::default`]）。
    pub fn open(archive: &'a [u8], params: &BuildParams) -> Result<Mount<'a>, OpenError> {
        Mount::open_with_page_config(archive, params, PageConfig::default())
    }

    /// [`Mount::open`] にページ設定を指定する版。
    pub fn open_with_page_config(
        archive: &'a [u8],
        params: &BuildParams,
        cfg: PageConfig,
    ) -> Result<Mount<'a>, OpenError> {
        let (vmidx_image, _) = resolve_index(archive, None, params)?;
        Ok(Mount::assemble(archive, vmidx_image, cfg))
    }

    /// 既存の vmidx 像を検証して開く（設計 Section 7 の open() カスケード）。
    /// `Valid` / `ValidStale` ならその像を使い、`Invalid` または parse 失敗なら
    /// EAGER で再構築する（「どの失敗でも応答は破棄して再構築」）。
    pub fn open_with_index(
        archive: &'a [u8],
        vmidx_image: Vec<u8>,
        params: &BuildParams,
    ) -> Result<Mount<'a>, OpenError> {
        let (vmidx_image, _) = resolve_index(archive, Some(vmidx_image), params)?;
        Ok(Mount::assemble(archive, vmidx_image, PageConfig::default()))
    }

    fn assemble(archive: &'a [u8], vmidx_image: Vec<u8>, cfg: PageConfig) -> Mount<'a> {
        let cache = RefCell::new(PageCache::from_config(&cfg));
        let diff = RefCell::new(DiffLayer::new(cfg.page_size));
        Mount {
            archive,
            vmidx_image,
            cfg,
            cache,
            diff,
            entries: RefCell::new(EntryTable::new()),
        }
    }

    /// 構築済みの vmidx 像（ファイル I/O 層が vmidx.tmp として書き出す対象）。
    pub fn index_bytes(&self) -> &[u8] {
        &self.vmidx_image
    }

    /// エントリ `path` の展開ストリーム `[offset, offset + len)` を読む。
    /// dirty なエントリは Diff Layer から（設計 READ PATH の Tier 1 最優先）、
    /// それ以外はページキャッシュ経由（ミス時のみ展開 + read-ahead 充填）。
    pub fn read(&self, path: &str, offset: u64, len: usize) -> Result<Vec<u8>, ReadError> {
        let resolved = resolve_entry(&self.entries.borrow(), &self.vmidx_image, path)?;
        // overlaid（dirty / created / 別名）は Diff Layer 経由で読む。別名は
        // ソース名が現在名と異なるので（`source != Some(path)`）ここに入り、
        // read_dirty がソース名でソース ZIP を読む。
        if self.diff.borrow().is_dirty(path) || resolved.source.as_deref() != Some(path) {
            let ctx = EntryCtx {
                archive: self.archive,
                vmidx_image: &self.vmidx_image,
                path,
                source: resolved.source.as_deref(),
                original_size: resolved.original_size,
            };
            return read_dirty(&ctx, &self.diff.borrow(), None, offset, len);
        }
        let mut cache = self.cache.borrow_mut();
        let mut io = PageIo { cache: &mut cache, cfg: &self.cfg };
        // メモリ上マウントには再 stat 対象のファイルが無いので鮮度チェックは no-op。
        read_cached(
            self.archive,
            &self.vmidx_image,
            &mut io,
            path,
            offset,
            len,
            || Ok(()),
        )
    }

    /// エントリ `path` の `[offset, offset + data.len())` を書く（設計 WRITE PATH）。
    /// ソース ZIP は触らず、Diff Layer Tier 1 に COW で取り込む。末尾を超える
    /// 書き込みはエントリを伸ばし（implicit extension）、間のページはゼロ埋めされる
    /// （commit で materialise）。`dirty_limit` は M2 では無制限（spill なし）。
    pub fn write(&self, path: &str, offset: u64, data: &[u8]) -> Result<(), WriteError> {
        let resolved = resolve_entry(&self.entries.borrow(), &self.vmidx_image, path)?;
        let ctx = EntryCtx {
            archive: self.archive,
            vmidx_image: &self.vmidx_image,
            path,
            source: resolved.source.as_deref(),
            original_size: resolved.original_size,
        };
        write_into(&ctx, &mut self.diff.borrow_mut(), None, offset, data)
    }

    /// 空のエントリを作る（設計 create()）。既存（未削除）なら [`EntryError::Exists`]。
    pub fn create(&self, path: &str) -> Result<(), EntryError> {
        entry_create(
            &mut self.entries.borrow_mut(),
            &mut self.diff.borrow_mut(),
            &self.vmidx_image,
            None,
            path,
        )
    }

    /// エントリを削除する（設計 remove()。tombstone）。存在しなければ
    /// [`EntryError::NotFound`]。
    pub fn remove(&self, path: &str) -> Result<(), EntryError> {
        entry_remove(
            &mut self.entries.borrow_mut(),
            &mut self.diff.borrow_mut(),
            &self.vmidx_image,
            None,
            path,
        )
    }

    /// エントリの論理サイズを変える（設計 truncate()）。縮小は末尾ページを落とし、
    /// 拡大は gap をゼロ埋め扱いにする。存在しなければ [`EntryError::NotFound`]。
    pub fn truncate(&self, path: &str, new_size: u64) -> Result<(), EntryError> {
        entry_truncate(
            &self.entries.borrow(),
            &mut self.diff.borrow_mut(),
            &self.vmidx_image,
            None,
            path,
            new_size,
        )
    }

    /// エントリ `old` を `new` へ rename する（設計 rename()）。データは再圧縮せず
    /// `new` を `old` のソースへの別名にする。`old` が無ければ [`EntryError::NotFound`]、
    /// `new` が既に存在すれば（同名指定含む）[`EntryError::Exists`]。
    pub fn rename(&self, old: &str, new: &str) -> Result<(), EntryError> {
        entry_rename(
            &mut self.entries.borrow_mut(),
            &mut self.diff.borrow_mut(),
            &self.vmidx_image,
            None,
            old,
            new,
        )
    }

    /// dirty なエントリ、または構造変更（create / remove / rename）があるか
    /// （commit が実体を持つか）。
    pub fn is_dirty(&self) -> bool {
        !self.diff.borrow().is_empty() || !self.entries.borrow().is_empty()
    }

    /// 全 dirty 変更を反映した新しい ZIP バイト列を返す（FULL commit。設計
    /// commit() FLOW の FULL path）。マウントを消費する: 呼び出し側は返った
    /// バイト列で開き直す（ディスクでは `archive.new.zip` に書いて `rename`）。
    ///
    /// メモリ版 `Mount` は INCREMENTAL/FULL 選択ポリシー（bloat 閾値）を持たない
    /// プリミティブ層。自動選択は閾値を持つ [`FileMount::commit`](crate::disk::FileMount::commit)
    /// に置く（`Mount` は `commit_full` / [`commit_incremental`](Self::commit_incremental)
    /// を明示的に呼ぶ）。
    pub fn commit_full(self) -> Result<Vec<u8>, CommitError> {
        build_full(
            self.archive,
            &self.vmidx_image,
            &self.diff.borrow(),
            &self.entries.borrow(),
        )
    }

    /// INCREMENTAL commit（設計 commit() FLOW の INCREMENTAL path。ADR 0012）。未変更
    /// エントリは元位置のまま、変更/新規/別名だけを末尾に追記した新しいアーカイブ
    /// バイト列を返す（メモリ版。ディスク版は実ファイルへの末尾追記 + truncate
    /// ロールバックで実装する）。`[既存バイト][追記分]` を連結して返し、マウントを
    /// 消費する。未変更エントリの再圧縮が無いぶん大きなアーカイブの小編集で安い。
    pub fn commit_incremental(self) -> Result<Vec<u8>, CommitError> {
        let appended = build_incremental(
            self.archive,
            &self.vmidx_image,
            &self.diff.borrow(),
            &self.entries.borrow(),
        )?;
        let mut out = Vec::with_capacity(self.archive.len() + appended.len());
        out.extend_from_slice(self.archive);
        out.extend_from_slice(&appended);
        Ok(out)
    }
}

/// open カスケード本体（設計 Section 7）。既存 vmidx 像があれば parse +
/// fingerprint を照合し、使えるならそのまま、`Invalid` / parse 失敗なら EAGER で
/// 再構築する。戻り値は (採用する像, 再構築したか)。ファイル I/O 層は再構築フラグ
/// を見て vmidx.tmp 書き出しの要否を決める。
pub fn resolve_index(
    archive: &[u8],
    existing: Option<Vec<u8>>,
    params: &BuildParams,
) -> Result<(Vec<u8>, bool), OpenError> {
    let ar = Archive::parse(archive).map_err(OpenError::Zip)?;
    if let Some(bytes) = existing {
        let live = SourceStat {
            file_size: params.source_file_size,
            inode: params.source_inode,
            mtime_ns: params.source_mtime_ns,
            cd_hash: hash_cd_block(ar.cd_block()),
        };
        let usable = match Vmidx::parse(&bytes) {
            Ok(v) => !matches!(v.check_fingerprint(&live), FingerprintVerdict::Invalid),
            Err(_) => false,
        };
        if usable {
            return Ok((bytes, false));
        }
    }
    let rebuilt = build_vmidx_eager(&ar, params).map_err(OpenError::Build)?;
    Ok((rebuilt, true))
}

/// vmidx 像とソース ZIP バイト列から 1 エントリの `[offset, offset + len)` を読む。
/// `archive` と `vmidx_image` はそれぞれ呼び出し側が所有（mmap / Vec）し、ここでは
/// 借用するだけ。
pub fn read_entry(
    archive: &[u8],
    vmidx_image: &[u8],
    path: &str,
    offset: u64,
    len: usize,
) -> Result<Vec<u8>, ReadError> {
    let vmidx = Vmidx::parse(vmidx_image).map_err(ReadError::Vmidx)?;
    let (_, record) = vmidx
        .lookup(path)
        .map_err(ReadError::Vmidx)?
        .ok_or(ReadError::NotFound)?;
    let provider =
        builtin_provider(record.provider_type).ok_or(ReadError::Unsupported(record.provider_type))?;
    let nearest = vmidx
        .nearest_checkpoint(&record, offset)
        .map_err(ReadError::Vmidx)?;

    let start = record.data_offset as usize;
    let end = start
        .checked_add(record.compressed_size as usize)
        .filter(|&e| e <= archive.len())
        .ok_or(ReadError::DataOutOfRange)?;
    let compressed = &archive[start..end];

    provider
        .read_range(
            compressed,
            nearest.as_ref(),
            offset,
            len,
            record.uncompressed_size,
        )
        .map_err(ReadError::Provider)
}

/// エントリ表 + vmidx から `path` の実効状態を解決する（read / write / truncate の
/// 共通入口）。tombstone・不在は [`ResolveError::NotFound`]、ソースが再圧縮不能な
/// 圧縮種別なら [`ResolveError::Unsupported`]。created はソース無し
/// （`original_size = 0`）。Source は ④a ではソース名 = 現在名。
pub fn resolve_entry(
    table: &EntryTable,
    vmidx_image: &[u8],
    path: &str,
) -> Result<ResolvedEntry, ResolveError> {
    let vmidx = Vmidx::parse(vmidx_image).map_err(ResolveError::Vmidx)?;
    // 別名（rename ターゲット）は現在名ではなく **ソース名**で vmidx を引く。
    if let Some(src) = table.aliased_source(path) {
        let (_, record) = vmidx
            .lookup(src)
            .map_err(ResolveError::Vmidx)?
            .ok_or(ResolveError::NotFound)?;
        if builtin_provider(record.provider_type).is_none() {
            return Err(ResolveError::Unsupported(record.provider_type));
        }
        return Ok(ResolvedEntry {
            source: Some(src.to_owned()),
            original_size: record.uncompressed_size,
        });
    }
    let rec = vmidx.lookup(path).map_err(ResolveError::Vmidx)?;
    match table.kind(path, rec.is_some()) {
        Kind::Absent => Err(ResolveError::NotFound),
        Kind::Created => Ok(ResolvedEntry {
            source: None,
            original_size: 0,
        }),
        Kind::Source => {
            let (_, record) = rec.ok_or(ResolveError::NotFound)?;
            if builtin_provider(record.provider_type).is_none() {
                return Err(ResolveError::Unsupported(record.provider_type));
            }
            Ok(ResolvedEntry {
                source: Some(path.to_owned()),
                original_size: record.uncompressed_size,
            })
        }
    }
}

/// 空のエントリを作る（設計 create()）。実効的に存在する（未削除）なら
/// [`EntryError::Exists`]。create-after-remove は tombstone を上書きして新規の
/// 空エントリを始める。`tier2` があれば METADATA CREATE を journal する。
pub fn entry_create(
    table: &mut EntryTable,
    diff: &mut DiffLayer,
    vmidx_image: &[u8],
    tier2: Option<&mut Tier2>,
    path: &str,
) -> Result<(), EntryError> {
    let in_vmidx = entry_in_vmidx(vmidx_image, path)?;
    if table.kind(path, in_vmidx) != Kind::Absent {
        return Err(EntryError::Exists);
    }
    // 残骸（前の remove で消し切れなかったページ）が無いことを保証して新規開始。
    diff.remove_entry(path);
    table.mark_created(path);
    diff.ensure_entry(path, 0);
    diff.set_logical_size(path, 0);
    if let Some(t2) = tier2 {
        t2.journal_op(path, &MetaOp::Create)
            .map_err(|e| EntryError::Journal(e.to_string()))?;
    }
    Ok(())
}

/// エントリを削除する（設計 remove()。tombstone）。存在しなければ
/// [`EntryError::NotFound`]。Tier 1 ページを落とし、Tier 2 索引を purge して
/// create-after-remove で古いページを読まないようにする。`tier2` があれば
/// METADATA REMOVE を journal する。
pub fn entry_remove(
    table: &mut EntryTable,
    diff: &mut DiffLayer,
    vmidx_image: &[u8],
    tier2: Option<&mut Tier2>,
    path: &str,
) -> Result<(), EntryError> {
    let in_vmidx = entry_in_vmidx(vmidx_image, path)?;
    if table.kind(path, in_vmidx) == Kind::Absent {
        return Err(EntryError::NotFound);
    }
    table.mark_tombstone(path);
    diff.remove_entry(path);
    if let Some(t2) = tier2 {
        t2.purge_entry(path);
        t2.journal_op(path, &MetaOp::Remove)
            .map_err(|e| EntryError::Journal(e.to_string()))?;
    }
    Ok(())
}

/// エントリの論理サイズを `new_size` に変える（設計 truncate()）。縮小は末尾
/// ページを落とし（境界ページ末尾はゼロ化、Tier 2 索引も purge）、拡大は gap を
/// ゼロ埋め扱いにする。存在しなければ [`EntryError::NotFound`]。拡大は DATA RECORD
/// が伸びを表さないので常に METADATA RESIZE を journal する（`tier2` があれば）。
pub fn entry_truncate(
    table: &EntryTable,
    diff: &mut DiffLayer,
    vmidx_image: &[u8],
    tier2: Option<&mut Tier2>,
    path: &str,
    new_size: u64,
) -> Result<(), EntryError> {
    let resolved = resolve_entry(table, vmidx_image, path)?;
    diff.ensure_entry(path, resolved.original_size);
    let current = diff.logical_size(path).unwrap_or(resolved.original_size);
    if new_size < current {
        diff.truncate_pages(path, new_size);
    }
    diff.set_logical_size(path, new_size);
    if let Some(t2) = tier2 {
        if new_size < current {
            t2.purge_pages_beyond(path, new_size);
        }
        t2.journal_op(path, &MetaOp::Resize { new_size })
            .map_err(|e| EntryError::Journal(e.to_string()))?;
    }
    Ok(())
}

/// エントリ `old` を `new` へ rename する（設計 rename()）。`old` が存在しなければ
/// [`EntryError::NotFound`]、`new` が既に実効的に存在すれば（同名指定を含む）
/// [`EntryError::Exists`]。データは再圧縮せず、`new` を `old` の究極のソースへの
/// 別名にする（[`EntryTable::apply_rename`]）。ソース ZIP は触らない。圧縮種別が
/// 未対応（Zstd 等）でも rename 自体は通る（verbatim コピーで commit できるため。
/// 読み書きは別名でも従来どおり Unsupported）。`tier2` があれば Diff / 索引を
/// 付け替え、METADATA RENAME を journal する。
pub fn entry_rename(
    table: &mut EntryTable,
    diff: &mut DiffLayer,
    vmidx_image: &[u8],
    tier2: Option<&mut Tier2>,
    old: &str,
    new: &str,
) -> Result<(), EntryError> {
    let old_in_vmidx = entry_in_vmidx(vmidx_image, old)?;
    if table.kind(old, old_in_vmidx) == Kind::Absent {
        return Err(EntryError::NotFound);
    }
    let new_in_vmidx = entry_in_vmidx(vmidx_image, new)?;
    // old == new もここで Exists（kind(new) == kind(old) != Absent）。
    if table.kind(new, new_in_vmidx) != Kind::Absent {
        return Err(EntryError::Exists);
    }
    table.apply_rename(old, new, old_in_vmidx);
    diff.rename_entry(old, new);
    if let Some(t2) = tier2 {
        t2.rename_entry(old, new);
        t2.journal_op(
            old,
            &MetaOp::Rename {
                new_name: new.to_owned(),
            },
        )
        .map_err(|e| EntryError::Journal(e.to_string()))?;
    }
    Ok(())
}

/// `path` が vmidx に在るか（エントリ表の `kind` 判定の入力）。
fn entry_in_vmidx(vmidx_image: &[u8], path: &str) -> Result<bool, EntryError> {
    let vmidx = Vmidx::parse(vmidx_image).map_err(EntryError::Vmidx)?;
    Ok(vmidx.lookup(path).map_err(EntryError::Vmidx)?.is_some())
}

/// dirty 経路（Tier 1 / Tier 2 / ソースの三段）が 1 エントリを触るのに要る文脈。
///
/// [`resolve_entry`] の結果をそのまま載せる想定で、`path` は Diff Layer のキー
/// （＝現在名）、`source` は未変更ページを引くソース名（`None` = created）、
/// `original_size` はそのソースの元サイズ（created は 0）。rename 後は
/// `path != source` になりうる（④b の別名）。
#[derive(Clone, Copy)]
pub struct EntryCtx<'a> {
    /// ソース ZIP 全体のバイト列。
    pub archive: &'a [u8],
    /// `archive` に対応する vmidx 像。
    pub vmidx_image: &'a [u8],
    /// Diff Layer / エントリ表のキー（現在名）。
    pub path: &'a str,
    /// 未変更ページを読むソースエントリ名。`None` = created（ソース無し）。
    pub source: Option<&'a str>,
    /// ソースの元サイズ。Diff に high-water があればそちらが優先される。
    pub original_size: u64,
}

/// エントリ `path` の `[offset, offset + data.len())` を Diff Layer に COW で書く
/// （設計 WRITE PATH）。`tier2` を渡すと書き込み経路は三段になる:
/// - **Tier 1 ヒット**: 常駐ページを in-place 更新。
/// - **Tier 2 ヒット**: vmdirty から読み戻して書き込みを適用し、新しい DATA RECORD を
///   追記する（設計 4.1「Tier 2 ページへの write hit は新レコード追記、Tier 1 へは
///   戻さない」）。
/// - **ミス**: ソース（キャッシュなしの [`read_entry`]）から COW し Tier 1 に載せる。
///
/// 末尾超過はエントリを伸ばす（`logical_size` 更新）。書き終えて `tier2` があり
/// `dirty_limit` を超えていれば最古から spill する。`tier2 = None` は M2 互換の
/// Tier 1 のみ（spill なし）。
pub fn write_into(
    ctx: &EntryCtx<'_>,
    diff: &mut DiffLayer,
    mut tier2: Option<&mut Tier2>,
    offset: u64,
    data: &[u8],
) -> Result<(), WriteError> {
    let EntryCtx { archive, vmidx_image, path, source, original_size } = *ctx;
    if data.is_empty() {
        return Ok(());
    }
    // 存在確認・種別チェックは呼び出し側の [`resolve_entry`] が済ませている。
    // `source` = 未変更ページを読むソース名（None = created）、`original_size` =
    // そのソースの元サイズ（created は 0）。
    let page_size = diff.page_size();
    diff.ensure_entry(path, original_size);
    // ソース読み出し（COW 復元）の上限は source high-water（truncate-shrink で縮む）。
    let original_size = diff.source_size(path).unwrap_or(original_size);
    let end = offset + data.len() as u64;
    let new_logical = diff.logical_size(path).unwrap_or(original_size).max(end);

    let first = offset / page_size;
    let last = (end - 1) / page_size; // data 非空なので end >= 1
    for page in first..=last {
        let page_start = page * page_size;
        if diff.has_page(path, page) {
            // Tier 1 ヒット: 常駐バッファを直接更新。
            let buf = diff.page_mut(path, page).expect("tier1 hit");
            apply_write(buf, page_start, offset, end, data);
        } else if let Some(t2) = tier2.as_deref_mut().filter(|t| t.has(path, page)) {
            // Tier 2 ヒット: 読み戻して適用し、新 DATA RECORD を追記（Tier 1 へは戻さない）。
            let mut buf = t2
                .read_page(path, page)
                .map_err(|e| WriteError::Spill(e.to_string()))?
                .expect("tier2 hit");
            apply_write(&mut buf, page_start, offset, end, data);
            t2.write_hit(path, page, &buf, new_logical)
                .map_err(|e| WriteError::Spill(e.to_string()))?;
        } else {
            // ミス: ソースから COW して Tier 1 に載せる。created（source=None）や
            // ソース末尾超えはゼロ（バッファは既にゼロ初期化）。
            let mut buf = vec![0u8; page_size as usize];
            if let Some(src) = source
                && page_start < original_size
            {
                let avail = ((original_size - page_start).min(page_size)) as usize;
                let orig = read_entry(archive, vmidx_image, src, page_start, avail)
                    .map_err(|e| WriteError::Read(Box::new(e)))?;
                buf[..avail].copy_from_slice(&orig);
            }
            apply_write(&mut buf, page_start, offset, end, data);
            diff.insert_page(path, page, buf);
        }
    }
    diff.set_logical_size(path, new_logical);

    // 書き込み後、上限超過なら最古から Tier 2 へ退避する。
    if let Some(t2) = tier2
        && diff.over_limit()
    {
        t2.spill_over_limit(diff)
            .map_err(|e| WriteError::Spill(e.to_string()))?;
    }
    Ok(())
}

/// `page_size` バイトのページバッファ `buf`（先頭が `page_start`）へ、書き込み範囲
/// `[offset, end)` のうち当該ページに重なる分だけ `data` から適用する。
fn apply_write(buf: &mut [u8], page_start: u64, offset: u64, end: u64, data: &[u8]) {
    let page_end = page_start + buf.len() as u64;
    let w_lo = offset.max(page_start);
    let w_hi = end.min(page_end);
    if w_lo >= w_hi {
        return;
    }
    let dst_lo = (w_lo - page_start) as usize;
    let dst_hi = (w_hi - page_start) as usize;
    let src_lo = (w_lo - offset) as usize;
    let src_hi = (w_hi - offset) as usize;
    buf[dst_lo..dst_hi].copy_from_slice(&data[src_lo..src_hi]);
}

/// dirty なエントリの `[offset, offset + len)` を Diff Layer から読む（設計
/// READ PATH の Tier 1 経路）。ページごとに Diff Layer 優先、無ければソースの
/// 未変更ページ、ソース範囲も超えていればゼロ（implicit extension の gap）。
/// `logical_size` を超える読みは短く返す（EOF セマンティクス）。
pub fn read_dirty(
    ctx: &EntryCtx<'_>,
    diff: &DiffLayer,
    tier2: Option<&Tier2>,
    offset: u64,
    len: usize,
) -> Result<Vec<u8>, ReadError> {
    let EntryCtx { archive, vmidx_image, path, source, original_size } = *ctx;
    // 存在確認は呼び出し側の [`resolve_entry`] が済ませている。`source` = 未変更
    // ページを読むソース名（None = created）。Diff にエントリが無いのは「1 度も
    // 書いていない別名」（純粋な rename）だけで、論理サイズ・source high-water とも
    // 呼び出し側が渡す `original_size`（= ソースの uncompressed_size）に従う。
    // dirty なら Diff の値（truncate-shrink で縮んだ high-water 含む）を優先。
    let logical = diff.logical_size(path).unwrap_or(original_size);
    let original_size = diff.source_size(path).unwrap_or(original_size);
    let page_size = diff.page_size();

    if len == 0 || offset >= logical {
        return Ok(Vec::new());
    }
    let end = (offset + len as u64).min(logical);

    let mut out = Vec::with_capacity((end - offset) as usize);
    let mut pos = offset;
    while pos < end {
        let page = pos / page_size;
        let page_start = page * page_size;
        let chunk_end = end.min(page_start + page_size);
        let take = (chunk_end - pos) as usize;
        let in_page = (pos - page_start) as usize;

        if let Some(p) = diff.page(path, page) {
            // Tier 1 ヒット。
            out.extend_from_slice(&p[in_page..in_page + take]);
        } else if let Some(p) = tier2
            .filter(|t| t.has(path, page))
            .map(|t| t.read_page(path, page))
            .transpose()
            .map_err(|e| ReadError::Tier2(e.to_string()))?
            .flatten()
        {
            // Tier 2 ヒット（vmdirty から読み戻し、page_size までゼロ埋め済み）。
            out.extend_from_slice(&p[in_page..in_page + take]);
        } else {
            // 未変更ページ。ソースがあれば範囲内を読み、超える分とソース無し
            // （created）はゼロ（gap / 末尾超え）。
            let orig_end = chunk_end.min(original_size);
            if let Some(src) = source.filter(|_| pos < orig_end) {
                let n = (orig_end - pos) as usize;
                let chunk = read_entry(archive, vmidx_image, src, pos, n)?;
                out.extend_from_slice(&chunk);
                if n < take {
                    out.resize(out.len() + (take - n), 0);
                }
            } else {
                out.resize(out.len() + take, 0);
            }
        }
        pos = chunk_end;
    }
    Ok(out)
}

/// ページキャッシュ経路（LAYER 2a）が要る二点セット。キャッシュ本体と、
/// read-ahead 幅などの設定は常に一緒に運ばれるのでまとめる。
pub struct PageIo<'a> {
    /// バイト量バウンドの LRU ページキャッシュ。ページ境界も `cache` が決める。
    pub cache: &'a mut PageCache,
    /// ページ設定（`read_ahead_pages` を [`fill_run`] が見る）。
    pub cfg: &'a PageConfig,
}

/// ページキャッシュ経由で 1 エントリの `[offset, offset + len)` を読む。
/// 要求範囲がまたぐ各ページについて、常駐していればキャッシュから、ミスなら
/// [`fill_run`] で目標ページ + read-ahead 分を展開・充填してから取り出す。
/// `cache` の `page_size` がページ境界を決める。
///
/// `on_miss` はキャッシュミスのたびに、ソースアーカイブへ触れる**前**に呼ばれる
/// （設計 SNAPSHOT CONSISTENCY: ミス時の ESTALE チェック点）。`Err` を返すと
/// その時点で読み取りを中断する。ディスク層はここで archive.zip を再 stat し、
/// 変更を検知したら [`ReadError::Stale`] を返す。ファイルを持たないメモリ上の
/// マウントは no-op（`|| Ok(())`）を渡す。
pub fn read_cached<F>(
    archive: &[u8],
    vmidx_image: &[u8],
    io: &mut PageIo<'_>,
    path: &str,
    offset: u64,
    len: usize,
    mut on_miss: F,
) -> Result<Vec<u8>, ReadError>
where
    F: FnMut() -> Result<(), ReadError>,
{
    let vmidx = Vmidx::parse(vmidx_image).map_err(ReadError::Vmidx)?;
    let (entry, record) = vmidx
        .lookup(path)
        .map_err(ReadError::Vmidx)?
        .ok_or(ReadError::NotFound)?;

    // 範囲検査はプロバイダと同じ判定（エントリサイズ超は OutOfRange）。
    check_range(offset, len, record.uncompressed_size).map_err(ReadError::Provider)?;
    if len == 0 {
        return Ok(Vec::new());
    }

    let page_size = io.cache.page_size();
    let total_pages = page_count(record.uncompressed_size, page_size);
    let end = offset + len as u64;

    let mut out = Vec::with_capacity(len);
    let mut pos = offset;
    while pos < end {
        let page = pos / page_size;
        let key = PageKey { entry, page };
        let page_start = page * page_size;
        let in_page = (pos - page_start) as usize;

        if let Some(data) = io.cache.get(key) {
            // ヒット。末尾ページは短く、data.len() が当該ページの実長。
            let take = (data.len() - in_page).min((end - pos) as usize);
            out.extend_from_slice(&data[in_page..in_page + take]);
            pos += take as u64;
            continue;
        }

        // ミス（直前の get が None を返した時点で計上済み）。アーカイブへ触れる
        // 前に鮮度チェック（ESTALE 検出点）。STALE なら以降を読まずに中断する。
        on_miss()?;

        // 目標ページ + read-ahead 分を 1 回で展開し（ラン）、キャッシュへ充填する。
        // 当該ページはランの先頭なので、退避方針に依らず確実に取れる戻り値
        // バイト列から直接切り出す（ラン > キャッシュ容量でも前進できる）。
        let (bytes, run_start) =
            fill_run(archive, &vmidx, io, &record, entry, page, total_pages)?;
        debug_assert_eq!(run_start, page_start);
        let page_len = page_extent(record.uncompressed_size, page, page_size).1;
        let take = (page_len - in_page).min((end - pos) as usize);
        out.extend_from_slice(&bytes[in_page..in_page + take]);
        pos += take as u64;
    }
    Ok(out)
}

/// 目標ページ `target_page` を含む「連続する未常駐ページのラン」を 1 回の
/// `read_range` で展開し、ページに切ってキャッシュへ入れる。戻り値は
/// (ラン展開バイト列, ラン先頭の展開オフセット = `target_page * page_size`)。
/// ランは `[target_page, last]` で、`last` は read-ahead 上限
/// （`cfg.read_ahead_pages`）かエントリ末尾、あるいは最初に常駐していた先読み
/// ページの手前で止まる（設計 READ PATH: read-ahead はエントリ境界で止まり、
/// 常駐ページは再展開しない）。
///
/// キャッシュへの充填と独立に戻り値でランを返すのは、ランがキャッシュ容量を
/// 超えるとき目標ページ自身が充填中に退避されうるため。呼び出し側は戻り値から
/// 目標ページを取り出し、キャッシュは将来のヒット用に充填する。
fn fill_run(
    archive: &[u8],
    vmidx: &Vmidx,
    io: &mut PageIo<'_>,
    record: &EntryRecord,
    entry: usize,
    target_page: u64,
    total_pages: u64,
) -> Result<(Vec<u8>, u64), ReadError> {
    let PageIo { cache, cfg } = io;
    let provider = builtin_provider(record.provider_type)
        .ok_or(ReadError::Unsupported(record.provider_type))?;
    let page_size = cache.page_size();

    // 先読みランの末尾を決める（目標ページ自身は呼び出し側でミス確定）。
    let read_ahead = cfg.read_ahead_pages as u64;
    let last_limit = target_page
        .saturating_add(read_ahead)
        .min(total_pages - 1);
    let mut last = target_page;
    while last < last_limit && !cache.contains(PageKey { entry, page: last + 1 }) {
        last += 1;
    }

    // ラン全域を 1 回で展開する（checkpoint 復元 + 前進デコードを read-ahead 分で
    // 償却）。
    let (run_start, _) = page_extent(record.uncompressed_size, target_page, page_size);
    let (last_start, last_len) = page_extent(record.uncompressed_size, last, page_size);
    let run_len = (last_start - run_start) as usize + last_len;

    let nearest = vmidx
        .nearest_checkpoint(record, run_start)
        .map_err(ReadError::Vmidx)?;
    let start = record.data_offset as usize;
    let aend = start
        .checked_add(record.compressed_size as usize)
        .filter(|&e| e <= archive.len())
        .ok_or(ReadError::DataOutOfRange)?;
    let compressed = &archive[start..aend];

    let bytes = provider
        .read_range(
            compressed,
            nearest.as_ref(),
            run_start,
            run_len,
            record.uncompressed_size,
        )
        .map_err(ReadError::Provider)?;

    // ランをページに切って充填する。
    for page in target_page..=last {
        let (ps, plen) = page_extent(record.uncompressed_size, page, page_size);
        let from = (ps - run_start) as usize;
        cache.insert(PageKey { entry, page }, bytes[from..from + plen].to_vec());
    }
    Ok((bytes, run_start))
}

#[cfg(test)]
mod tests {
    use super::*;
    use libz_rs_sys as z;
    use std::os::raw::c_int;

    /// STORE / DEFLATE 混在の最小 ZIP を手組みするフィクスチャ。
    struct ZipBuilder {
        body: Vec<u8>,
        cd: Vec<u8>,
        count: u16,
    }

    impl ZipBuilder {
        fn new() -> ZipBuilder {
            ZipBuilder {
                body: Vec::new(),
                cd: Vec::new(),
                count: 0,
            }
        }

        /// 1 エントリ追加。`stored` は LFH/CD に書く実データ（STORE なら生、
        /// DEFLATE なら圧縮済み）、`uncompressed_size` は展開後サイズ。
        fn add(&mut self, name: &str, method: u16, stored: &[u8], uncompressed_size: u32) {
            let lho = self.body.len() as u32;
            let nb = name.as_bytes();
            push_u32(&mut self.body, 0x0403_4b50); // LFH
            push_u16(&mut self.body, 20);
            push_u16(&mut self.body, 0);
            push_u16(&mut self.body, method);
            push_u16(&mut self.body, 0);
            push_u16(&mut self.body, 0);
            push_u32(&mut self.body, 0); // crc（読み取り経路では未検証）
            push_u32(&mut self.body, stored.len() as u32);
            push_u32(&mut self.body, uncompressed_size);
            push_u16(&mut self.body, nb.len() as u16);
            push_u16(&mut self.body, 0);
            self.body.extend_from_slice(nb);
            self.body.extend_from_slice(stored);

            push_u32(&mut self.cd, 0x0201_4b50); // CDFH
            push_u16(&mut self.cd, 20);
            push_u16(&mut self.cd, 20);
            push_u16(&mut self.cd, 0);
            push_u16(&mut self.cd, method);
            push_u16(&mut self.cd, 0);
            push_u16(&mut self.cd, 0);
            push_u32(&mut self.cd, 0);
            push_u32(&mut self.cd, stored.len() as u32);
            push_u32(&mut self.cd, uncompressed_size);
            push_u16(&mut self.cd, nb.len() as u16);
            push_u16(&mut self.cd, 0);
            push_u16(&mut self.cd, 0);
            push_u16(&mut self.cd, 0);
            push_u16(&mut self.cd, 0);
            push_u32(&mut self.cd, 0);
            push_u32(&mut self.cd, lho);
            self.cd.extend_from_slice(nb);
            self.count += 1;
        }

        fn finish(mut self) -> Vec<u8> {
            let cd_offset = self.body.len() as u32;
            let cd_size = self.cd.len() as u32;
            self.body.extend_from_slice(&self.cd);
            push_u32(&mut self.body, 0x0605_4b50);
            push_u16(&mut self.body, 0);
            push_u16(&mut self.body, 0);
            push_u16(&mut self.body, self.count);
            push_u16(&mut self.body, self.count);
            push_u32(&mut self.body, cd_size);
            push_u32(&mut self.body, cd_offset);
            push_u16(&mut self.body, 0);
            self.body
        }
    }

    fn push_u16(v: &mut Vec<u8>, x: u16) {
        v.extend_from_slice(&x.to_le_bytes());
    }
    fn push_u32(v: &mut Vec<u8>, x: u32) {
        v.extend_from_slice(&x.to_le_bytes());
    }

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
            assert_eq!(r, z::Z_OK);
            strm.next_in = data.as_ptr();
            strm.avail_in = data.len() as _;
            strm.next_out = out.as_mut_ptr();
            strm.avail_out = out.len() as _;
            assert_eq!(z::deflate(&mut strm, z::Z_FINISH), z::Z_STREAM_END);
            let produced = out.len() - strm.avail_out as usize;
            out.truncate(produced);
            z::deflateEnd(&mut strm);
        }
        out
    }

    fn sample_data(n: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(n);
        let mut state: u32 = 0x9e37_79b9;
        for i in 0..n {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            v.push(((state >> 24) as u8 & 0x1f) ^ (i as u8 & 0x07));
        }
        v
    }

    #[test]
    fn reads_store_and_deflate_entries() {
        let store_data = b"hello, stored world";
        let deflate_data = sample_data(180_000);
        let comp = raw_deflate(&deflate_data);

        let mut zb = ZipBuilder::new();
        zb.add("notes.txt", 0, store_data, store_data.len() as u32);
        zb.add("big.bin", 8, &comp, deflate_data.len() as u32);
        let zip = zb.finish();

        let params = BuildParams {
            checkpoint_interval: 16 * 1024,
            ..BuildParams::default()
        };
        let mount = Mount::open(&zip, &params).expect("open");

        // STORE: 全域・部分域。
        assert_eq!(
            mount.read("notes.txt", 0, store_data.len()).unwrap(),
            store_data
        );
        assert_eq!(mount.read("notes.txt", 7, 6).unwrap(), b"stored");

        // DEFLATE: 先頭・深いオフセット（チェックポイント経由のシーク）。
        assert_eq!(mount.read("big.bin", 0, 100).unwrap(), &deflate_data[..100]);
        for &off in &[10_000u64, 90_000, 179_000] {
            let len = ((deflate_data.len() as u64 - off).min(777)) as usize;
            let got = mount.read("big.bin", off, len).unwrap();
            assert_eq!(
                got,
                &deflate_data[off as usize..off as usize + len],
                "deflate mismatch at {off}"
            );
        }
    }

    #[test]
    fn deflate_entry_has_checkpoints_in_index() {
        // EAGER 索引が DEFLATE エントリにチェックポイントを生成していることを、
        // ビューから直接確かめる（深いオフセットの読みが先頭からでなく
        // チェックポイントから復元される根拠）。
        let data = sample_data(180_000);
        let comp = raw_deflate(&data);
        let mut zb = ZipBuilder::new();
        zb.add("big.bin", 8, &comp, data.len() as u32);
        let zip = zb.finish();
        let params = BuildParams {
            checkpoint_interval: 16 * 1024,
            ..BuildParams::default()
        };
        let mount = Mount::open(&zip, &params).expect("open");
        let vmidx = Vmidx::parse(mount.index_bytes()).unwrap();
        let (_, rec) = vmidx.lookup("big.bin").unwrap().unwrap();
        assert!(rec.checkpoint_count > 0, "expected eager checkpoints");
        // target=90000 で uncompressed_offset>0 の最近 CP が引けること。
        let cp = vmidx.nearest_checkpoint(&rec, 90_000).unwrap();
        assert!(cp.is_some_and(|c| c.uncompressed_offset() > 0));
    }

    #[test]
    fn missing_entry_is_not_found() {
        let mut zb = ZipBuilder::new();
        zb.add("a.txt", 0, b"x", 1);
        let zip = zb.finish();
        let mount = Mount::open(&zip, &BuildParams::default()).expect("open");
        assert_eq!(mount.read("absent", 0, 1), Err(ReadError::NotFound));
    }

    #[test]
    fn open_with_valid_index_reuses_it() {
        let mut zb = ZipBuilder::new();
        zb.add("a.txt", 0, b"abcdef", 6);
        let zip = zb.finish();
        let params = BuildParams {
            source_file_size: 4096,
            source_inode: 5,
            source_mtime_ns: 9,
            ..BuildParams::default()
        };
        // 先に EAGER で作った像を、検証つきで開き直す → 再構築せず使える。
        let image = Mount::open(&zip, &params).unwrap().index_bytes().to_vec();
        let mount = Mount::open_with_index(&zip, image.clone(), &params).expect("open_with_index");
        assert_eq!(mount.read("a.txt", 1, 4).unwrap(), b"bcde");
        // 像はそのまま使われている（再構築なら別バイト列になりうる）。
        assert_eq!(mount.index_bytes(), image.as_slice());
    }

    #[test]
    fn open_with_stale_index_rebuilds() {
        let mut zb = ZipBuilder::new();
        zb.add("a.txt", 0, b"abcdef", 6);
        let zip = zb.finish();
        let params = BuildParams::default();
        // 別アーカイブから作った（cd_hash 不一致＝Invalid な）像を渡す。
        let mut other = ZipBuilder::new();
        other.add("zzz.bin", 0, b"different", 9);
        let other_zip = other.finish();
        let stale = Mount::open(&other_zip, &params).unwrap().index_bytes().to_vec();

        let mount = Mount::open_with_index(&zip, stale, &params).expect("rebuild");
        // 正しいアーカイブの内容が読める＝再構築された。
        assert_eq!(mount.read("a.txt", 0, 6).unwrap(), b"abcdef");
        assert_eq!(mount.read("zzz.bin", 0, 1), Err(ReadError::NotFound));
    }

    /// `big.bin` 1 エントリ（DEFLATE）だけの ZIP を組む共通フィクスチャ。
    fn deflate_zip(data: &[u8]) -> Vec<u8> {
        let comp = raw_deflate(data);
        let mut zb = ZipBuilder::new();
        zb.add("big.bin", 8, &comp, data.len() as u32);
        zb.finish()
    }

    #[test]
    fn read_ahead_populates_neighbor_pages() {
        let data = sample_data(180_000);
        let cfg = PageConfig {
            page_size: 4096,
            read_ahead_pages: 8,
            cache_bytes: 16 << 20,
        };
        let zip = deflate_zip(&data);
        let params = BuildParams {
            checkpoint_interval: 16 * 1024,
            ..BuildParams::default()
        };
        let mount = Mount::open_with_page_config(&zip, &params, cfg).expect("open");

        // ページ 0 の途中を 1 バイト読むだけで、ページ 0..=8 が充填される。
        let got = mount.read("big.bin", 100, 1).unwrap();
        assert_eq!(got, &data[100..101]);
        {
            let cache = mount.cache.borrow();
            assert_eq!(cache.len(), 9, "target + 8 read-ahead pages");
            for p in 0..=8u64 {
                assert!(cache.contains(PageKey { entry: 0, page: p }), "page {p}");
            }
            assert!(!cache.contains(PageKey { entry: 0, page: 9 }));
            assert_eq!(cache.misses(), 1, "single miss drove the whole run");
        }

        // 先読み済みページ（ページ 5）の読みはミスを増やさない（ヒット）。
        let off = 5 * 4096 + 17;
        let got = mount.read("big.bin", off as u64, 50).unwrap();
        assert_eq!(got, &data[off..off + 50]);
        assert_eq!(mount.cache.borrow().misses(), 1, "served from cache");
    }

    #[test]
    fn read_ahead_disabled_loads_only_target_page() {
        let data = sample_data(50_000);
        let cfg = PageConfig {
            page_size: 4096,
            read_ahead_pages: 0,
            cache_bytes: 16 << 20,
        };
        let zip = deflate_zip(&data);
        let params = BuildParams {
            checkpoint_interval: 16 * 1024,
            ..BuildParams::default()
        };
        let mount = Mount::open_with_page_config(&zip, &params, cfg).expect("open");
        mount.read("big.bin", 0, 1).unwrap();
        assert_eq!(mount.cache.borrow().len(), 1);
    }

    #[test]
    fn multi_page_read_spans_pages_and_short_tail() {
        // 末尾が短いページになるサイズ（4096*N にしない）。
        let data = sample_data(10_000); // 4096,4096,1808 の 3 ページ
        let cfg = PageConfig {
            page_size: 4096,
            read_ahead_pages: 8,
            cache_bytes: 16 << 20,
        };
        let zip = deflate_zip(&data);
        let params = BuildParams::default();
        let mount = Mount::open_with_page_config(&zip, &params, cfg).expect("open");

        // 3 ページ全域を 1 回で（複数ページ跨ぎ + 短い末尾）。
        assert_eq!(mount.read("big.bin", 0, data.len()).unwrap(), data);
        // ページ境界をまたぐ部分読み。
        assert_eq!(
            mount.read("big.bin", 4090, 20).unwrap(),
            &data[4090..4110]
        );
        // 末尾ぴったり（短い末尾ページ）。
        assert_eq!(
            mount.read("big.bin", 9_990, 10).unwrap(),
            &data[9_990..10_000]
        );
        // 末尾超過は OutOfRange。
        assert!(matches!(
            mount.read("big.bin", 9_990, 20),
            Err(ReadError::Provider(ProviderError::OutOfRange { .. }))
        ));
    }

    #[test]
    fn small_cache_evicts_but_stays_correct() {
        let data = sample_data(120_000);
        // ページ 2 枚分しか持てないキャッシュ。read-ahead 分は退避される。
        let cfg = PageConfig {
            page_size: 4096,
            read_ahead_pages: 8,
            cache_bytes: 2 * 4096,
        };
        let zip = deflate_zip(&data);
        let params = BuildParams {
            checkpoint_interval: 16 * 1024,
            ..BuildParams::default()
        };
        let mount = Mount::open_with_page_config(&zip, &params, cfg).expect("open");

        // あちこち読んでも常に原データと一致し、常駐は 2 ページに収まる。
        for &off in &[0u64, 70_000, 5_000, 119_000, 40_000, 100] {
            let len = ((data.len() as u64 - off).min(900)) as usize;
            let got = mount.read("big.bin", off, len).unwrap();
            assert_eq!(got, &data[off as usize..off as usize + len], "at {off}");
            assert!(mount.cache.borrow().len() <= 2, "cache bounded");
        }
    }

    #[test]
    fn store_entry_reads_through_cache() {
        let store = b"hello, stored world, with several pages worth of bytes here.";
        let mut zb = ZipBuilder::new();
        zb.add("notes.txt", 0, store, store.len() as u32);
        let zip = zb.finish();
        let cfg = PageConfig {
            page_size: 16,
            read_ahead_pages: 2,
            cache_bytes: 16 << 20,
        };
        let mount = Mount::open_with_page_config(&zip, &BuildParams::default(), cfg).expect("open");
        assert_eq!(mount.read("notes.txt", 0, store.len()).unwrap(), store);
        assert_eq!(mount.read("notes.txt", 7, 6).unwrap(), b"stored");
        // STORE もページキャッシュ経由（隣接ページが充填される）。
        assert!(mount.cache.borrow().len() >= 2);
    }

    // ───────────────────────── M2: 書き込み + FULL commit ─────────────────────

    #[test]
    fn write_to_missing_entry_is_not_found() {
        let mut zb = ZipBuilder::new();
        zb.add("a.txt", 0, b"x", 1);
        let zip = zb.finish();
        let mount = Mount::open(&zip, &BuildParams::default()).expect("open");
        assert_eq!(mount.write("absent", 0, b"y"), Err(WriteError::NotFound));
    }

    #[test]
    fn empty_write_is_noop() {
        let mut zb = ZipBuilder::new();
        zb.add("a.txt", 0, b"hello", 5);
        let zip = zb.finish();
        let mount = Mount::open(&zip, &BuildParams::default()).expect("open");
        mount.write("a.txt", 2, b"").unwrap();
        assert!(!mount.is_dirty(), "zero-length write must not dirty the entry");
    }

    #[test]
    fn write_then_read_and_commit_store_entry() {
        let mut zb = ZipBuilder::new();
        zb.add("a.txt", 0, b"hello world", 11);
        zb.add("b.txt", 0, b"unchanged", 9);
        let zip = zb.finish();
        let params = BuildParams::default();
        let mount = Mount::open(&zip, &params).expect("open");

        // "hello world" の 6..11 ("world") を上書きする。
        mount.write("a.txt", 6, b"rust!").unwrap();
        assert!(mount.is_dirty());
        assert_eq!(mount.read("a.txt", 0, 11).unwrap(), b"hello rust!");
        // 部分読み（dirty ページから）。
        assert_eq!(mount.read("a.txt", 6, 5).unwrap(), b"rust!");
        // 未変更エントリは元のまま。
        assert_eq!(mount.read("b.txt", 0, 9).unwrap(), b"unchanged");

        // FULL commit → 新 ZIP を開き直して反映を確認。
        let new_zip = mount.commit_full().unwrap();
        let m2 = Mount::open(&new_zip, &params).expect("reopen committed");
        assert_eq!(m2.read("a.txt", 0, 11).unwrap(), b"hello rust!");
        assert_eq!(m2.read("b.txt", 0, 9).unwrap(), b"unchanged");
    }

    #[test]
    fn write_past_end_extends_with_zero_fill() {
        let mut zb = ZipBuilder::new();
        zb.add("a.txt", 0, b"abc", 3);
        let zip = zb.finish();
        let params = BuildParams::default();
        let mount = Mount::open(&zip, &params).expect("open");

        // offset 5 に書く: [3,5) は gap（ゼロ）、論理サイズは 7 に伸びる。
        mount.write("a.txt", 5, b"XY").unwrap();
        let expect = b"abc\x00\x00XY";
        assert_eq!(mount.read("a.txt", 0, 7).unwrap(), expect);
        // EOF セマンティクス: 末尾跨ぎは短く、末尾以降は空。
        assert_eq!(mount.read("a.txt", 6, 10).unwrap(), b"Y");
        assert_eq!(mount.read("a.txt", 7, 5).unwrap(), b"");

        let new_zip = mount.commit_full().unwrap();
        let m2 = Mount::open(&new_zip, &params).expect("reopen committed");
        assert_eq!(m2.read("a.txt", 0, 7).unwrap(), expect);
    }

    #[test]
    fn write_spanning_two_pages() {
        let store = vec![b'.'; 20];
        let mut zb = ZipBuilder::new();
        zb.add("a.txt", 0, &store, store.len() as u32);
        let zip = zb.finish();
        // ページ 8 バイト: 書き込み [6,12) はページ 0 と 1 を跨ぐ。
        let cfg = PageConfig {
            page_size: 8,
            read_ahead_pages: 0,
            cache_bytes: 16 << 20,
        };
        let params = BuildParams::default();
        let mount = Mount::open_with_page_config(&zip, &params, cfg).expect("open");
        mount.write("a.txt", 6, b"ABCDEF").unwrap();
        let mut expect = store.clone();
        expect[6..12].copy_from_slice(b"ABCDEF");
        assert_eq!(mount.read("a.txt", 0, 20).unwrap(), expect);

        let new_zip = mount.commit_full().unwrap();
        let m2 = Mount::open_with_page_config(&new_zip, &params, cfg).expect("reopen");
        assert_eq!(m2.read("a.txt", 0, 20).unwrap(), expect);
    }

    #[test]
    fn write_and_commit_deflate_entry() {
        let data = sample_data(180_000);
        let comp = raw_deflate(&data);
        let mut zb = ZipBuilder::new();
        zb.add("big.bin", 8, &comp, data.len() as u32);
        zb.add("note.txt", 0, b"sidecar", 7);
        let zip = zb.finish();
        let params = BuildParams {
            checkpoint_interval: 16 * 1024,
            ..BuildParams::default()
        };
        let mount = Mount::open(&zip, &params).expect("open");

        // 深い位置を 100 バイト上書きする（COW は当該ページのみ元から読む）。
        let patch = vec![0xABu8; 100];
        mount.write("big.bin", 90_000, &patch).unwrap();
        assert_eq!(mount.read("big.bin", 90_000, 100).unwrap(), patch);
        // 同じ dirty ページ内の前後は元データのまま。
        assert_eq!(
            mount.read("big.bin", 89_900, 100).unwrap(),
            &data[89_900..90_000]
        );
        // 別ページ（未変更）はソースから読める。
        assert_eq!(mount.read("big.bin", 0, 100).unwrap(), &data[..100]);
        assert_eq!(
            mount.read("big.bin", 179_000, 500).unwrap(),
            &data[179_000..179_500]
        );

        // FULL commit: big.bin は再圧縮、note.txt は verbatim コピー。
        let new_zip = mount.commit_full().unwrap();
        let m2 = Mount::open(&new_zip, &params).expect("reopen committed");
        let mut expect = data.clone();
        expect[90_000..90_100].copy_from_slice(&patch);
        assert_eq!(m2.read("big.bin", 0, expect.len()).unwrap(), expect);
        assert_eq!(m2.read("note.txt", 0, 7).unwrap(), b"sidecar");
        // 再圧縮後の深いシークも通る（新索引のチェックポイント経由）。
        assert_eq!(
            m2.read("big.bin", 120_000, 200).unwrap(),
            &expect[120_000..120_200]
        );
    }

    #[test]
    fn commit_without_writes_round_trips() {
        let data = sample_data(50_000);
        let comp = raw_deflate(&data);
        let mut zb = ZipBuilder::new();
        zb.add("a.txt", 0, b"hello", 5);
        zb.add("big.bin", 8, &comp, data.len() as u32);
        let zip = zb.finish();
        let params = BuildParams::default();
        let mount = Mount::open(&zip, &params).expect("open");
        assert!(!mount.is_dirty());

        let new_zip = mount.commit_full().unwrap();
        let m2 = Mount::open(&new_zip, &params).expect("reopen committed");
        assert_eq!(m2.read("a.txt", 0, 5).unwrap(), b"hello");
        assert_eq!(m2.read("big.bin", 0, data.len()).unwrap(), data);
    }

    // ───────────────────────── ④ エントリ操作（create/remove/truncate）─────────

    #[test]
    fn create_write_read_and_commit_new_entry() {
        let mut zb = ZipBuilder::new();
        zb.add("a.txt", 0, b"existing", 8);
        let zip = zb.finish();
        let params = BuildParams::default();
        let mount = Mount::open(&zip, &params).expect("open");

        // 新規エントリは create 前は存在しない。
        assert_eq!(mount.read("new.txt", 0, 1), Err(ReadError::NotFound));
        mount.create("new.txt").unwrap();
        assert!(mount.is_dirty());
        // 作りたては空（読みは短い）。
        assert_eq!(mount.read("new.txt", 0, 10).unwrap(), b"");
        // 書いて読み戻す。implicit extension で gap はゼロ。
        mount.write("new.txt", 2, b"hi").unwrap();
        assert_eq!(mount.read("new.txt", 0, 4).unwrap(), b"\x00\x00hi");

        // commit 後に開き直すと新エントリが在り、既存も保たれる。
        let new_zip = mount.commit_full().unwrap();
        let m2 = Mount::open(&new_zip, &params).expect("reopen");
        assert_eq!(m2.read("new.txt", 0, 4).unwrap(), b"\x00\x00hi");
        assert_eq!(m2.read("a.txt", 0, 8).unwrap(), b"existing");
    }

    #[test]
    fn create_existing_entry_fails() {
        let mut zb = ZipBuilder::new();
        zb.add("a.txt", 0, b"x", 1);
        let zip = zb.finish();
        let mount = Mount::open(&zip, &BuildParams::default()).expect("open");
        assert_eq!(mount.create("a.txt"), Err(EntryError::Exists));
        // 二重 create も Exists。
        mount.create("b.txt").unwrap();
        assert_eq!(mount.create("b.txt"), Err(EntryError::Exists));
    }

    #[test]
    fn remove_hides_entry_and_commit_drops_it() {
        let mut zb = ZipBuilder::new();
        zb.add("a.txt", 0, b"keep", 4);
        zb.add("b.txt", 0, b"gone", 4);
        let zip = zb.finish();
        let params = BuildParams::default();
        let mount = Mount::open(&zip, &params).expect("open");

        mount.remove("b.txt").unwrap();
        assert_eq!(mount.read("b.txt", 0, 4), Err(ReadError::NotFound));
        assert_eq!(mount.write("b.txt", 0, b"x"), Err(WriteError::NotFound));
        // 存在しないものの remove は ENOENT。
        assert_eq!(mount.remove("absent"), Err(EntryError::NotFound));

        let new_zip = mount.commit_full().unwrap();
        let m2 = Mount::open(&new_zip, &params).expect("reopen");
        assert_eq!(m2.read("a.txt", 0, 4).unwrap(), b"keep");
        assert_eq!(m2.read("b.txt", 0, 1), Err(ReadError::NotFound));
    }

    #[test]
    fn remove_then_write_then_remove_clean_entry() {
        // dirty にしてから remove しても tombstone が勝つ。
        let mut zb = ZipBuilder::new();
        zb.add("a.txt", 0, b"hello", 5);
        let zip = zb.finish();
        let params = BuildParams::default();
        let mount = Mount::open(&zip, &params).expect("open");
        mount.write("a.txt", 0, b"HELLO").unwrap();
        mount.remove("a.txt").unwrap();
        assert_eq!(mount.read("a.txt", 0, 5), Err(ReadError::NotFound));
        let new_zip = mount.commit_full().unwrap();
        let m2 = Mount::open(&new_zip, &params).expect("reopen");
        assert_eq!(m2.read("a.txt", 0, 1), Err(ReadError::NotFound));
    }

    #[test]
    fn create_after_remove_restarts_fresh() {
        let mut zb = ZipBuilder::new();
        zb.add("a.txt", 0, b"original", 8);
        let zip = zb.finish();
        let params = BuildParams::default();
        let mount = Mount::open(&zip, &params).expect("open");

        mount.remove("a.txt").unwrap();
        // remove 後の create は新規の空エントリ（ソースの "original" は見えない）。
        mount.create("a.txt").unwrap();
        assert_eq!(mount.read("a.txt", 0, 8).unwrap(), b"");
        mount.write("a.txt", 0, b"fresh").unwrap();
        assert_eq!(mount.read("a.txt", 0, 8).unwrap(), b"fresh");

        let new_zip = mount.commit_full().unwrap();
        let m2 = Mount::open(&new_zip, &params).expect("reopen");
        assert_eq!(m2.read("a.txt", 0, 5).unwrap(), b"fresh");
    }

    #[test]
    fn truncate_shrink_and_extend() {
        let mut zb = ZipBuilder::new();
        zb.add("a.txt", 0, b"0123456789", 10);
        let zip = zb.finish();
        let params = BuildParams::default();
        let mount = Mount::open(&zip, &params).expect("open");

        // 縮小: 4 バイトへ。末尾以降は EOF。
        mount.truncate("a.txt", 4).unwrap();
        assert_eq!(mount.read("a.txt", 0, 10).unwrap(), b"0123");

        // 拡大: 7 バイトへ。伸びた gap はゼロ。
        mount.truncate("a.txt", 7).unwrap();
        assert_eq!(mount.read("a.txt", 0, 10).unwrap(), b"0123\x00\x00\x00");

        // 存在しないものは ENOENT。
        assert_eq!(mount.truncate("absent", 0), Err(EntryError::NotFound));

        let new_zip = mount.commit_full().unwrap();
        let m2 = Mount::open(&new_zip, &params).expect("reopen");
        // commit 後は size 7（clean エントリは範囲超え read が OutOfRange）。
        assert_eq!(m2.read("a.txt", 0, 7).unwrap(), b"0123\x00\x00\x00");
    }

    #[test]
    fn truncate_shrink_then_reextend_reads_zero_not_stale() {
        // 縮小後に再拡大したとき、落とした末尾が蘇らずゼロで読めること。
        let store = vec![b'Z'; 20];
        let mut zb = ZipBuilder::new();
        zb.add("a.txt", 0, &store, 20);
        let zip = zb.finish();
        // ページ 8 バイト。
        let cfg = PageConfig {
            page_size: 8,
            read_ahead_pages: 0,
            cache_bytes: 16 << 20,
        };
        let params = BuildParams::default();
        let mount = Mount::open_with_page_config(&zip, &params, cfg).expect("open");
        // まず dirty にして全域ページを Tier 1 に載せる。
        mount.write("a.txt", 0, &[b'Z'; 20]).unwrap();
        mount.truncate("a.txt", 3).unwrap();
        mount.truncate("a.txt", 20).unwrap();
        let expect: Vec<u8> = b"ZZZ".iter().copied().chain(std::iter::repeat_n(0, 17)).collect();
        assert_eq!(mount.read("a.txt", 0, 20).unwrap(), expect);
    }

    #[test]
    fn multiple_writes_to_same_entry_accumulate() {
        let mut zb = ZipBuilder::new();
        zb.add("a.txt", 0, b"0123456789", 10);
        let zip = zb.finish();
        let params = BuildParams::default();
        let mount = Mount::open(&zip, &params).expect("open");
        mount.write("a.txt", 0, b"AB").unwrap();
        mount.write("a.txt", 8, b"YZ").unwrap();
        assert_eq!(mount.read("a.txt", 0, 10).unwrap(), b"AB234567YZ");
        let new_zip = mount.commit_full().unwrap();
        let m2 = Mount::open(&new_zip, &params).expect("reopen");
        assert_eq!(m2.read("a.txt", 0, 10).unwrap(), b"AB234567YZ");
    }

    #[test]
    fn rename_unchanged_store_entry_then_commit() {
        let mut zb = ZipBuilder::new();
        zb.add("a.txt", 0, b"payload!!", 9);
        let zip = zb.finish();
        let params = BuildParams::default();
        let mount = Mount::open(&zip, &params).expect("open");

        mount.rename("a.txt", "b.txt").unwrap();
        // 旧名は消え、新名で元データが読める（再圧縮なし）。
        assert_eq!(mount.read("a.txt", 0, 9), Err(ReadError::NotFound));
        assert_eq!(mount.read("b.txt", 0, 9).unwrap(), b"payload!!");

        let new_zip = mount.commit_full().unwrap();
        let m2 = Mount::open(&new_zip, &params).expect("reopen");
        assert_eq!(m2.read("b.txt", 0, 9).unwrap(), b"payload!!");
        assert_eq!(m2.read("a.txt", 0, 1), Err(ReadError::NotFound));
    }

    #[test]
    fn rename_unchanged_deflate_entry_commits_verbatim() {
        let data = sample_data(50_000);
        let mut zb = ZipBuilder::new();
        zb.add("d.bin", 8, &raw_deflate(&data), data.len() as u32);
        let zip = zb.finish();
        let params = BuildParams::default();
        let mount = Mount::open(&zip, &params).expect("open");

        mount.rename("d.bin", "moved.bin").unwrap();
        assert_eq!(mount.read("moved.bin", 0, 100).unwrap(), &data[..100]);

        let new_zip = mount.commit_full().unwrap();
        let m2 = Mount::open(&new_zip, &params).expect("reopen");
        assert_eq!(m2.read("moved.bin", 1000, 200).unwrap(), &data[1000..1200]);
        assert_eq!(m2.read("d.bin", 0, 1), Err(ReadError::NotFound));
    }

    #[test]
    fn rename_then_write_recompresses_under_new_name() {
        let mut zb = ZipBuilder::new();
        zb.add("a.txt", 0, b"0123456789", 10);
        let zip = zb.finish();
        let params = BuildParams::default();
        let mount = Mount::open(&zip, &params).expect("open");

        mount.rename("a.txt", "b.txt").unwrap();
        mount.write("b.txt", 0, b"AB").unwrap();
        // COW: 書いた所だけ変わり、残りはソース a.txt の元データから引く。
        assert_eq!(mount.read("b.txt", 0, 10).unwrap(), b"AB23456789");

        let new_zip = mount.commit_full().unwrap();
        let m2 = Mount::open(&new_zip, &params).expect("reopen");
        assert_eq!(m2.read("b.txt", 0, 10).unwrap(), b"AB23456789");
    }

    #[test]
    fn rename_chain_folds_to_original_source() {
        let mut zb = ZipBuilder::new();
        zb.add("a.txt", 0, b"chained", 7);
        let zip = zb.finish();
        let params = BuildParams::default();
        let mount = Mount::open(&zip, &params).expect("open");

        mount.rename("a.txt", "b.txt").unwrap();
        mount.rename("b.txt", "c.txt").unwrap();
        assert_eq!(mount.read("b.txt", 0, 7), Err(ReadError::NotFound));
        assert_eq!(mount.read("c.txt", 0, 7).unwrap(), b"chained");

        let new_zip = mount.commit_full().unwrap();
        let m2 = Mount::open(&new_zip, &params).expect("reopen");
        assert_eq!(m2.read("c.txt", 0, 7).unwrap(), b"chained");
        assert_eq!(m2.read("a.txt", 0, 1), Err(ReadError::NotFound));
    }

    #[test]
    fn rename_errors_on_missing_and_existing() {
        let mut zb = ZipBuilder::new();
        zb.add("a.txt", 0, b"a", 1);
        zb.add("b.txt", 0, b"b", 1);
        let zip = zb.finish();
        let mount = Mount::open(&zip, &BuildParams::default()).expect("open");

        assert_eq!(mount.rename("ghost", "x"), Err(EntryError::NotFound));
        assert_eq!(mount.rename("a.txt", "b.txt"), Err(EntryError::Exists));
        // 同名指定も Exists。
        assert_eq!(mount.rename("a.txt", "a.txt"), Err(EntryError::Exists));
    }

    #[test]
    fn rename_onto_removed_name_reuses_target() {
        let mut zb = ZipBuilder::new();
        zb.add("a.txt", 0, b"AAA", 3);
        zb.add("b.txt", 0, b"BBB", 3);
        let zip = zb.finish();
        let params = BuildParams::default();
        let mount = Mount::open(&zip, &params).expect("open");

        mount.remove("b.txt").unwrap();
        mount.rename("a.txt", "b.txt").unwrap();
        // b.txt は今や a.txt のソースを指す（元の "BBB" ではない）。
        assert_eq!(mount.read("b.txt", 0, 3).unwrap(), b"AAA");

        let new_zip = mount.commit_full().unwrap();
        let m2 = Mount::open(&new_zip, &params).expect("reopen");
        assert_eq!(m2.read("b.txt", 0, 3).unwrap(), b"AAA");
        assert_eq!(m2.read("a.txt", 0, 1), Err(ReadError::NotFound));
    }

    #[test]
    fn rename_created_entry_stays_created() {
        let mut zb = ZipBuilder::new();
        zb.add("keep.txt", 0, b"keep", 4);
        let zip = zb.finish();
        let params = BuildParams::default();
        let mount = Mount::open(&zip, &params).expect("open");

        mount.create("x.txt").unwrap();
        mount.write("x.txt", 0, b"made").unwrap();
        mount.rename("x.txt", "y.txt").unwrap();
        assert_eq!(mount.read("x.txt", 0, 4), Err(ReadError::NotFound));
        assert_eq!(mount.read("y.txt", 0, 4).unwrap(), b"made");

        let new_zip = mount.commit_full().unwrap();
        let m2 = Mount::open(&new_zip, &params).expect("reopen");
        assert_eq!(m2.read("y.txt", 0, 4).unwrap(), b"made");
        assert_eq!(m2.read("keep.txt", 0, 4).unwrap(), b"keep");
    }

    #[test]
    fn rename_recreate_old_name_keeps_both() {
        // rename a→b の後 a を作り直すと、a（新規）と b（元 a のソース）が併存。
        let mut zb = ZipBuilder::new();
        zb.add("a.txt", 0, b"ORIGINAL", 8);
        let zip = zb.finish();
        let params = BuildParams::default();
        let mount = Mount::open(&zip, &params).expect("open");

        mount.rename("a.txt", "b.txt").unwrap();
        mount.create("a.txt").unwrap();
        mount.write("a.txt", 0, b"new").unwrap();
        assert_eq!(mount.read("a.txt", 0, 8).unwrap(), b"new");
        assert_eq!(mount.read("b.txt", 0, 8).unwrap(), b"ORIGINAL");

        let new_zip = mount.commit_full().unwrap();
        let m2 = Mount::open(&new_zip, &params).expect("reopen");
        assert_eq!(m2.read("a.txt", 0, 3).unwrap(), b"new");
        assert_eq!(m2.read("b.txt", 0, 8).unwrap(), b"ORIGINAL");
    }

    fn lho_of(zip: &[u8], name: &[u8]) -> u64 {
        Archive::parse(zip)
            .unwrap()
            .entries()
            .iter()
            .find(|e| e.name == name)
            .unwrap()
            .local_header_offset
    }

    #[test]
    fn incremental_commit_appends_changed_keeps_unchanged() {
        let mut zb = ZipBuilder::new();
        zb.add("keep.txt", 0, b"unchanged!", 10);
        zb.add("edit.txt", 0, b"0123456789", 10);
        let zip = zb.finish();
        let params = BuildParams::default();
        let mount = Mount::open(&zip, &params).expect("open");
        let keep_lho_before = lho_of(&zip, b"keep.txt");

        mount.write("edit.txt", 0, b"AB").unwrap();
        let grown = mount.commit_incremental().unwrap();

        // append-only: 既存バイトはそのまま prefix に残る。
        assert!(grown.len() > zip.len());
        assert_eq!(&grown[..zip.len()], &zip[..]);

        // 開き直して両方読める。
        let m2 = Mount::open(&grown, &params).expect("reopen");
        assert_eq!(m2.read("keep.txt", 0, 10).unwrap(), b"unchanged!");
        assert_eq!(m2.read("edit.txt", 0, 10).unwrap(), b"AB23456789");

        // 未変更 keep.txt は元位置のまま、変更 edit.txt は追記領域（元長以降）へ。
        assert_eq!(lho_of(&grown, b"keep.txt"), keep_lho_before);
        assert!(lho_of(&grown, b"edit.txt") >= zip.len() as u64);
    }

    #[test]
    fn incremental_commit_handles_create_remove_rename() {
        let mut zb = ZipBuilder::new();
        zb.add("a.txt", 0, b"AAAA", 4);
        zb.add("b.txt", 0, b"BBBB", 4);
        zb.add("c.txt", 0, b"CCCC", 4);
        let zip = zb.finish();
        let params = BuildParams::default();
        let mount = Mount::open(&zip, &params).expect("open");

        mount.create("new.txt").unwrap();
        mount.write("new.txt", 0, b"NEW").unwrap();
        mount.remove("b.txt").unwrap();
        mount.rename("c.txt", "d.txt").unwrap();
        let grown = mount.commit_incremental().unwrap();

        assert_eq!(&grown[..zip.len()], &zip[..]); // prefix 不変。
        let m2 = Mount::open(&grown, &params).expect("reopen");
        assert_eq!(m2.read("a.txt", 0, 4).unwrap(), b"AAAA"); // 未変更。
        assert_eq!(m2.read("new.txt", 0, 3).unwrap(), b"NEW"); // created。
        assert_eq!(m2.read("b.txt", 0, 1), Err(ReadError::NotFound)); // removed。
        assert_eq!(m2.read("d.txt", 0, 4).unwrap(), b"CCCC"); // renamed。
        assert_eq!(m2.read("c.txt", 0, 1), Err(ReadError::NotFound));
        assert_eq!(lho_of(&grown, b"a.txt"), lho_of(&zip, b"a.txt")); // 未変更は元位置。
    }

    #[test]
    fn incremental_commit_without_changes_keeps_entries_readable() {
        // 変更ゼロでも壊れた追記をしない（新 CD は全 live を元位置で指す）。
        let mut zb = ZipBuilder::new();
        zb.add("x.txt", 0, b"hello", 5);
        let zip = zb.finish();
        let params = BuildParams::default();
        let mount = Mount::open(&zip, &params).expect("open");
        let grown = mount.commit_incremental().unwrap();
        let m2 = Mount::open(&grown, &params).expect("reopen");
        assert_eq!(m2.read("x.txt", 0, 5).unwrap(), b"hello");
        assert_eq!(lho_of(&grown, b"x.txt"), lho_of(&zip, b"x.txt"));
    }
}
