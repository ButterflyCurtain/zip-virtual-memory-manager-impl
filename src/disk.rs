//! ディスク上の `archive.zip` を mmap し、サイドカー `archive.zip.vmm/vmidx` を
//! 読み書きする薄いファイル I/O 層（設計 SIDECAR FILES / FIRST-OPEN）。
//!
//! [`FileMount`] は実ファイルを read-only で mmap し、その `&[u8]` を
//! [`mount`](crate::mount) のロジック（`resolve_index` / `read_cached`）に渡す。
//! 設計方針「mmap は外から渡す」に沿い、mmap の所有はこの層に閉じ、`mount` /
//! `vmidx` / `archive` はバイトスライスだけを見る。読み取りのたびに（ミス時に）
//! archive.zip を再 stat して外部変更を検出する（ESTALE、設計 SNAPSHOT
//! CONSISTENCY）。
//!
//! open() の流れ:
//! 1. `archive.zip` を mmap、stat（size / inode 相当 / mtime）から [`BuildParams`]。
//! 2. `archive.zip.vmm/vmidx` があれば読み、`mount::resolve_index` で検証
//!    （fingerprint）。無効・不在なら EAGER で再構築。
//! 3. 再構築したら `vmidx.tmp` に書いて `rename`（Section 6.3 a/b）。
//!
//! 既定プロファイルでは fsync しない（Section 6.3 c。vmidx はキャッシュで、
//! 失われても再構築できる）。クラッシュ安全な durability は後段（M3）。

use crate::archive::{Archive, Bloat};
use crate::commit::{appended_cd_block, build_full, build_incremental, CommitError};
use crate::difflayer::{DiffLayer, UNLIMITED};
use crate::entrytable::EntryTable;
use crate::index_build::BuildParams;
use crate::mount::{
    entry_create, entry_remove, entry_rename, entry_truncate, read_cached, read_dirty,
    resolve_entry, resolve_index, write_into, EntryCtx, EntryError, OpenError, PageIo, ReadError,
    WriteError,
};
use crate::page::{PageCache, PageConfig};
use crate::tier2::Tier2;
use crate::vmidx::hash_cd_block;
use crate::vmdirty::{
    new_generation_id, now_ns, read_vmdirty, DirtyPage, EntryOp, Header, MetaOp, RecoveryResult,
    SyncPolicy,
};
use memmap2::Mmap;
use std::cell::{Cell, RefCell};
use std::fmt;
use std::fs::{self, File, Metadata};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// open() 時の設定（ページ設定 + spill ポリシー + ESTALE 間隔）。spill は既定で
/// 無効（`dirty_limit = UNLIMITED`）＝従来どおり Tier 1 のみで動く。
#[derive(Debug, Clone, Copy)]
pub struct OpenOptions {
    pub page: PageConfig,
    /// Tier 1 に保持してよい dirty バイト上限。`UNLIMITED` で spill 無効。
    pub dirty_limit: u64,
    /// spill 書き込みの durability（既定 [`SyncPolicy::Sync`]）。
    pub sync: SyncPolicy,
    /// ESTALE チェック間隔（ページキャッシュミス N 回ごと。0 = 無効）。
    pub estale_interval: u32,
    /// [`FileMount::commit`] が FULL を選ぶ bloat 比の閾値（設計
    /// `gc_threshold`、既定 **2.0** = アーカイブが live の倍に膨らんだら回収）。
    /// `bloat_ratio ≥ gc_threshold` で FULL。
    pub gc_threshold: f64,
    /// [`FileMount::commit`] が FULL を選ぶ絶対 dead バイト数の閾値（設計
    /// `gc_max_bloat_bytes`、既定 `UNLIMITED` = バイト数では発火しない）。
    /// `bloat_bytes ≥ gc_max_bloat_bytes` で FULL。比とは独立に評価する。
    pub gc_max_bloat_bytes: u64,
}

impl Default for OpenOptions {
    fn default() -> OpenOptions {
        OpenOptions {
            page: PageConfig::default(),
            dirty_limit: UNLIMITED,
            sync: SyncPolicy::Sync,
            estale_interval: 1,
            gc_threshold: 2.0,
            gc_max_bloat_bytes: UNLIMITED,
        }
    }
}

/// [`FileMount::commit`] がどの経路を採ったか（設計 WRITE STRATEGY SELECTION）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitOutcome {
    /// dirty も構造変更も無く、何もしなかった。
    Noop,
    /// INCREMENTAL（末尾追記）を採った（bloat が閾値未満）。
    Incremental,
    /// FULL（全書き直し + rename）を採った（bloat が閾値以上）。
    Full,
}

/// 回復プロトコルの判断（設計 Section 3 の caller resolution options）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryDecision {
    /// committed なページ/操作のみ復元し、uncommitted は破棄する（自動回復に安全）。
    RecoverCommitted,
    /// committed / uncommitted の双方を復元する（部分状態を受容）。
    RecoverAll,
    /// vmdirty を削除して CLEAN でマウントする。
    Discard,
    /// マウントせず vmdirty も触らない（手動検査用）。open はエラーになる。
    Abort,
}

/// 回復決定木に渡す状況（設計 Section 3）。
pub struct RecoveryContext<'a> {
    pub result: &'a RecoveryResult,
    /// dirty 中にソース ZIP が変わった（fingerprint 不一致＝設計の CONFLICT）。
    pub source_changed: bool,
}

/// open() で vmdirty を見つけたときに呼ばれる回復ハンドラ。データ安全性の判断は
/// 呼び出し側に委ねる（設計 Section 3）。
pub trait RecoveryHandler {
    fn decide(&mut self, ctx: &RecoveryContext) -> RecoveryDecision;
}

/// 既定の回復ハンドラ＝設計 Section 3 の決定木。**曖昧でない枝のみ自動で安全側に
/// 倒し**、データを失いうる枝は [`RecoveryDecision::Abort`]（呼び出し側へ委ねる）:
///
/// - CONFLICT（ソース変更）/ ヘッダ読めず / version 非対応 → Abort。
/// - stale な空ファイル → Discard。
/// - commit 境界あり（`last_commit_seq > 0`）→ RecoverCommitted（設計の safe default）。
/// - commit マーカー無しで未コミットあり → Abort（recover_all/discard は明示が要る）。
pub struct DefaultRecoveryHandler;

impl RecoveryHandler for DefaultRecoveryHandler {
    fn decide(&mut self, ctx: &RecoveryContext) -> RecoveryDecision {
        use crate::vmdirty::RecoveryStatus;
        if ctx.source_changed {
            return RecoveryDecision::Abort;
        }
        match ctx.result.status {
            RecoveryStatus::Ok => {
                if ctx.result.is_empty() {
                    RecoveryDecision::Discard
                } else if ctx.result.last_commit_seq > 0 {
                    RecoveryDecision::RecoverCommitted
                } else {
                    RecoveryDecision::Abort
                }
            }
            _ => RecoveryDecision::Abort,
        }
    }
}

/// [`FileMount::open`] の失敗。
#[derive(Debug)]
pub enum FileMountError {
    /// ファイル操作（open / read / mmap / サイドカー書き込み）の失敗。
    Io(io::Error),
    /// マウントを開く処理（parse / 索引構築）の失敗。
    Open(OpenError),
    /// commit の新 ZIP 組み立ての失敗。
    Commit(CommitError),
    /// [`FileMount::compact`] を DIRTY なマウントで呼んだ（設計: compact() は CLEAN
    /// からのみ。先に [`commit`](FileMount::commit) すること）。
    CompactWhileDirty,
    /// vmdirty が存在し、回復の判断が呼び出し側に委ねられた（決定木が Abort、または
    /// ヘッダ破損 / version 非対応 / CONFLICT）。`RecoveryResult` を見て
    /// [`FileMount::open_with_recovery`] でハンドラを与え直す。エントリ操作を含む
    /// vmdirty も本増分では未対応のためここに来る。
    RecoveryRequired(Box<RecoveryResult>),
}

impl fmt::Display for FileMountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FileMountError::Io(e) => write!(f, "file mount: {e}"),
            FileMountError::Open(e) => write!(f, "file mount: {e}"),
            FileMountError::Commit(e) => write!(f, "file mount: {e}"),
            FileMountError::CompactWhileDirty => {
                write!(f, "file mount: compact() requires a clean mount; commit() first")
            }
            FileMountError::RecoveryRequired(r) => write!(
                f,
                "file mount: vmdirty present, recovery decision deferred to caller (status {:?}, last_commit_seq {})",
                r.status, r.last_commit_seq
            ),
        }
    }
}

impl std::error::Error for FileMountError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            FileMountError::Io(e) => Some(e),
            FileMountError::Open(e) => Some(e),
            FileMountError::Commit(e) => Some(e),
            FileMountError::CompactWhileDirty => None,
            FileMountError::RecoveryRequired(_) => None,
        }
    }
}

impl From<CommitError> for FileMountError {
    fn from(e: CommitError) -> FileMountError {
        FileMountError::Commit(e)
    }
}

impl From<io::Error> for FileMountError {
    fn from(e: io::Error) -> FileMountError {
        FileMountError::Io(e)
    }
}

impl From<OpenError> for FileMountError {
    fn from(e: OpenError) -> FileMountError {
        FileMountError::Open(e)
    }
}

/// open 時に記録する archive.zip の指紋（ESTALE 検出の比較対象）。cd_hash は
/// fingerprint レイヤが持つので、ランタイムの軽量チェックには stat 3 値のみ使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StatFingerprint {
    size: u64,
    inode: u64,
    mtime_ns: u64,
}

impl StatFingerprint {
    fn of(md: &Metadata) -> StatFingerprint {
        StatFingerprint {
            size: md.len(),
            inode: file_id(md),
            mtime_ns: mtime_ns(md),
        }
    }
}

/// ディスク上の `archive.zip` に対するマウント。読み取りに加え、Diff Layer
/// Tier 1 を介した書き込みと FULL commit（`archive.new.zip` → `rename`）を提供する。
pub struct FileMount {
    archive: Mmap,
    vmidx_image: Vec<u8>,
    cfg: PageConfig,
    cache: RefCell<PageCache>,
    /// 未コミットの dirty ページ（Tier 1）。commit で `archive.zip` に反映する。
    diff: RefCell<DiffLayer>,
    /// セッション内の構造変更（create / remove）を vmidx に被せる表。回復時は
    /// vmdirty の METADATA replay で再構成する。
    entries: RefCell<EntryTable>,
    /// Tier 2 spill ストア（vmdirty ジャーナル）。spill 有効時、または回復で dirty
    /// を読み込んだ時のみ `Some`。`None` は Tier 1 のみ（M2 互換）。
    tier2: RefCell<Option<Tier2>>,
    /// アクティブな vmdirty のパス（commit 成功時に削除）。
    vmdirty_path: PathBuf,
    /// 再 stat 用のパスと open 時指紋（ESTALE 検出、設計 SNAPSHOT CONSISTENCY）。
    archive_path: PathBuf,
    fingerprint: StatFingerprint,
    /// ESTALE チェック間隔（ページキャッシュミス N 回ごと。0 = 無効。既定 1）。
    estale_interval: u32,
    /// ミスチェックの呼び出し回数（間隔の判定用）。
    miss_tick: Cell<u32>,
    /// 一度 STALE になったら以降の read は一律 ESTALE（スティッキー）。
    stale: Cell<bool>,
    /// spill / compaction の durability（新 vmdirty 生成時に引き継ぐ）。
    sync: SyncPolicy,
    /// open 時のソース cd_hash（compaction で新 vmdirty ヘッダに入れる。fingerprint
    /// の一部）。再 stat はせず open 時の値を保持する。
    cd_hash: [u8; 16],
    /// [`FileMount::commit`] の FULL 発火閾値（設計 gc_threshold / gc_max_bloat_bytes）。
    gc_threshold: f64,
    gc_max_bloat_bytes: u64,
}

impl FileMount {
    /// `archive_path` の ZIP を開く。サイドカー vmidx を検証し、無効・不在なら
    /// EAGER で再構築して `archive.zip.vmm/vmidx` に書き戻す。ページ設定は既定、
    /// ESTALE チェックは毎ミス、spill 無効。回復は [`DefaultRecoveryHandler`]。
    pub fn open(archive_path: impl AsRef<Path>) -> Result<FileMount, FileMountError> {
        FileMount::open_with_options(archive_path, OpenOptions::default())
    }

    /// [`FileMount::open`] にページ設定を指定する版（他は既定）。
    pub fn open_with_page_config(
        archive_path: impl AsRef<Path>,
        cfg: PageConfig,
    ) -> Result<FileMount, FileMountError> {
        FileMount::open_with_options(
            archive_path,
            OpenOptions {
                page: cfg,
                ..OpenOptions::default()
            },
        )
    }

    /// [`OpenOptions`]（ページ設定 + spill ポリシー）を指定して開く。回復は
    /// [`DefaultRecoveryHandler`]。
    pub fn open_with_options(
        archive_path: impl AsRef<Path>,
        options: OpenOptions,
    ) -> Result<FileMount, FileMountError> {
        FileMount::open_with_recovery(archive_path, options, &mut DefaultRecoveryHandler)
    }

    /// 回復ハンドラを明示して開く（設計 Section 3 の全枝を呼び出し側が制御できる）。
    /// vmdirty が在れば `read_vmdirty` → fingerprint 照合 → ハンドラ判断 →
    /// applyRecovery（committed/全ページを Tier 1 に復元、旧 vmdirty を
    /// `vmdirty.bak.{gen}` へ rename、新 gen で開始し回復分を flush）。
    pub fn open_with_recovery<H: RecoveryHandler>(
        archive_path: impl AsRef<Path>,
        options: OpenOptions,
        handler: &mut H,
    ) -> Result<FileMount, FileMountError> {
        let archive_path = archive_path.as_ref();
        let sidecar = sidecar_dir(archive_path);

        // 中断した commit の始末（mmap を張る前 = truncate 可能）。ADR 0017。
        //
        // `commit.intent` は「アーカイブを変える前に、変更後に期待される状態を
        // 書いておく」write-ahead レコード。これがあると、実アーカイブを intent の
        // pre / post と突き合わせるだけで disposition が一意に決まる:
        //
        //   post 一致  → commit は完了した。dirty 状態は新アーカイブに在るので
        //                vmdirty は stale。replay せず捨てて CLEAN で開く。
        //   pre 一致   → commit は起きなかった。巻き戻すものは無い（FULL の
        //                `archive.zip.new` は下の orphan 掃除が消す）。vmdirty は活かす。
        //   どちらでもない → INCREMENTAL なら追記が途中。`pre_size` へ truncate して
        //                旧アーカイブへ戻す。FULL なら外部改変なので何も触らず、
        //                後段の fingerprint 関門で CONFLICT に倒す。
        //
        // INTENT を**最後に**消すので、後始末の途中でクラッシュしても判定は変わらない。
        let mut discard_vmdirty_after_commit = false;
        {
            let intent_path = commit_intent_path(&sidecar);
            if let Some(intent) = read_commit_intent(&intent_path)? {
                let live = live_archive_fingerprint(archive_path)?;
                let mut resolved = true;
                if intent.matches_post(live) {
                    discard_vmdirty_after_commit = true;
                } else if intent.matches_pre(live) {
                    // 何もしない（vmdirty をそのまま回復に使う）。
                } else if intent.mode == CommitMode::Incremental {
                    // 追記が途中で切れている。旧バイト `[0, pre_size)` は書き換えて
                    // いないので、truncate すれば妥当な旧 ZIP に戻る。
                    let f = fs::OpenOptions::new().write(true).open(archive_path)?;
                    f.set_len(intent.pre_size)?;
                    f.sync_all()?;
                    // 戻した結果が本当に pre なら INTENT を消してよい。違えば
                    // 説明のつかない状態なので INTENT を残し、CONFLICT に倒す。
                    resolved = intent.matches_pre(live_archive_fingerprint(archive_path)?);
                } else {
                    // FULL でどちらとも一致しない = 外部がアーカイブを差し替えた。
                    resolved = false;
                }
                if resolved {
                    fs::remove_file(&intent_path)?;
                    let _ = fsync_parent_dir(&intent_path);
                }
            }
        }

        let file = File::open(archive_path)?;
        let md = file.metadata()?;
        let fingerprint = StatFingerprint::of(&md);
        let params = BuildParams {
            source_file_size: fingerprint.size,
            source_inode: fingerprint.inode,
            source_mtime_ns: fingerprint.mtime_ns,
            ..BuildParams::default()
        };

        // SAFETY: read-only マッピング。外部プロセスがアーカイブを書き換えると
        // 観測する内容が変わりうる（mmap 一般の未定義性）。本層は read-only
        // マウントとして扱い、変更検出は fingerprint / ESTALE に委ねる設計。
        let archive = unsafe { Mmap::map(&file)? };

        let vmidx_path = sidecar.join("vmidx");
        let existing = match fs::read(&vmidx_path) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };

        let (image, rebuilt) = resolve_index(&archive, existing, &params)?;
        if rebuilt {
            write_sidecar_index(&sidecar, &vmidx_path, &image)?;
        }

        let page_size = options.page.page_size;
        let mut diff = DiffLayer::with_dirty_limit(page_size, options.dirty_limit);
        let mut table = EntryTable::new();

        // 現在の cd_hash（fingerprint 照合 / 新 vmdirty ヘッダ用）。
        let cd_hash = {
            let ar = Archive::parse(&archive).map_err(|e| FileMountError::Open(OpenError::Zip(e)))?;
            hash_cd_block(ar.cd_block())
        };

        let vmdirty_path = sidecar.join("vmdirty");
        let mut tier2: Option<Tier2> = None;

        // 中断した compaction の置き去り（`vmdirty.compact`）は authoritative では
        // ないので掃除する（vmdirty 本体だけが正、orphan は無視）。
        let _ = fs::remove_file(compact_tmp(&vmdirty_path));

        // 中断した FULL commit の置き去り（`archive.zip.new`）も同様に掃除する
        // （設計 SIDECAR FILES と Journal Spec の `VmmMount.open`: "orphan … is
        // removed silently at next open()"）。**完成した中身でも採用しない** —
        // rename されていない以上 commit は成功しておらず、`archive.zip` が唯一の正。
        // 注: 仕様の前提「アクティブなマウントが無いとき」は lock 層（未実装）が
        // 入るまで検証できない。`vmdirty.compact` の掃除と同じ制約。
        let _ = fs::remove_file(commit_tmp(archive_path));

        // 完了済み INCREMENTAL commit の後始末: dirty 状態は新アーカイブに在るので
        // 残った vmdirty は stale。replay せず捨てる（CLEAN な新アーカイブとして開く）。
        if discard_vmdirty_after_commit && vmdirty_path.exists() {
            fs::remove_file(&vmdirty_path)?;
            let _ = fsync_parent_dir(&vmdirty_path);
        }

        // ── 回復（vmdirty があれば）──
        if vmdirty_path.exists() {
            let bytes = fs::read(&vmdirty_path)?;
            let result = read_vmdirty(&bytes);
            let source_changed = vmdirty_source_changed(&bytes, &fingerprint, &cd_hash);
            let decision = handler.decide(&RecoveryContext {
                result: &result,
                source_changed,
            });
            match decision {
                RecoveryDecision::Abort => {
                    return Err(FileMountError::RecoveryRequired(Box::new(result)));
                }
                RecoveryDecision::Discard => {
                    fs::remove_file(&vmdirty_path)?;
                }
                RecoveryDecision::RecoverCommitted | RecoveryDecision::RecoverAll => {
                    let all = decision == RecoveryDecision::RecoverAll;
                    // ページとエントリ操作を sequence 順に replay し、Diff Layer と
                    // エントリ表を復元する（設計 ENTRY OPERATIONS の "replays records
                    // strictly in sequence order"）。
                    replay_recovered(&mut diff, &mut table, &image, &result, all);
                    // 旧 vmdirty を bak へ退避し、新 gen で開始。回復した構造変更と
                    // 論理サイズを再 journal してから flush で durable に
                    // （新 vmdirty が自己完結し、二次クラッシュでも同じ状態へ戻る）。
                    let bak = vmdirty_bak_path(&sidecar, &result.generation_id);
                    fs::rename(&vmdirty_path, &bak)?;
                    let header = new_vmdirty_header(&fingerprint, &cd_hash, page_size as u32);
                    let mut t2 = Tier2::create(&vmdirty_path, &header, options.sync, page_size)?;
                    rejournal_recovered(&mut t2, &diff, &table)?;
                    t2.flush(&mut diff)?;
                    tier2 = Some(t2);
                }
            }
        }

        // spill 有効で、まだ Tier 2 が無ければ空の vmdirty を作る。
        if tier2.is_none() && options.dirty_limit != UNLIMITED {
            fs::create_dir_all(&sidecar)?;
            let header = new_vmdirty_header(&fingerprint, &cd_hash, page_size as u32);
            tier2 = Some(Tier2::create(&vmdirty_path, &header, options.sync, page_size)?);
        }

        Ok(FileMount {
            archive,
            vmidx_image: image,
            cfg: options.page,
            cache: RefCell::new(PageCache::from_config(&options.page)),
            diff: RefCell::new(diff),
            entries: RefCell::new(table),
            tier2: RefCell::new(tier2),
            vmdirty_path,
            archive_path: archive_path.to_path_buf(),
            fingerprint,
            estale_interval: options.estale_interval,
            miss_tick: Cell::new(0),
            stale: Cell::new(false),
            sync: options.sync,
            cd_hash,
            gc_threshold: options.gc_threshold,
            gc_max_bloat_bytes: options.gc_max_bloat_bytes,
        })
    }

    /// ESTALE チェック間隔を設定する（ページキャッシュミス N 回ごとに再 stat。
    /// 0 = チェック無効。設計 `--estale-check-interval`）。
    pub fn with_estale_interval(mut self, n: u32) -> FileMount {
        self.estale_interval = n;
        self
    }

    /// マウントが STALE 状態か（外部変更を検出済みか）。
    pub fn is_stale(&self) -> bool {
        self.stale.get()
    }

    /// エントリ `path` の展開ストリーム `[offset, offset + len)` を読む。
    /// ページキャッシュ経由（[`read_cached`]）。一度 STALE になると以降は
    /// 一律 [`ReadError::Stale`]。キャッシュミスのたびに archive.zip を再 stat し、
    /// 変更を検知したら STALE へ遷移する（設計 SNAPSHOT CONSISTENCY）。
    pub fn read(&self, path: &str, offset: u64, len: usize) -> Result<Vec<u8>, ReadError> {
        // STALE はスティッキー: キャッシュヒットで賄える read も含め一律拒否。
        if self.stale.get() {
            return Err(ReadError::Stale);
        }
        let resolved = resolve_entry(&self.entries.borrow(), &self.vmidx_image, path)?;
        // overlaid（dirty / created / 別名）は Diff Layer 経由（設計 READ PATH の
        // Tier 1 → Tier 2）。別名はソース名が現在名と異なるのでここに入る。
        if self.diff.borrow().is_dirty(path) || resolved.source.as_deref() != Some(path) {
            let t2 = self.tier2.borrow();
            let ctx = EntryCtx {
                archive: &self.archive,
                vmidx_image: &self.vmidx_image,
                path,
                source: resolved.source.as_deref(),
                original_size: resolved.original_size,
            };
            return read_dirty(&ctx, &self.diff.borrow(), t2.as_ref(), offset, len);
        }
        let mut cache = self.cache.borrow_mut();
        let mut io = PageIo { cache: &mut cache, cfg: &self.cfg };
        read_cached(
            &self.archive,
            &self.vmidx_image,
            &mut io,
            path,
            offset,
            len,
            || self.check_fresh(),
        )
    }

    /// エントリ `path` の `[offset, offset + data.len())` を書く（設計 WRITE PATH）。
    /// `archive.zip` は触らず Diff Layer Tier 1 に COW で取り込む。反映は
    /// [`commit`](FileMount::commit) まで遅延する。STALE 後は書けない。
    pub fn write(&self, path: &str, offset: u64, data: &[u8]) -> Result<(), WriteError> {
        let resolved = resolve_entry(&self.entries.borrow(), &self.vmidx_image, path)?;
        let mut t2 = self.tier2.borrow_mut();
        let ctx = EntryCtx {
            archive: &self.archive,
            vmidx_image: &self.vmidx_image,
            path,
            source: resolved.source.as_deref(),
            original_size: resolved.original_size,
        };
        write_into(&ctx, &mut self.diff.borrow_mut(), t2.as_mut(), offset, data)
    }

    /// 空のエントリを作る（設計 create()）。既存（未削除）なら [`EntryError::Exists`]。
    /// spill 有効時は METADATA CREATE を vmdirty に journal する。
    pub fn create(&self, path: &str) -> Result<(), EntryError> {
        let mut t2 = self.tier2.borrow_mut();
        entry_create(
            &mut self.entries.borrow_mut(),
            &mut self.diff.borrow_mut(),
            &self.vmidx_image,
            t2.as_mut(),
            path,
        )
    }

    /// エントリを削除する（設計 remove()）。存在しなければ [`EntryError::NotFound`]。
    pub fn remove(&self, path: &str) -> Result<(), EntryError> {
        let mut t2 = self.tier2.borrow_mut();
        entry_remove(
            &mut self.entries.borrow_mut(),
            &mut self.diff.borrow_mut(),
            &self.vmidx_image,
            t2.as_mut(),
            path,
        )
    }

    /// エントリの論理サイズを変える（設計 truncate()）。存在しなければ
    /// [`EntryError::NotFound`]。
    pub fn truncate(&self, path: &str, new_size: u64) -> Result<(), EntryError> {
        let mut t2 = self.tier2.borrow_mut();
        entry_truncate(
            &self.entries.borrow(),
            &mut self.diff.borrow_mut(),
            &self.vmidx_image,
            t2.as_mut(),
            path,
            new_size,
        )
    }

    /// エントリ `old` を `new` へ rename する（設計 rename()）。`old` が無ければ
    /// [`EntryError::NotFound`]、`new` が既に存在すれば（同名指定含む）
    /// [`EntryError::Exists`]。spill 有効時は METADATA RENAME を vmdirty に journal する。
    pub fn rename(&self, old: &str, new: &str) -> Result<(), EntryError> {
        let mut t2 = self.tier2.borrow_mut();
        entry_rename(
            &mut self.entries.borrow_mut(),
            &mut self.diff.borrow_mut(),
            &self.vmidx_image,
            t2.as_mut(),
            old,
            new,
        )
    }

    /// dirty な変更、または構造変更（create / remove / rename）があるか。
    pub fn is_dirty(&self) -> bool {
        !self.diff.borrow().is_empty() || !self.entries.borrow().is_empty()
    }

    /// Tier 1 の全 dirty ページを vmdirty へ durable 化し COMMIT MARKER を書く
    /// （設計 flush()、STRICT）。spill 無効（Tier 2 無し）なら no-op。flush 後の
    /// クラッシュは [`RecoveryDecision::RecoverCommitted`] で丸ごと復元できる。
    pub fn flush(&self) -> Result<(), FileMountError> {
        if let Some(t2) = self.tier2.borrow_mut().as_mut() {
            t2.flush(&mut self.diff.borrow_mut())?;
        }
        Ok(())
    }

    /// vmdirty **ジャーナル**の dead レコードが live ページを超えているか（journal
    /// compaction の発火条件、設計 `compactJournal()` = dead_record > live_record）。
    /// spill 無効なら常に `false`。アーカイブ本体の bloat（[`should_full_commit`]
    /// (Self::should_full_commit)）とは別軸。
    pub fn should_compact_journal(&self) -> bool {
        self.tier2
            .borrow()
            .as_ref()
            .is_some_and(Tier2::should_compact)
    }

    /// vmdirty **ジャーナル**を compaction する（⑤、設計 `compactJournal()`）。
    /// supersede / purge 済みの dead DATA RECORD を捨て、現在の live 状態だけを持つ
    /// 新世代ジャーナルを作って原子的に差し替える。spill 無効（Tier 2 無し）なら no-op。
    /// これは **vmdirty ジャーナル**の整理で、アーカイブ本体の dead 回収（[`compact`]
    /// (Self::compact) / FULL commit）とは別物。
    ///
    /// 手順（クラッシュ安全）:
    /// 1. `flush` で Tier 1 を旧 vmdirty へ durable 化（以降クラッシュしても旧
    ///    vmdirty が完全な authoritative）。
    /// 2. `vmdirty.compact`（別ファイル）に新世代を構築: METADATA（構造変更 +
    ///    RESIZE）→ live ページ（Tier 1 常駐は Diff から、Tier 2 のみは旧から読み写す）
    ///    → COMMIT MARKER。ここまでで新ファイルは durable。旧 vmdirty は無傷なので
    ///    この間のクラッシュは旧から復元でき、orphan な `vmdirty.compact` は次回 open で
    ///    掃除される。
    /// 3. 旧 Tier 2 を閉じ（Windows の置換のため）、`vmdirty.compact` → `vmdirty` へ
    ///    rename で原子置換し親 dir を fsync。以降は新世代で続行。
    pub fn compact_journal(&self) -> Result<(), FileMountError> {
        if self.tier2.borrow().is_none() {
            return Ok(());
        }
        let page_size = self.cfg.page_size;

        // 1. Tier 1 を旧 vmdirty へ durable 化（クラッシュ時の authoritative）。
        self.tier2
            .borrow_mut()
            .as_mut()
            .expect("tier2 present")
            .flush(&mut self.diff.borrow_mut())?;

        // 2. 新世代を temp に構築。
        let tmp = compact_tmp(&self.vmdirty_path);
        let header = new_vmdirty_header(&self.fingerprint, &self.cd_hash, page_size as u32);
        let mut new = Tier2::create(&tmp, &header, self.sync, page_size)?;
        // METADATA を先に（RENAME/CREATE/REMOVE + RESIZE。DATA より前の seq）。
        rejournal_recovered(&mut new, &self.diff.borrow(), &self.entries.borrow())?;
        {
            let old_ref = self.tier2.borrow();
            let old = old_ref.as_ref().expect("tier2 present");
            let diff = self.diff.borrow();
            // live ページ: Tier 1 常駐は Diff から（最新コピー）。
            for (entry, page) in diff.resident_pages() {
                let logical = diff.logical_size(&entry).unwrap_or(0);
                let full = diff.page(&entry, page).expect("resident page").to_vec();
                new.write_hit(&entry, page, &full, logical)?;
            }
            // Tier 2 のみのページは旧ジャーナルから読み写す（Tier 1 へは戻さない）。
            for (entry, page) in old.indexed_pages() {
                if diff.has_page(&entry, page) {
                    continue;
                }
                let logical = diff.logical_size(&entry).unwrap_or(0);
                let full = old.read_page(&entry, page)?.expect("indexed page present");
                new.write_hit(&entry, page, &full, logical)?;
            }
        }
        new.commit_marker()?; // 新ファイルを durable に締める。

        // 3. 旧を閉じてから原子置換。rename が失敗しても tier2 は必ず Some(new) に
        //    残し（None 化しない）、新世代は temp パスのまま journaling を続けられる
        //    （次回 open は旧 vmdirty から復元、orphan は掃除）。
        let old = self.tier2.borrow_mut().take().expect("tier2 present");
        drop(old);
        let renamed = fs::rename(&tmp, &self.vmdirty_path);
        *self.tier2.borrow_mut() = Some(new);
        renamed?;
        fsync_parent_dir(&self.vmdirty_path)?;
        Ok(())
    }

    /// 明示 FULL commit（設計 commit() FLOW の FULL path / `commit_strategy=FULL` /
    /// `commit(force_compact=true)`）。Diff Layer を反映した新しい完全な ZIP を
    /// `archive.new.zip` に書き、`archive.zip` へ `rename` で原子的に差し替える。
    /// マウントを消費する（mmap を解放）。クラッシュ前は元の `archive.zip` が無傷で残り、
    /// 後は新 ZIP が有効（POSIX rename の原子性）。変更が無ければ no-op。
    ///
    /// 通常は bloat 閾値で自動選択する [`commit`](Self::commit) を使う。FULL を強制
    /// したい時だけ本メソッドを直接呼ぶ。
    ///
    /// 耐久性（⑤ / Section 6.3）: 新 ZIP を `sync_all`（データ + メタデータ）してから
    /// `rename` し、rename 自体も親ディレクトリ fsync で durable にする。よって commit が
    /// 成功を返したら新アーカイブは安定ストレージ上にある。fsync 失敗は伝播する
    /// （fsyncgate）。サイドカー vmidx は更新せず残し（次回 open で fingerprint 不一致なら
    /// 再構築。vmidx はキャッシュなので fsync しない）。
    pub fn commit_full(self) -> Result<(), FileMountError> {
        if self.diff.borrow().is_empty() && self.entries.borrow().is_empty() {
            return Ok(());
        }
        self.full_commit_inner()
    }

    /// FULL commit の実体（空 Diff の早期 return を持たない）。[`commit_full`]
    /// (Self::commit_full) は変更なしを no-op にするが、[`compact`](Self::compact) は
    /// CLEAN でも dead 回収のため本体を走らせる。
    fn full_commit_inner(self) -> Result<(), FileMountError> {
        // 耐久性: アーカイブへ触れる前に dirty 状態を vmdirty で完結させる。flush で
        // Tier 1 常駐分を durable 化（commit 中のクラッシュは recover_committed で
        // 復元可能）、続いて Tier 2 のみのページを Tier 1 へ rehydrate して build_full
        // が全 dirty ページを Tier 1 から読めるようにする。
        if let Some(t2) = self.tier2.borrow_mut().as_mut() {
            t2.flush(&mut self.diff.borrow_mut())?;
            t2.rehydrate_into(&mut self.diff.borrow_mut())?;
        }

        let new_zip = build_full(
            &self.archive,
            &self.vmidx_image,
            &self.diff.borrow(),
            &self.entries.borrow(),
        )?;

        // archive.new.zip に書いてから rename で差し替える。Windows では mmap 保持中の
        // ファイルを切り詰め上書きできないが、別名 write → rename は通る（新 inode、
        // 旧マッピングは旧内容のまま生存。設計が想定する典型的な置き換え）。
        // 耐久性（Section 6.3 / fsyncgate）: 新 ZIP をデータ + メタデータごと fsync して
        // から rename し、rename 自体も親ディレクトリ fsync で durable にする。これで
        // 「commit が成功を返した ⇒ 新アーカイブは安定ストレージ上」を保証する。fsync
        // 失敗は握りつぶさず伝播する（vmidx はキャッシュなので別途 fsync しない）。
        let tmp = commit_tmp(&self.archive_path);
        durable_replace(&tmp, &self.archive_path, &new_zip)?;

        // commit 成功 → dirty 状態は新 archive に在る。vmdirty を削除し、その削除も
        // サイドカー dir の fsync で durable にする（設計 commit(): "On success: deletes
        // vmdirty"）。`vmdirty.bak.*` は forensics 用に残す。
        //
        // 残存ウィンドウ（既知）: アーカイブが durable になった後・vmdirty 削除が durable
        // になる前にクラッシュすると、再 open 時に「新アーカイブ + 旧 vmdirty」が残り、
        // 指紋不一致で CONFLICT（RecoveryRequired）に見える。完全な解消は commit epoch /
        // マーカ方式（別増分）が要る。ここでは窓を最小化するに留める。
        if self.vmdirty_path.exists() {
            let _ = fs::remove_file(&self.vmdirty_path);
            let _ = fsync_parent_dir(&self.vmdirty_path);
        }
        Ok(())
    }

    /// アーカイブの dead space を回収する FULL compaction（設計 `compact()`）。CLEAN な
    /// マウント（dirty ページも構造変更も無い）からのみ呼べ、書き込み無しで蓄積した
    /// dead を捨てた最小アーカイブを作って原子置換する。DIRTY なら
    /// [`FileMountError::CompactWhileDirty`]（先に [`commit`](Self::commit) すること）。
    ///
    /// これは **アーカイブ本体**の整理で、vmdirty ジャーナルの整理
    /// （[`compact_journal`](Self::compact_journal) = 設計 `compactJournal()`）とは別物。
    /// 中身は空 Diff での FULL commit（全エントリを verbatim コピーした正準形を書く）。
    pub fn compact(self) -> Result<(), FileMountError> {
        if !(self.diff.borrow().is_empty() && self.entries.borrow().is_empty()) {
            return Err(FileMountError::CompactWhileDirty);
        }
        self.full_commit_inner()
    }

    /// INCREMENTAL commit（設計 commit() FLOW の INCREMENTAL path。ADR 0012）。未変更
    /// エントリは元位置のまま、変更/新規/別名だけを **アーカイブ末尾へ追記**して新 CD/
    /// EOCD を書く。大きなアーカイブの小編集が安い（未変更分を再圧縮も再コピーもしない）。
    /// マウントを消費する（mmap を解放してから追記する）。
    ///
    /// クラッシュ安全（truncate ロールバック）: 旧バイト `[0, old_len)` を一切書き換えない
    /// ので、未完なら `old_len` への truncate で旧アーカイブ（妥当な ZIP）に戻る。
    /// 1. dirty を vmdirty へ durable 化（+ Tier 2 を Tier 1 へ rehydrate）。
    /// 2. INTENT（`old_len` / `new_len`）を sidecar に durable 記録（追記前）。
    /// 3. mmap を解放し、アーカイブ末尾へ追記して `sync_all`。
    /// 4. commit point: vmdirty を削除 → **最後に INTENT を削除**（INTENT を最後に消すことで、
    ///    回復は「完了（vmdirty 破棄）/未完（truncate ロールバック）」を一意に判定できる）。
    pub fn commit_incremental(self) -> Result<(), FileMountError> {
        if self.diff.borrow().is_empty() && self.entries.borrow().is_empty() {
            return Ok(());
        }

        // 1. dirty を vmdirty で完結させ、Tier 2 のみのページを Tier 1 へ（build_incremental が
        //    全 dirty ページを Tier 1 から読めるように）。FULL commit と同じ前処理。
        if let Some(t2) = self.tier2.borrow_mut().as_mut() {
            t2.flush(&mut self.diff.borrow_mut())?;
            t2.rehydrate_into(&mut self.diff.borrow_mut())?;
        }

        let old_len = self.archive.len() as u64;
        let appended = build_incremental(
            &self.archive,
            &self.vmidx_image,
            &self.diff.borrow(),
            &self.entries.borrow(),
        )?;
        let new_len = old_len + appended.len() as u64;

        // 追記後のアーカイブの cd_hash を、**書く前に**算出しておく（ADR 0017）。
        // 新しい CD は追記領域の中に丸ごと収まっているので、旧バイト列は要らない。
        let post_cd_hash = {
            let cd = appended_cd_block(&appended, old_len).ok_or_else(|| {
                FileMountError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "build_incremental produced no locatable central directory",
                ))
            })?;
            hash_cd_block(cd)
        };
        let intent = CommitIntent {
            mode: CommitMode::Incremental,
            pre_size: old_len,
            pre_cd_hash: self.cd_hash,
            post_size: new_len,
            post_cd_hash,
        };

        let archive_path = self.archive_path.clone();
        let vmdirty_path = self.vmdirty_path.clone();
        let sidecar = sidecar_dir(&archive_path);

        // mmap を解放してからファイルを変更する（Windows のマップ中ファイル制約回避）。
        drop(self);

        // 2. INTENT を durable に（追記前 = 判定とロールバックの根拠）。
        fs::create_dir_all(&sidecar)?;
        write_commit_intent(&sidecar, &intent)?;

        // 3. アーカイブ末尾へ追記して fsync（pure append なので親 dir fsync は不要、
        //    サイズ + データは sync_all が durable 化）。
        {
            let mut f = fs::OpenOptions::new().write(true).open(&archive_path)?;
            f.seek(SeekFrom::Start(old_len))?;
            f.write_all(&appended)?;
            f.sync_all()?;
        }

        // 4. commit point: dirty 状態は新アーカイブに在る → vmdirty を捨て、最後に INTENT を
        //    消す。サイドカー dir を 1 度 fsync して両削除を durable に。
        if vmdirty_path.exists() {
            let _ = fs::remove_file(&vmdirty_path);
        }
        let _ = fs::remove_file(commit_intent_path(&sidecar));
        let _ = fsync_parent_dir(&vmdirty_path);
        Ok(())
    }

    /// 現在のアーカイブの bloat 会計（[`Bloat`]、設計 "Bloat tracking"）。CD だけから
    /// 求まり追加 I/O 不要。open 時の mmap（= commit() 時点のアーカイブ）から算出する。
    pub fn bloat(&self) -> Bloat {
        match Archive::parse(&self.archive) {
            Ok(ar) => ar.bloat(),
            // open 済みなら parse は通るが、保険として live=file の中立値を返す。
            Err(_) => {
                let n = self.archive.len() as u64;
                Bloat {
                    file_size: n,
                    live_size: n,
                    bloat_bytes: 0,
                    bloat_ratio: 1.0,
                }
            }
        }
    }

    /// [`commit`](Self::commit) が FULL（全書き直し）を選ぶか（設計
    /// "Compaction thresholds"）。`bloat_ratio ≥ gc_threshold` または
    /// `bloat_bytes ≥ gc_max_bloat_bytes` のどちらか一方でも満たせば true。両条件は
    /// 独立に評価する。
    pub fn should_full_commit(&self) -> bool {
        let b = self.bloat();
        b.bloat_ratio >= self.gc_threshold || b.bloat_bytes >= self.gc_max_bloat_bytes
    }

    /// commit する（設計 WRITE STRATEGY SELECTION の `commit()`）。bloat 閾値で
    /// INCREMENTAL / FULL を自動選択する標準の入口。第一目標のディスク効率は、通常は
    /// 安い INCREMENTAL（未変更は追記ゼロ）で進め、dead が積もって
    /// [`should_full_commit`](Self::should_full_commit) が立った時だけ FULL で全回収する
    /// （定常サイズを `gc_threshold` 倍の live で上界する）ことで保つ。
    ///
    /// 選択は CD だけを見て決めるので、選んだ片方しか build しない（再圧縮の二度手間は
    /// 起きない）。FULL を強制したい呼び出し側は [`commit_full`](Self::commit_full) を、
    /// INCREMENTAL を強制したい側は [`commit_incremental`](Self::commit_incremental) を
    /// 直接呼ぶ（設計の `commit_strategy=FULL` / `force_compact` 相当）。マウントを消費する。
    pub fn commit(self) -> Result<CommitOutcome, FileMountError> {
        if self.diff.borrow().is_empty() && self.entries.borrow().is_empty() {
            return Ok(CommitOutcome::Noop);
        }
        if self.should_full_commit() {
            self.full_commit_inner()?;
            Ok(CommitOutcome::Full)
        } else {
            self.commit_incremental()?;
            Ok(CommitOutcome::Incremental)
        }
    }

    /// キャッシュミス時の鮮度チェック（ソースへ触れる前）。間隔判定を通過したら
    /// archive.zip を再 stat し、open 時指紋と size/inode/mtime を比較する。差異
    /// （またはファイル消失）で STALE へ遷移し [`ReadError::Stale`]。mtime は同
    /// サイズ in-place 編集を捕捉するトリガとして含める（設計どおりゲートではない
    /// が、ここでは差異検出に使う）。
    fn check_fresh(&self) -> Result<(), ReadError> {
        if self.estale_interval == 0 {
            return Ok(());
        }
        let tick = self.miss_tick.get().wrapping_add(1);
        self.miss_tick.set(tick);
        if !tick.is_multiple_of(self.estale_interval) {
            return Ok(());
        }
        match fs::metadata(&self.archive_path) {
            Ok(md) if StatFingerprint::of(&md) == self.fingerprint => Ok(()),
            // 差異あり、または stat 失敗（消失・差し替え途中など）→ STALE。
            _ => {
                self.stale.set(true);
                Err(ReadError::Stale)
            }
        }
    }

    /// 採用中の vmidx 像。
    pub fn index_bytes(&self) -> &[u8] {
        &self.vmidx_image
    }
}

/// `archive.zip` → `archive.zip.vmm` のサイドカーディレクトリパス。
fn sidecar_dir(archive_path: &Path) -> PathBuf {
    let mut name = archive_path
        .file_name()
        .unwrap_or_default()
        .to_os_string();
    name.push(".vmm");
    archive_path.with_file_name(name)
}

/// commit 用の一時ファイルパス（`archive.zip` の隣に `archive.zip.new`）。
/// 書いてから `archive.zip` へ rename する。
fn commit_tmp(archive_path: &Path) -> PathBuf {
    let mut name = archive_path
        .file_name()
        .unwrap_or_default()
        .to_os_string();
    name.push(".new");
    archive_path.with_file_name(name)
}

/// compaction 用の一時ファイルパス（`vmdirty` の隣に `vmdirty.compact`）。新世代を
/// ここへ構築し、durable にしてから `vmdirty` へ rename で原子置換する。中断時の
/// 置き去りは authoritative ではなく、次回 open で掃除する。
fn compact_tmp(vmdirty_path: &Path) -> PathBuf {
    let mut name = vmdirty_path.file_name().unwrap_or_default().to_os_string();
    name.push(".compact");
    vmdirty_path.with_file_name(name)
}

/// commit INTENT ファイル先頭 4 バイト（"VMIC" 相当の識別子）。
const INTENT_MAGIC: u32 = 0x564D_4943;

/// INTENT レコードの形式版。
///
/// - 1 = ADR 0012。`magic + old_len + new_len` の 20 バイト。INCREMENTAL 専用。
/// - 2 = ADR 0017。commit **前後**の fingerprint（size + cd_hash）を持ち、FULL も覆う。
///
/// version 1 のレコードは読まない（[`read_commit_intent`] が `None` を返す）。
/// クレートは未公開（`publish = false`）で、INTENT はクラッシュ窓にしか存在しない
/// 一時ファイルなので、on-disk 互換は保たない。
const INTENT_VERSION: u16 = 2;

/// version 2 の INTENT レコード長。
const INTENT_SIZE: usize = 60;

/// commit の書き込みモード。回復時にどの巻き戻しが要るかを決める。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommitMode {
    /// 新 ZIP を丸ごと書いて `rename` で置き換える。旧アーカイブは触らないので
    /// 巻き戻しは不要（置き去りの `archive.zip.new` は open で掃除される）。
    Full,
    /// 末尾へ追記する。未完なら `pre_size` への truncate で旧アーカイブへ戻す。
    Incremental,
}

/// アーカイブを変更する**前**に durable に記録する intent（ADR 0017）。
///
/// fingerprint だけでは「自分の commit が成功して archive が変わった」と
/// 「外部が archive を差し替えた」を区別できない（どちらも size / cd_hash が変わる）。
/// 変更後に期待される状態を先に書いておくことで、`open()` の判断が 1 ビットの
/// 一致判定から 3 分岐になる:
///
/// - 実アーカイブ == `post` → commit は完了した → vmdirty を捨てて CLEAN で開く
/// - 実アーカイブ == `pre`  → commit は起きなかった → 通常の回復
/// - どちらでもない → INCREMENTAL なら追記が途中（`pre_size` へ truncate）、
///   FULL なら本物の CONFLICT（外部改変）
///
/// **窓が無くなるのではなく、窓の中のどの状態も判定可能になる**、という性質の解決。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CommitIntent {
    mode: CommitMode,
    /// commit 前のアーカイブ長。INCREMENTAL の truncate ロールバック先でもある。
    pre_size: u64,
    /// commit 前のアーカイブの cd_hash。
    pre_cd_hash: [u8; 16],
    /// commit 後に期待されるアーカイブ長。
    post_size: u64,
    /// commit 後に期待される cd_hash。
    post_cd_hash: [u8; 16],
}

impl CommitIntent {
    /// `magic(4) + version(2) + mode(2) + pre_size(8) + pre_cd_hash(16)
    ///  + post_size(8) + post_cd_hash(16) + crc32c(4)` = 60 バイト。
    fn encode(&self) -> [u8; INTENT_SIZE] {
        let mut b = [0u8; INTENT_SIZE];
        b[0..4].copy_from_slice(&INTENT_MAGIC.to_le_bytes());
        b[4..6].copy_from_slice(&INTENT_VERSION.to_le_bytes());
        let mode: u16 = match self.mode {
            CommitMode::Full => 0,
            CommitMode::Incremental => 1,
        };
        b[6..8].copy_from_slice(&mode.to_le_bytes());
        b[8..16].copy_from_slice(&self.pre_size.to_le_bytes());
        b[16..32].copy_from_slice(&self.pre_cd_hash);
        b[32..40].copy_from_slice(&self.post_size.to_le_bytes());
        b[40..56].copy_from_slice(&self.post_cd_hash);
        let crc = crc32c::crc32c(&b[0..56]);
        b[56..60].copy_from_slice(&crc.to_le_bytes());
        b
    }

    /// 形式・版・CRC がすべて合ったときだけ `Some`。
    fn decode(b: &[u8]) -> Option<CommitIntent> {
        if b.len() != INTENT_SIZE {
            return None;
        }
        if u32::from_le_bytes(b[0..4].try_into().ok()?) != INTENT_MAGIC {
            return None;
        }
        if u16::from_le_bytes(b[4..6].try_into().ok()?) != INTENT_VERSION {
            return None;
        }
        if u32::from_le_bytes(b[56..60].try_into().ok()?) != crc32c::crc32c(&b[0..56]) {
            return None;
        }
        let mode = match u16::from_le_bytes(b[6..8].try_into().ok()?) {
            0 => CommitMode::Full,
            1 => CommitMode::Incremental,
            _ => return None,
        };
        let mut pre_cd_hash = [0u8; 16];
        pre_cd_hash.copy_from_slice(&b[16..32]);
        let mut post_cd_hash = [0u8; 16];
        post_cd_hash.copy_from_slice(&b[40..56]);
        Some(CommitIntent {
            mode,
            pre_size: u64::from_le_bytes(b[8..16].try_into().ok()?),
            pre_cd_hash,
            post_size: u64::from_le_bytes(b[32..40].try_into().ok()?),
            post_cd_hash,
        })
    }

    /// 実アーカイブが commit **前**の状態か。
    fn matches_pre(&self, live: (u64, Option<[u8; 16]>)) -> bool {
        live.0 == self.pre_size && live.1 == Some(self.pre_cd_hash)
    }

    /// 実アーカイブが commit **後**の状態か。
    fn matches_post(&self, live: (u64, Option<[u8; 16]>)) -> bool {
        live.0 == self.post_size && live.1 == Some(self.post_cd_hash)
    }
}

/// commit INTENT のファイルパス（`.vmm/commit.intent`）。アーカイブを変える前に
/// 書き、commit 成功で**最後に**削除する。
fn commit_intent_path(sidecar: &Path) -> PathBuf {
    sidecar.join("commit.intent")
}

/// INTENT を durable に書く（アーカイブ変更前。ファイル本体 + サイドカー dir を fsync）。
fn write_commit_intent(sidecar: &Path, intent: &CommitIntent) -> io::Result<()> {
    let path = commit_intent_path(sidecar);
    {
        let mut f = File::create(&path)?;
        f.write_all(&intent.encode())?;
        f.sync_all()?;
    }
    fsync_parent_dir(&path)?;
    Ok(())
}

/// INTENT を読む。無ければ `None`。壊れ / 部分書き / 旧版（magic・版・長さ・CRC の
/// いずれか不一致）も `None` 扱い＝INTENT が durable に書けていない＝アーカイブを
/// 変える前のクラッシュで、アーカイブは無傷なのでロールバック不要。
fn read_commit_intent(path: &Path) -> io::Result<Option<CommitIntent>> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    Ok(CommitIntent::decode(&bytes))
}

/// 現在の `archive.zip` の fingerprint（サイズ, cd_hash）。ZIP として parse できない
/// ときは cd_hash が `None` になり、intent のどちらの記録とも一致しなくなる。
fn live_archive_fingerprint(path: &Path) -> io::Result<(u64, Option<[u8; 16]>)> {
    let bytes = fs::read(path)?;
    let hash = Archive::parse(&bytes)
        .ok()
        .map(|ar| hash_cd_block(ar.cd_block()));
    Ok((bytes.len() as u64, hash))
}

/// `data` を `tmp` に書き、**fsync してから** `dst` へ rename で原子置換し、親
/// ディレクトリも fsync して rename 自体を durable にする（POSIX）。クラッシュ
/// 安全な「書いて差し替える」の標準手順（write → fsync → rename → dir fsync）。
/// fsync 失敗は握りつぶさず伝播する（fsyncgate）。Windows はディレクトリの fsync
/// 概念が無いので rename（MoveFileEx 相当）の原子性に委ねる。
fn durable_replace(tmp: &Path, dst: &Path, data: &[u8]) -> io::Result<()> {
    {
        let mut f = File::create(tmp)?;
        f.write_all(data)?;
        // データ + メタデータ（サイズ）の双方を安定ストレージへ。
        f.sync_all()?;
    }
    fs::rename(tmp, dst)?;
    fsync_parent_dir(dst)?;
    Ok(())
}

/// `path` の親ディレクトリを fsync して、その中で起きた rename / unlink を durable
/// にする（POSIX のディレクトリ耐久性）。Windows ではディレクトリハンドルへの
/// flush は一般に行えない / 意味を持たないので no-op（rename の原子性に委ねる）。
#[cfg(unix)]
fn fsync_parent_dir(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn fsync_parent_dir(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// vmidx 像を `vmidx.tmp` に書いて `vmidx` へ rename（Section 6.3 a/b）。vmidx は
/// 失われても再構築できるキャッシュなので、ここは fsync しない（Section 6.3 c）。
fn write_sidecar_index(sidecar: &Path, vmidx_path: &Path, image: &[u8]) -> io::Result<()> {
    fs::create_dir_all(sidecar)?;
    let tmp = sidecar.join("vmidx.tmp");
    fs::write(&tmp, image)?;
    fs::rename(&tmp, vmidx_path)?;
    Ok(())
}

/// vmdirty の `vmdirty.bak.{generation_id hex}` パス（回復後に旧ファイルを退避
/// する先。forensics 用に残し VMM は使わない。設計 Section 6）。
fn vmdirty_bak_path(sidecar: &Path, generation_id: &[u8; 16]) -> PathBuf {
    let mut hex = String::with_capacity(32);
    for b in generation_id {
        hex.push_str(&format!("{b:02x}"));
    }
    sidecar.join(format!("vmdirty.bak.{hex}"))
}

/// 新しい vmdirty FILE HEADER を組む（現在のソース指紋 + 新 generation_id）。
fn new_vmdirty_header(fp: &StatFingerprint, cd_hash: &[u8; 16], page_size: u32) -> Header {
    let mut source_cd_hash = [0u8; 20];
    source_cd_hash[..16].copy_from_slice(cd_hash);
    Header {
        flags: 0,
        generation_id: new_generation_id(),
        source_file_size: fp.size,
        source_inode: fp.inode,
        source_cd_hash,
        created_at_ns: now_ns(),
        page_size,
    }
}

/// vmdirty のヘッダ指紋が現在のソース ZIP と食い違うか（設計 Section 6 の
/// provenance check ＝ CONFLICT 判定）。cd_hash が確定要因、size はFAST チェック。
/// ヘッダが読めない場合は `false`（その失敗は `status` 側で扱う）。
fn vmdirty_source_changed(bytes: &[u8], fp: &StatFingerprint, cd_hash: &[u8; 16]) -> bool {
    match Header::decode(bytes) {
        Ok(h) => {
            h.source_file_size != fp.size
                || h.source_inode != fp.inode
                || h.source_cd_hash[..16] != cd_hash[..]
        }
        Err(_) => false,
    }
}

/// 回復で選んだページとエントリ操作（committed、`all` なら uncommitted も）を
/// **sequence 順に**統合 replay して Diff Layer Tier 1 とエントリ表を復元する
/// （設計 ENTRY OPERATIONS: "replays records strictly in sequence order"）。
/// ページの `logical_size` は設計 Section 2 の max ルール（`base = vmidx の
/// uncompressed_size`、`max(logical, page_index×page_size + data_len)`）、
/// METADATA は create/remove/resize をそのまま適用する。
fn replay_recovered(
    diff: &mut DiffLayer,
    table: &mut EntryTable,
    vmidx_image: &[u8],
    result: &RecoveryResult,
    all: bool,
) {
    enum Item<'a> {
        Page(&'a DirtyPage),
        Op(&'a EntryOp),
    }
    let mut items: Vec<(u64, Item)> = Vec::new();
    for p in &result.committed_pages {
        items.push((p.sequence, Item::Page(p)));
    }
    for o in &result.committed_ops {
        items.push((o.sequence, Item::Op(o)));
    }
    if all {
        for p in &result.uncommitted_pages {
            items.push((p.sequence, Item::Page(p)));
        }
        for o in &result.uncommitted_ops {
            items.push((o.sequence, Item::Op(o)));
        }
    }
    items.sort_by_key(|(seq, _)| *seq);

    let ps = diff.page_size();
    for (_, item) in items {
        match item {
            Item::Page(p) => {
                let base = base_for(vmidx_image, table, &p.entry_name);
                diff.ensure_entry(&p.entry_name, base);
                let candidate = p.page_index.saturating_mul(ps) + p.data.len() as u64;
                let cur = diff.logical_size(&p.entry_name).unwrap_or(base);
                diff.set_logical_size(&p.entry_name, cur.max(candidate));
                // ページは page_size まで右ゼロ埋めして載せる（短いテールも均一に）。
                let mut buf = vec![0u8; ps as usize];
                let n = p.data.len().min(ps as usize);
                buf[..n].copy_from_slice(&p.data[..n]);
                diff.insert_page(&p.entry_name, p.page_index, buf);
            }
            Item::Op(o) => match &o.op {
                MetaOp::Create => {
                    // 新規の空エントリで始める（create-after-remove のリスタート含む）。
                    diff.remove_entry(&o.entry_name);
                    table.mark_created(&o.entry_name);
                    diff.ensure_entry(&o.entry_name, 0);
                    diff.set_logical_size(&o.entry_name, 0);
                }
                MetaOp::Remove => {
                    table.mark_tombstone(&o.entry_name);
                    diff.remove_entry(&o.entry_name);
                }
                MetaOp::Resize { new_size } => {
                    let base = base_for(vmidx_image, table, &o.entry_name);
                    diff.ensure_entry(&o.entry_name, base);
                    let cur = diff.logical_size(&o.entry_name).unwrap_or(base);
                    if *new_size < cur {
                        diff.truncate_pages(&o.entry_name, *new_size);
                    }
                    diff.set_logical_size(&o.entry_name, *new_size);
                }
                MetaOp::Rename { new_name } => {
                    // dirty 状態を付け替えてから別名オーバーレイを立てる（以降の
                    // RESIZE/DATA は新名で来て base_for がソースから base を引く）。
                    let old_in_vmidx = original_size(vmidx_image, &o.entry_name).is_some();
                    diff.rename_entry(&o.entry_name, new_name);
                    table.apply_rename(&o.entry_name, new_name, old_in_vmidx);
                }
            },
        }
    }
}

/// 回復した状態を新しい vmdirty 世代へ再 journal する（flush でページを書く前）。
/// 構造変更（create / remove）と各 dirty エントリのサイズ（RESIZE）を先に書いて
/// おくことで、新 vmdirty 単体から同じ状態を復元できる（拡大は DATA RECORD が
/// 伸びを表さないので RESIZE が要る）。
///
/// source high-water（truncate-shrink で縮んだソース読み出し上限）が現在の論理
/// サイズより小さいときは、先に RESIZE(source_size)（縮小）を書いてから
/// RESIZE(logical)（拡大）を書く。これで二次クラッシュ後の replay でも「縮小して
/// 捨てた領域は extend してもゼロ」という不変条件が保たれる。
fn rejournal_recovered(t2: &mut Tier2, diff: &DiffLayer, table: &EntryTable) -> io::Result<()> {
    for name in table.created_names() {
        t2.journal_op(name, &MetaOp::Create)?;
    }
    // rename 元（別名のソース名）は tombstone だが、その消失は RENAME が表すので
    // REMOVE は書かない（書くと replay で RENAME 前にソースが消える）。
    let alias_sources: Vec<&str> = table.aliases().map(|(_, s)| s).collect();
    for name in table.tombstones() {
        if alias_sources.contains(&name) {
            continue;
        }
        t2.journal_op(name, &MetaOp::Remove)?;
    }
    // 別名は RENAME(ソース → 現在名) で再現する。RESIZE/DATA より前に書くことで
    // replay の base_for がソースから base を引ける（dirty 別名のサイズ整合）。
    let aliases: Vec<(String, String)> = table
        .aliases()
        .map(|(c, s)| (c.to_owned(), s.to_owned()))
        .collect();
    for (current, source) in aliases {
        t2.journal_op(
            &source,
            &MetaOp::Rename {
                new_name: current,
            },
        )?;
    }
    let sizes: Vec<(String, u64, u64)> = diff
        .dirty_paths()
        .map(|n| {
            let logical = diff.logical_size(n).unwrap_or(0);
            let source = diff.source_size(n).unwrap_or(logical);
            (n.to_owned(), source, logical)
        })
        .collect();
    for (name, source, logical) in sizes {
        if source < logical {
            t2.journal_op(&name, &MetaOp::Resize { new_size: source })?;
        }
        t2.journal_op(&name, &MetaOp::Resize { new_size: logical })?;
    }
    Ok(())
}

/// 回復 replay の logical_size base。別名（rename ターゲット）なら **ソース名**で
/// vmidx を引く（現在名は vmidx に無いため）。プレーンなら現在名。無ければ 0。
fn base_for(vmidx_image: &[u8], table: &EntryTable, name: &str) -> u64 {
    let src = table.aliased_source(name).unwrap_or(name);
    original_size(vmidx_image, src).unwrap_or(0)
}

/// vmidx からエントリの元 `uncompressed_size` を引く（回復の logical_size の base）。
fn original_size(vmidx_image: &[u8], name: &str) -> Option<u64> {
    let vmidx = crate::vmidx::Vmidx::parse(vmidx_image).ok()?;
    let (_, record) = vmidx.lookup(name).ok()??;
    Some(record.uncompressed_size)
}

/// inode 相当の安定 ID（fingerprint の FAST チェック用。cd_hash が確定要因なので
/// 取得できなければ 0 でよい）。
///
/// Windows の安定 file ID（`MetadataExt::file_index`）は nightly 限定
/// （`windows_by_handle`）なので、stable では 0 を返す。同一ファイルの再 open
/// では常に 0 同士で一致し、内容差は cd_hash が捉えるため問題ない。
#[cfg(unix)]
fn file_id(md: &Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    md.ino()
}

#[cfg(not(unix))]
fn file_id(_md: &Metadata) -> u64 {
    0
}

/// 更新時刻をエポックからのナノ秒で（fingerprint の変化トリガ。ゲートではない）。
fn mtime_ns(md: &Metadata) -> u64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// テスト用の一時ディレクトリ（Drop で再帰削除）。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> TempDir {
            static N: AtomicU32 = AtomicU32::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "zipvmm_disk_{}_{}",
                std::process::id(),
                n
            ));
            fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// STORE エントリだけの最小 ZIP を組む（ファイル I/O 経路の検証用。DEFLATE の
    /// 解凍は mount のテストで検証済み）。
    fn store_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut body = Vec::new();
        let mut cd = Vec::new();
        for (name, data) in entries {
            let lho = body.len() as u32;
            let nb = name.as_bytes();
            push32(&mut body, 0x0403_4b50);
            for _ in 0..2 {
                push16(&mut body, 0);
            }
            push16(&mut body, 0); // method = STORE
            push16(&mut body, 0);
            push16(&mut body, 0);
            push32(&mut body, 0);
            push32(&mut body, data.len() as u32);
            push32(&mut body, data.len() as u32);
            push16(&mut body, nb.len() as u16);
            push16(&mut body, 0);
            body.extend_from_slice(nb);
            body.extend_from_slice(data);

            push32(&mut cd, 0x0201_4b50);
            push16(&mut cd, 20);
            push16(&mut cd, 20);
            push16(&mut cd, 0);
            push16(&mut cd, 0);
            push16(&mut cd, 0);
            push16(&mut cd, 0);
            push32(&mut cd, 0);
            push32(&mut cd, data.len() as u32);
            push32(&mut cd, data.len() as u32);
            push16(&mut cd, nb.len() as u16);
            push16(&mut cd, 0);
            push16(&mut cd, 0);
            push16(&mut cd, 0);
            push16(&mut cd, 0);
            push32(&mut cd, 0);
            push32(&mut cd, lho);
            cd.extend_from_slice(nb);
        }
        let cd_offset = body.len() as u32;
        let cd_size = cd.len() as u32;
        body.extend_from_slice(&cd);
        push32(&mut body, 0x0605_4b50);
        push16(&mut body, 0);
        push16(&mut body, 0);
        push16(&mut body, entries.len() as u16);
        push16(&mut body, entries.len() as u16);
        push32(&mut body, cd_size);
        push32(&mut body, cd_offset);
        push16(&mut body, 0);
        body
    }

    fn push16(v: &mut Vec<u8>, x: u16) {
        v.extend_from_slice(&x.to_le_bytes());
    }
    fn push32(v: &mut Vec<u8>, x: u32) {
        v.extend_from_slice(&x.to_le_bytes());
    }

    #[test]
    fn opens_file_reads_and_persists_sidecar_index() {
        let dir = TempDir::new();
        let zip_path = dir.path().join("a.zip");
        fs::write(&zip_path, store_zip(&[("notes.txt", b"hello disk"), ("d/x.bin", b"payload")]))
            .unwrap();

        // サイドカーはまだ無い → open で EAGER 構築して書き出すはず。
        let vmidx_path = dir.path().join("a.zip.vmm").join("vmidx");
        assert!(!vmidx_path.exists());

        let m = FileMount::open(&zip_path).expect("open");
        assert_eq!(m.read("notes.txt", 0, 10).unwrap(), b"hello disk");
        assert_eq!(m.read("notes.txt", 6, 4).unwrap(), b"disk");
        assert_eq!(m.read("d/x.bin", 0, 7).unwrap(), b"payload");
        assert_eq!(m.read("absent", 0, 1), Err(ReadError::NotFound));

        // サイドカー vmidx が作られている。
        assert!(vmidx_path.exists());
        let persisted = fs::read(&vmidx_path).unwrap();
        assert_eq!(persisted, m.index_bytes());
    }

    #[test]
    fn reopen_reuses_persisted_index() {
        let dir = TempDir::new();
        let zip_path = dir.path().join("b.zip");
        fs::write(&zip_path, store_zip(&[("only.txt", b"abcdef")])).unwrap();

        let first = FileMount::open(&zip_path).expect("first open");
        let image1 = first.index_bytes().to_vec();
        drop(first);

        // 2 回目: サイドカーが有効なのでそのまま再利用される（同一バイト列）。
        let second = FileMount::open(&zip_path).expect("reopen");
        assert_eq!(second.index_bytes(), image1.as_slice());
        assert_eq!(second.read("only.txt", 2, 3).unwrap(), b"cde");
    }

    /// 中断した FULL commit の置き去り `archive.zip.new` は、次の `open()` で黙って
    /// 掃除される（設計 SIDECAR FILES と vmdirty Journal Spec の `VmmMount.open` が
    /// ともに "orphan … is removed silently at next open()" と要求している）。
    ///
    /// 中身が「完成した ZIP」でも採用はしない。rename されていない以上 commit は
    /// 成功しておらず、`archive.zip` が唯一の正。
    #[test]
    fn orphan_commit_tmp_is_removed_at_open() {
        let dir = TempDir::new();
        let zip_path = dir.path().join("orph.zip");
        fs::write(&zip_path, store_zip(&[("a.bin", &[0x11u8; 8])])).unwrap();

        // 中断した commit が残した .new（完成しているが別内容）。
        let orphan = dir.path().join("orph.zip.new");
        fs::write(&orphan, store_zip(&[("a.bin", &[0x99u8; 8])])).unwrap();

        let m = FileMount::open(&zip_path).expect("open");
        assert!(
            !orphan.exists(),
            "orphan archive.zip.new must be removed silently at open()"
        );
        // 採用されていない = 元の内容が読める。
        assert_eq!(m.read("a.bin", 0, 8).unwrap(), vec![0x11u8; 8]);
    }

    #[test]
    fn corrupt_sidecar_is_rebuilt() {
        let dir = TempDir::new();
        let zip_path = dir.path().join("c.zip");
        fs::write(&zip_path, store_zip(&[("only.txt", b"abcdef")])).unwrap();

        // 壊れた vmidx を先に置く。
        let sidecar = dir.path().join("c.zip.vmm");
        fs::create_dir_all(&sidecar).unwrap();
        fs::write(sidecar.join("vmidx"), b"not a valid vmidx").unwrap();

        // open は parse 失敗 → 破棄して再構築し、正しく読める。
        let m = FileMount::open(&zip_path).expect("open rebuilds");
        assert_eq!(m.read("only.txt", 0, 6).unwrap(), b"abcdef");
        // 書き戻された vmidx は妥当（再 open で再利用できる）。
        let m2 = FileMount::open(&zip_path).expect("reopen");
        assert_eq!(m2.read("only.txt", 0, 6).unwrap(), b"abcdef");
    }

    /// 小さいページ・read-ahead 無しで開く（各ページが個別のミス＝鮮度チェック点
    /// になるようにする）フィクスチャ。
    fn open_paged(zip_path: &Path) -> FileMount {
        let cfg = PageConfig {
            page_size: 8,
            read_ahead_pages: 0,
            cache_bytes: 16 << 20,
        };
        FileMount::open_with_page_config(zip_path, cfg).expect("open")
    }

    /// 外部からの「アーカイブ差し替え」を模す。Windows では mmap 保持中のファイルを
    /// 切り詰め上書き（`fs::write`）できない（エラー 1224）ので、別名に書いてから
    /// rename で置き換える（= 新しい inode。旧マッピングは旧内容のまま生き残る）。
    /// これは設計が想定する典型的な外部変更（rename 差し替え）でもある。
    fn replace_archive(zip_path: &Path, content: Vec<u8>) {
        let tmp = zip_path.with_extension("new");
        fs::write(&tmp, content).unwrap();
        fs::rename(&tmp, zip_path).unwrap();
    }

    #[test]
    fn external_change_triggers_estale_and_is_sticky() {
        let dir = TempDir::new();
        let zip_path = dir.path().join("e.zip");
        fs::write(
            &zip_path,
            store_zip(&[("data.bin", b"0123456789abcdef0123456789abcdef")]),
        )
        .unwrap();

        let m = open_paged(&zip_path);
        // ページ 0 は通る（未変更）。
        assert_eq!(m.read("data.bin", 0, 4).unwrap(), b"0123");
        assert!(!m.is_stale());

        // 外部からサイズの違う内容に差し替える（mmap 保持下での書き換え）。
        replace_archive(&zip_path, store_zip(&[("data.bin", b"XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXYYYYYYYY")]));

        // 未キャッシュページ（offset 16）の読みはミス → 再 stat でサイズ差 → STALE。
        assert_eq!(m.read("data.bin", 16, 4), Err(ReadError::Stale));
        assert!(m.is_stale());

        // スティッキー: キャッシュ済みページ 0 の再読も一律 STALE。
        assert_eq!(m.read("data.bin", 0, 4), Err(ReadError::Stale));
    }

    #[test]
    fn estale_interval_zero_disables_check() {
        let dir = TempDir::new();
        let zip_path = dir.path().join("f.zip");
        fs::write(
            &zip_path,
            store_zip(&[("data.bin", b"0123456789abcdef0123456789abcdef")]),
        )
        .unwrap();

        let m = open_paged(&zip_path).with_estale_interval(0);
        assert_eq!(m.read("data.bin", 0, 4).unwrap(), b"0123");

        // サイズを変えて差し替えても、チェック無効なので STALE にならない。
        replace_archive(&zip_path, store_zip(&[("data.bin", b"XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXYYYYYYYY")]));
        // 未キャッシュページの読みでもチェックは走らず、STALE を返さない。
        assert_ne!(m.read("data.bin", 0, 4), Err(ReadError::Stale));
        assert!(!m.is_stale());
    }

    #[test]
    fn write_then_commit_persists_and_reopens() {
        let dir = TempDir::new();
        let zip_path = dir.path().join("w.zip");
        fs::write(
            &zip_path,
            store_zip(&[("a.txt", b"hello world"), ("b.txt", b"unchanged")]),
        )
        .unwrap();

        let m = FileMount::open(&zip_path).expect("open");
        m.write("a.txt", 6, b"rust!").unwrap();
        assert!(m.is_dirty());
        // dirty な読みは Diff Layer から。
        assert_eq!(m.read("a.txt", 0, 11).unwrap(), b"hello rust!");
        // FULL commit → archive.zip を差し替え、マウントを消費。
        m.commit_full().expect("commit");

        // 一時ファイルは残っていない。
        assert!(!dir.path().join("w.zip.new").exists());

        // 開き直すと反映済み（vmidx は fingerprint 不一致で再構築される）。
        let m2 = FileMount::open(&zip_path).expect("reopen");
        assert_eq!(m2.read("a.txt", 0, 11).unwrap(), b"hello rust!");
        assert_eq!(m2.read("b.txt", 0, 9).unwrap(), b"unchanged");
    }

    #[test]
    fn commit_without_writes_is_noop() {
        let dir = TempDir::new();
        let zip_path = dir.path().join("n.zip");
        let original = store_zip(&[("a.txt", b"abc")]);
        fs::write(&zip_path, &original).unwrap();

        let m = FileMount::open(&zip_path).expect("open");
        assert!(!m.is_dirty());
        m.commit_full().expect("noop commit");
        // ファイルは書き換えられていない。
        assert_eq!(fs::read(&zip_path).unwrap(), original);
    }

    #[test]
    fn estale_interval_throttles_checks() {
        let dir = TempDir::new();
        let zip_path = dir.path().join("g.zip");
        fs::write(
            &zip_path,
            store_zip(&[("data.bin", b"0123456789abcdef0123456789abcdef")]),
        )
        .unwrap();

        // 間隔 2 = ミス 2 回ごとに stat。
        let m = open_paged(&zip_path).with_estale_interval(2);
        // ミス 1 回目（ページ 0）: stat しない。
        assert_eq!(m.read("data.bin", 0, 4).unwrap(), b"0123");

        replace_archive(&zip_path, store_zip(&[("data.bin", b"XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXYYYYYYYY")]));

        // ミス 2 回目（ページ 2 = offset16）: ここで stat → サイズ差 → STALE。
        assert_eq!(m.read("data.bin", 16, 4), Err(ReadError::Stale));
        assert!(m.is_stale());
    }

    // ───────────────────────── M3 ③: Tier 2 spill + 回復 ─────────────────────

    /// 8B ページ / read-ahead 無し / 指定 `dirty_limit` で spill を起こす設定。
    fn spill_options(dirty_limit: u64) -> OpenOptions {
        OpenOptions {
            page: PageConfig {
                page_size: 8,
                read_ahead_pages: 0,
                cache_bytes: 16 << 20,
            },
            dirty_limit,
            sync: SyncPolicy::Sync,
            estale_interval: 1,
            ..OpenOptions::default()
        }
    }

    fn open_spill(zip_path: &Path, dirty_limit: u64) -> FileMount {
        FileMount::open_with_options(zip_path, spill_options(dirty_limit)).expect("open spill")
    }

    /// 常に固定の回復判断を返すテスト用ハンドラ。
    struct FixedHandler(RecoveryDecision);
    impl RecoveryHandler for FixedHandler {
        fn decide(&mut self, _ctx: &RecoveryContext) -> RecoveryDecision {
            self.0
        }
    }

    fn vmdirty_path(dir: &Path, zip: &str) -> PathBuf {
        dir.join(format!("{zip}.vmm")).join("vmdirty")
    }

    fn count_baks(dir: &Path, zip: &str) -> usize {
        let sidecar = dir.join(format!("{zip}.vmm"));
        fs::read_dir(&sidecar)
            .map(|rd| {
                rd.filter_map(Result::ok)
                    .filter(|e| {
                        e.file_name()
                            .to_string_lossy()
                            .starts_with("vmdirty.bak.")
                    })
                    .count()
            })
            .unwrap_or(0)
    }

    #[test]
    fn spill_then_three_tier_read_is_correct() {
        let dir = TempDir::new();
        let zip_path = dir.path().join("s.zip");
        let original: Vec<u8> = (0..64u8).collect();
        fs::write(&zip_path, store_zip(&[("data.bin", &original)])).unwrap();

        // 上限 2 ページ = 16B。64B（8 ページ）書くと 6 ページが Tier 2 へ spill。
        let m = open_spill(&zip_path, 2 * 8);
        m.write("data.bin", 0, &[0xFFu8; 64]).unwrap();
        assert!(vmdirty_path(dir.path(), "s.zip").exists(), "spill creates vmdirty");

        // Tier 1（常駐）と Tier 2（spill 済み）双方から正しく読み戻せる。
        assert_eq!(m.read("data.bin", 0, 64).unwrap(), vec![0xFFu8; 64]);
        assert_eq!(m.read("data.bin", 30, 10).unwrap(), vec![0xFFu8; 10]);
        // 末尾ぴったり。
        assert_eq!(m.read("data.bin", 56, 8).unwrap(), vec![0xFFu8; 8]);
    }

    #[test]
    fn spilled_entry_reads_unwritten_pages_from_source() {
        let dir = TempDir::new();
        let zip_path = dir.path().join("s2.zip");
        let original: Vec<u8> = (0..64u8).collect();
        fs::write(&zip_path, store_zip(&[("data.bin", &original)])).unwrap();

        // SPILL_ONLY（上限 0）: 書いたページは即 Tier 2 へ。
        let m = open_spill(&zip_path, 0);
        m.write("data.bin", 0, &[0xAAu8; 8]).unwrap(); // ページ 0 のみ
        // ページ 0 は Tier 2 から、未書き込みページはソースから。
        assert_eq!(m.read("data.bin", 0, 8).unwrap(), vec![0xAAu8; 8]);
        assert_eq!(m.read("data.bin", 40, 8).unwrap(), &original[40..48]);
    }

    #[test]
    fn write_hit_on_spilled_page_supersedes() {
        let dir = TempDir::new();
        let zip_path = dir.path().join("s3.zip");
        let original: Vec<u8> = (0..16u8).collect();
        fs::write(&zip_path, store_zip(&[("data.bin", &original)])).unwrap();

        let m = open_spill(&zip_path, 0); // 即 spill
        m.write("data.bin", 0, &[0x01u8; 8]).unwrap(); // ページ 0 → Tier 2
        // 同じページ 0 へ再書き込み（Tier 2 ヒット → 新 DATA RECORD）。
        m.write("data.bin", 0, &[0x02u8; 8]).unwrap();
        assert_eq!(m.read("data.bin", 0, 8).unwrap(), vec![0x02u8; 8]);
    }

    #[test]
    fn commit_after_spill_reflects_all_pages() {
        let dir = TempDir::new();
        let zip_path = dir.path().join("s4.zip");
        let original: Vec<u8> = (0..64u8).collect();
        fs::write(&zip_path, store_zip(&[("data.bin", &original), ("keep.txt", b"unchanged")]))
            .unwrap();

        let m = open_spill(&zip_path, 2 * 8);
        // 偶数バイトだけ 0xEE に（ページを跨いで spill 済み・常駐が混在）。
        for i in (0..64u64).step_by(2) {
            m.write("data.bin", i, &[0xEEu8]).unwrap();
        }
        m.commit_full().expect("commit after spill");
        // commit 成功で vmdirty は消える。
        assert!(!vmdirty_path(dir.path(), "s4.zip").exists());

        // 開き直すと全ページ反映済み（rehydrate で Tier 2 分も拾われた）。
        let m2 = FileMount::open(&zip_path).expect("reopen");
        let mut expect = original.clone();
        for i in (0..64usize).step_by(2) {
            expect[i] = 0xEE;
        }
        assert_eq!(m2.read("data.bin", 0, 64).unwrap(), expect);
        assert_eq!(m2.read("keep.txt", 0, 9).unwrap(), b"unchanged");
    }

    /// **回帰テスト（重い方）**: `flush()` のあとに書き足してから commit しても、
    /// アーカイブの内容が壊れないこと。
    ///
    /// `VmdirtyWriter::pos` が COMMIT MARKER 分だけ進んでいなかったため、marker より
    /// 後に Tier 2 へ落ちたページの `data_offset` がずれる。commit は
    /// `rehydrate_into` でその索引を使って Tier 2 のページを読み戻すので、
    /// **ずれたオフセットの中身がそのまま新しい ZIP に焼き込まれる**（黙ってアーカイブが
    /// 壊れる）。tier2 側の単体テストは読み戻しのずれを、こちらは実害を押さえる。
    #[test]
    fn commit_after_flush_then_more_writes_is_not_corrupted() {
        let dir = TempDir::new();
        let zip_path = dir.path().join("fm.zip");
        fs::write(&zip_path, store_zip(&[("data.bin", &[0u8; 64])])).unwrap();

        // ページ 8B / 上限 2 ページ。
        let m = open_spill(&zip_path, 2 * 8);
        m.write("data.bin", 0, &[0x11u8; 16]).unwrap();
        m.flush().expect("flush"); // ← ここで COMMIT MARKER が入る
        // marker より後に書いたぶんが Tier 2 へ spill される。
        m.write("data.bin", 16, &[0x22u8; 32]).unwrap();
        m.commit_full().expect("commit");

        let m2 = FileMount::open(&zip_path).expect("reopen");
        let got = m2.read("data.bin", 0, 64).unwrap();
        let mut want = vec![0u8; 64];
        want[..16].fill(0x11);
        want[16..48].fill(0x22);
        assert_eq!(got, want);
    }

    #[test]
    fn recover_committed_after_flush_then_crash() {
        let dir = TempDir::new();
        let zip_path = dir.path().join("r.zip");
        let original: Vec<u8> = (0..64u8).collect();
        fs::write(&zip_path, store_zip(&[("data.bin", &original)])).unwrap();

        // セッション 1: 書いて flush（durable）→ commit せずに「クラッシュ」（drop）。
        {
            let m = open_spill(&zip_path, 2 * 8);
            m.write("data.bin", 0, &[0x11u8; 64]).unwrap();
            m.flush().expect("flush");
        }
        // vmdirty は残っている（commit していない）。
        assert!(vmdirty_path(dir.path(), "r.zip").exists());

        // セッション 2: 既定ハンドラ = commit 境界ありなので auto recover_committed。
        let m2 = open_spill(&zip_path, 2 * 8);
        assert_eq!(m2.read("data.bin", 0, 64).unwrap(), vec![0x11u8; 64]);
        // 旧 vmdirty は bak へ退避されている。
        assert_eq!(count_baks(dir.path(), "r.zip"), 1);

        // 回復後に commit すると新 archive に反映され、開き直しても残る。
        m2.commit_full().expect("commit recovered");
        let m3 = FileMount::open(&zip_path).expect("reopen clean");
        assert_eq!(m3.read("data.bin", 0, 64).unwrap(), vec![0x11u8; 64]);
    }

    #[test]
    fn no_commit_marker_aborts_by_default_but_recover_all_works() {
        let dir = TempDir::new();
        let zip_path = dir.path().join("a.zip");
        let original: Vec<u8> = (0..64u8).collect();
        fs::write(&zip_path, store_zip(&[("data.bin", &original)])).unwrap();

        // セッション 1: spill するが flush しない（COMMIT MARKER 無し）→ crash。
        {
            let m = open_spill(&zip_path, 0);
            m.write("data.bin", 0, &[0x22u8; 64]).unwrap();
        }

        // 既定ハンドラ: last_commit_seq==0 かつ未コミットあり → Abort = RecoveryRequired。
        let res = FileMount::open_with_options(&zip_path, spill_options(0));
        assert!(
            matches!(res, Err(FileMountError::RecoveryRequired(_))),
            "expected RecoveryRequired"
        );

        // 明示的に recover_all を選べば未コミットページも復元される。
        let mut h = FixedHandler(RecoveryDecision::RecoverAll);
        let m2 = FileMount::open_with_recovery(&zip_path, spill_options(0), &mut h)
            .expect("recover_all");
        assert_eq!(m2.read("data.bin", 0, 64).unwrap(), vec![0x22u8; 64]);
    }

    #[test]
    fn source_change_is_conflict_and_aborts() {
        let dir = TempDir::new();
        let zip_path = dir.path().join("c.zip");
        let original: Vec<u8> = (0..64u8).collect();
        fs::write(&zip_path, store_zip(&[("data.bin", &original)])).unwrap();

        {
            let m = open_spill(&zip_path, 2 * 8);
            m.write("data.bin", 0, &[0x33u8; 64]).unwrap();
            m.flush().expect("flush");
        }
        // ソース ZIP をサイズの違う内容へ差し替える（cd_hash / size 不一致 = CONFLICT）。
        replace_archive(&zip_path, store_zip(&[("data.bin", &[0u8; 80])]));

        // 既定ハンドラは CONFLICT で Abort。
        let res = FileMount::open_with_options(&zip_path, spill_options(2 * 8));
        assert!(
            matches!(res, Err(FileMountError::RecoveryRequired(_))),
            "expected RecoveryRequired on conflict"
        );
    }

    // ───────────────────────── ④ エントリ操作（disk + spill + 回復）─────────────

    #[test]
    fn entry_ops_persist_through_commit() {
        let dir = TempDir::new();
        let zip_path = dir.path().join("eo.zip");
        fs::write(
            &zip_path,
            store_zip(&[("keep.txt", b"keep"), ("drop.txt", b"drop"), ("big.txt", b"0123456789")]),
        )
        .unwrap();

        let m = FileMount::open(&zip_path).expect("open");
        m.create("new.txt").unwrap();
        m.write("new.txt", 0, b"made").unwrap();
        m.remove("drop.txt").unwrap();
        m.truncate("big.txt", 4).unwrap();
        assert!(m.is_dirty());
        m.commit_full().expect("commit");

        let m2 = FileMount::open(&zip_path).expect("reopen");
        assert_eq!(m2.read("keep.txt", 0, 4).unwrap(), b"keep");
        assert_eq!(m2.read("new.txt", 0, 4).unwrap(), b"made");
        assert_eq!(m2.read("drop.txt", 0, 1), Err(ReadError::NotFound));
        // commit 後の big.txt はサイズ 4（clean エントリは範囲超え read が OutOfRange）。
        assert_eq!(m2.read("big.txt", 0, 4).unwrap(), b"0123");
    }

    #[test]
    fn entry_ops_only_commit_rewrites_archive() {
        // ページ書き込みは無く構造変更（remove）だけでも commit が走ること。
        let dir = TempDir::new();
        let zip_path = dir.path().join("eo2.zip");
        fs::write(&zip_path, store_zip(&[("a.txt", b"a"), ("b.txt", b"b")])).unwrap();
        let m = FileMount::open(&zip_path).expect("open");
        m.remove("b.txt").unwrap();
        assert!(m.is_dirty());
        m.commit_full().expect("commit");
        let m2 = FileMount::open(&zip_path).expect("reopen");
        assert_eq!(m2.read("a.txt", 0, 1).unwrap(), b"a");
        assert_eq!(m2.read("b.txt", 0, 1), Err(ReadError::NotFound));
    }

    #[test]
    fn recover_created_entry_after_flush_then_crash() {
        let dir = TempDir::new();
        let zip_path = dir.path().join("rc.zip");
        fs::write(&zip_path, store_zip(&[("data.bin", b"abcdefgh")])).unwrap();

        // セッション 1: create + write + flush（durable）→ commit せず crash。
        {
            let m = open_spill(&zip_path, 0); // 即 spill
            m.create("made.bin").unwrap();
            m.write("made.bin", 0, &[0x55u8; 8]).unwrap();
            m.flush().expect("flush");
        }
        assert!(vmdirty_path(dir.path(), "rc.zip").exists());

        // セッション 2: commit 境界あり → auto recover_committed。created が復活。
        let m2 = open_spill(&zip_path, 0);
        assert_eq!(m2.read("made.bin", 0, 8).unwrap(), vec![0x55u8; 8]);
        assert_eq!(m2.read("data.bin", 0, 8).unwrap(), b"abcdefgh");
        assert_eq!(count_baks(dir.path(), "rc.zip"), 1);

        // 回復後 commit すると新 archive に反映され、開き直しても残る。
        m2.commit_full().expect("commit recovered");
        let m3 = FileMount::open(&zip_path).expect("reopen clean");
        assert_eq!(m3.read("made.bin", 0, 8).unwrap(), vec![0x55u8; 8]);
        assert_eq!(m3.read("data.bin", 0, 8).unwrap(), b"abcdefgh");
    }

    #[test]
    fn recover_removed_entry_after_flush_then_crash() {
        let dir = TempDir::new();
        let zip_path = dir.path().join("rr.zip");
        fs::write(&zip_path, store_zip(&[("keep.bin", b"keep"), ("gone.bin", b"gone")])).unwrap();

        {
            let m = open_spill(&zip_path, 0);
            m.remove("gone.bin").unwrap();
            m.flush().expect("flush");
        }

        // 回復で tombstone が復活 → gone.bin は ENOENT。
        let m2 = open_spill(&zip_path, 0);
        assert_eq!(m2.read("gone.bin", 0, 1), Err(ReadError::NotFound));
        assert_eq!(m2.read("keep.bin", 0, 4).unwrap(), b"keep");
        m2.commit_full().expect("commit recovered");
        let m3 = FileMount::open(&zip_path).expect("reopen");
        assert_eq!(m3.read("gone.bin", 0, 1), Err(ReadError::NotFound));
        assert_eq!(m3.read("keep.bin", 0, 4).unwrap(), b"keep");
    }

    #[test]
    fn recover_truncate_extend_restores_logical_size() {
        let dir = TempDir::new();
        let zip_path = dir.path().join("rt.zip");
        let original: Vec<u8> = (0..8u8).collect();
        fs::write(&zip_path, store_zip(&[("data.bin", &original)])).unwrap();

        // truncate-extend は DATA RECORD が伸びを表さない（RESIZE METADATA のみ）。
        {
            let m = open_spill(&zip_path, 2 * 8);
            m.truncate("data.bin", 20).unwrap();
            m.flush().expect("flush");
        }

        // 回復で論理サイズ 20 が戻り、伸びた gap はゼロ、元データは保たれる。
        let m2 = open_spill(&zip_path, 2 * 8);
        let mut expect = original.clone();
        expect.resize(20, 0);
        assert_eq!(m2.read("data.bin", 0, 20).unwrap(), expect);
        m2.commit_full().expect("commit recovered");
        let m3 = FileMount::open(&zip_path).expect("reopen");
        assert_eq!(m3.read("data.bin", 0, 20).unwrap(), expect);
    }

    #[test]
    fn stale_empty_vmdirty_is_discarded() {
        let dir = TempDir::new();
        let zip_path = dir.path().join("e.zip");
        fs::write(&zip_path, store_zip(&[("data.bin", b"abcdefgh")])).unwrap();

        // ヘッダだけの空 vmdirty を作って閉じる（書き込みも flush もしない）。
        {
            let _m = open_spill(&zip_path, 2 * 8);
        }
        assert!(vmdirty_path(dir.path(), "e.zip").exists());

        // 再 open: 空ファイル → silently discard → CLEAN マウント。
        let m = FileMount::open(&zip_path).expect("clean reopen");
        assert_eq!(m.read("data.bin", 0, 8).unwrap(), b"abcdefgh");
        assert!(!m.is_dirty());
    }

    #[test]
    fn rename_persists_through_commit() {
        let dir = TempDir::new();
        let zip_path = dir.path().join("rn.zip");
        fs::write(&zip_path, store_zip(&[("a.txt", b"payload!"), ("k.txt", b"keep")])).unwrap();

        let m = FileMount::open(&zip_path).expect("open");
        m.rename("a.txt", "b.txt").unwrap();
        assert_eq!(m.read("a.txt", 0, 1), Err(ReadError::NotFound));
        assert_eq!(m.read("b.txt", 0, 8).unwrap(), b"payload!");
        m.commit_full().expect("commit");

        let m2 = FileMount::open(&zip_path).expect("reopen");
        assert_eq!(m2.read("b.txt", 0, 8).unwrap(), b"payload!");
        assert_eq!(m2.read("a.txt", 0, 1), Err(ReadError::NotFound));
        assert_eq!(m2.read("k.txt", 0, 4).unwrap(), b"keep");
    }

    #[test]
    fn recover_unchanged_rename_after_flush_then_crash() {
        let dir = TempDir::new();
        let zip_path = dir.path().join("rrn.zip");
        fs::write(&zip_path, store_zip(&[("a.bin", b"abcdefgh")])).unwrap();

        // セッション 1: 純粋な rename（書き込みなし）+ flush → commit せず crash。
        {
            let m = open_spill(&zip_path, 0);
            m.rename("a.bin", "b.bin").unwrap();
            m.flush().expect("flush");
        }
        assert!(vmdirty_path(dir.path(), "rrn.zip").exists());

        // セッション 2: auto recover_committed。別名は RENAME 再 journal から復元され、
        // 未変更ページはソース a.bin から読める。
        let m2 = open_spill(&zip_path, 0);
        assert_eq!(m2.read("b.bin", 0, 8).unwrap(), b"abcdefgh");
        assert_eq!(m2.read("a.bin", 0, 1), Err(ReadError::NotFound));
        m2.commit_full().expect("commit recovered");
        let m3 = FileMount::open(&zip_path).expect("reopen");
        assert_eq!(m3.read("b.bin", 0, 8).unwrap(), b"abcdefgh");
        assert_eq!(m3.read("a.bin", 0, 1), Err(ReadError::NotFound));
    }

    #[test]
    fn durable_replace_writes_and_atomically_replaces() {
        let dir = TempDir::new();
        let dst = dir.path().join("a.bin");
        fs::write(&dst, b"OLD CONTENT").unwrap();
        let tmp = dir.path().join("a.bin.new");

        durable_replace(&tmp, &dst, b"new durable bytes").expect("durable_replace");

        assert_eq!(fs::read(&dst).unwrap(), b"new durable bytes");
        // 一時ファイルは rename で消費され残らない。
        assert!(!tmp.exists());
    }

    #[test]
    fn commit_leaves_no_vmdirty_and_reopens_clean() {
        // 耐久 commit 後はサイドカー vmdirty が消え、再 open が CLEAN になること。
        let dir = TempDir::new();
        let zip_path = dir.path().join("dc.zip");
        fs::write(&zip_path, store_zip(&[("a.txt", b"0123456789")])).unwrap();

        let m = open_spill(&zip_path, 0); // spill 有効 → vmdirty 生成
        m.write("a.txt", 0, b"XY").unwrap();
        m.flush().expect("flush");
        assert!(vmdirty_path(dir.path(), "dc.zip").exists());
        m.commit_full().expect("durable commit");

        assert!(!vmdirty_path(dir.path(), "dc.zip").exists());
        let m2 = FileMount::open(&zip_path).expect("reopen");
        assert!(!m2.is_dirty());
        assert_eq!(m2.read("a.txt", 0, 10).unwrap(), b"XY23456789");
    }

    #[test]
    fn recover_rename_then_partial_write_after_flush_then_crash() {
        let dir = TempDir::new();
        let zip_path = dir.path().join("rnw.zip");
        // 16 バイト（ページ 8 で 2 ページ）。COW で先頭ページだけ書く。
        let original: Vec<u8> = (0..16u8).collect();
        fs::write(&zip_path, store_zip(&[("a.bin", &original)])).unwrap();

        {
            let m = open_spill(&zip_path, 0); // 即 spill（dirty 別名 → Tier 2）
            m.rename("a.bin", "b.bin").unwrap();
            m.write("b.bin", 0, b"XY").unwrap();
            m.flush().expect("flush");
        }

        // 回復後、書いた先頭は XY、未書き込み（2 ページ目）はソース a.bin の元バイト。
        let m2 = open_spill(&zip_path, 0);
        let mut expect = original.clone();
        expect[0] = b'X';
        expect[1] = b'Y';
        assert_eq!(m2.read("b.bin", 0, 16).unwrap(), expect);
        assert_eq!(m2.read("a.bin", 0, 1), Err(ReadError::NotFound));
        m2.commit_full().expect("commit recovered");
        let m3 = FileMount::open(&zip_path).expect("reopen");
        assert_eq!(m3.read("b.bin", 0, 16).unwrap(), expect);
    }

    #[test]
    fn compaction_shrinks_journal_and_preserves_state() {
        let dir = TempDir::new();
        let zip_path = dir.path().join("comp.zip");
        fs::write(&zip_path, store_zip(&[("a.bin", &[0u8; 8])])).unwrap();

        let m = open_spill(&zip_path, 0); // 即 spill
        // 同じページを繰り返し書く → 旧 DATA RECORD が dead として積む。
        for i in 0..10u8 {
            m.write("a.bin", 0, &[i; 8]).unwrap();
        }
        let vp = vmdirty_path(dir.path(), "comp.zip");
        let before = fs::metadata(&vp).unwrap().len();
        assert!(m.should_compact_journal(), "dead > live のはず");

        m.compact_journal().expect("compact_journal");

        // compaction 後は dead が消え、ジャーナルが縮む。
        assert!(!m.should_compact_journal());
        let after = fs::metadata(&vp).unwrap().len();
        assert!(after < before, "journal should shrink: {after} >= {before}");
        // orphan な vmdirty.compact は残らない。
        assert!(!compact_tmp(&vp).exists());
        // live 状態（最後に書いた値）は保たれる。
        assert_eq!(m.read("a.bin", 0, 8).unwrap(), vec![9u8; 8]);
    }

    #[test]
    fn compaction_result_survives_crash_and_recovers() {
        let dir = TempDir::new();
        let zip_path = dir.path().join("compr.zip");
        fs::write(&zip_path, store_zip(&[("a.bin", &[0u8; 8])])).unwrap();

        {
            let m = open_spill(&zip_path, 0);
            for i in 0..6u8 {
                m.write("a.bin", 0, &[i; 8]).unwrap();
            }
            m.compact_journal().expect("compact_journal");
            // compaction は COMMIT MARKER で締めるので、ここで crash しても committed。
        }

        // 再 open: 新世代ジャーナルから recover_committed。最後の書き込みが戻る。
        let m2 = open_spill(&zip_path, 0);
        assert_eq!(m2.read("a.bin", 0, 8).unwrap(), vec![5u8; 8]);
        m2.commit_full().expect("commit recovered");
        let m3 = FileMount::open(&zip_path).expect("reopen clean");
        assert_eq!(m3.read("a.bin", 0, 8).unwrap(), vec![5u8; 8]);
    }

    #[test]
    fn compaction_preserves_entry_ops_and_recovers() {
        let dir = TempDir::new();
        let zip_path = dir.path().join("compe.zip");
        fs::write(
            &zip_path,
            store_zip(&[("a.bin", b"AAAA"), ("b.bin", b"BBBB"), ("c.bin", b"CCCC")]),
        )
        .unwrap();

        {
            let m = open_spill(&zip_path, 0);
            m.create("new.bin").unwrap();
            m.write("new.bin", 0, &[7u8; 4]).unwrap();
            m.remove("b.bin").unwrap();
            m.rename("c.bin", "d.bin").unwrap();
            // dead を積む: new.bin の同じページを書き直す。
            for i in 0..5u8 {
                m.write("new.bin", 0, &[i; 4]).unwrap();
            }
            m.compact_journal().expect("compact_journal");

            // compaction 直後の live 状態。
            assert_eq!(m.read("new.bin", 0, 4).unwrap(), vec![4u8; 4]);
            assert_eq!(m.read("a.bin", 0, 4).unwrap(), b"AAAA");
            assert_eq!(m.read("b.bin", 0, 1), Err(ReadError::NotFound));
            assert_eq!(m.read("d.bin", 0, 4).unwrap(), b"CCCC");
            assert_eq!(m.read("c.bin", 0, 1), Err(ReadError::NotFound));
        }

        // crash → 新世代から回復しても同じ（構造変更 + 別名 + created が保たれる）。
        let m2 = open_spill(&zip_path, 0);
        assert_eq!(m2.read("new.bin", 0, 4).unwrap(), vec![4u8; 4]);
        assert_eq!(m2.read("b.bin", 0, 1), Err(ReadError::NotFound));
        assert_eq!(m2.read("d.bin", 0, 4).unwrap(), b"CCCC");
        assert_eq!(m2.read("c.bin", 0, 1), Err(ReadError::NotFound));
        m2.commit_full().expect("commit recovered");
        let m3 = FileMount::open(&zip_path).expect("reopen");
        assert_eq!(m3.read("new.bin", 0, 4).unwrap(), vec![4u8; 4]);
        assert_eq!(m3.read("d.bin", 0, 4).unwrap(), b"CCCC");
        assert_eq!(m3.read("b.bin", 0, 1), Err(ReadError::NotFound));
        assert_eq!(m3.read("c.bin", 0, 1), Err(ReadError::NotFound));
    }

    #[test]
    fn incremental_commit_appends_and_reopens_clean() {
        let dir = TempDir::new();
        let zip_path = dir.path().join("inc.zip");
        let orig = store_zip(&[("keep.txt", b"KEEP"), ("edit.txt", b"0123456789")]);
        fs::write(&zip_path, &orig).unwrap();

        let m = open_spill(&zip_path, 0);
        m.write("edit.txt", 0, b"XY").unwrap();
        m.commit_incremental().expect("incremental commit");

        // append-only: 既存バイトは prefix にそのまま残り、ファイルは伸びる。
        let grown = fs::read(&zip_path).unwrap();
        assert!(grown.len() > orig.len());
        assert_eq!(&grown[..orig.len()], &orig[..]);
        // 後始末: intent も vmdirty も残らない。
        assert!(!commit_intent_path(&sidecar_dir(&zip_path)).exists());
        assert!(!vmdirty_path(dir.path(), "inc.zip").exists());

        let m2 = FileMount::open(&zip_path).expect("reopen");
        assert!(!m2.is_dirty());
        assert_eq!(m2.read("keep.txt", 0, 4).unwrap(), b"KEEP");
        assert_eq!(m2.read("edit.txt", 0, 10).unwrap(), b"XY23456789");
    }

    #[test]
    fn incremental_commit_rolls_back_incomplete_append() {
        let dir = TempDir::new();
        let zip_path = dir.path().join("rb.zip");
        let orig = store_zip(&[("a.bin", b"ORIGINAL")]);
        fs::write(&zip_path, &orig).unwrap();
        let old_len = orig.len() as u64;

        // 書いて flush（vmdirty durable）、commit せず閉じる。
        {
            let m = open_spill(&zip_path, 0);
            m.write("a.bin", 0, b"ZZ").unwrap();
            m.flush().expect("flush");
        }

        // 「追記の途中でクラッシュ」を模す: 不完全な追記の痕跡 + INTENT を手で残す。
        {
            let mut f = fs::OpenOptions::new().append(true).open(&zip_path).unwrap();
            f.write_all(&[0xFFu8; 30]).unwrap(); // new_len(=+100) に届かない不完全追記
        }
        let pre_cd_hash = {
            let ar = Archive::parse(&orig).unwrap();
            hash_cd_block(ar.cd_block())
        };
        write_commit_intent(
            &sidecar_dir(&zip_path),
            &CommitIntent {
                mode: CommitMode::Incremental,
                pre_size: old_len,
                pre_cd_hash,
                post_size: old_len + 100, // 届かなかった追記後サイズ
                post_cd_hash: [0xAAu8; 16],
            },
        )
        .unwrap();

        // 再 open: INTENT 未完 → アーカイブを old_len へ truncate（旧へ復帰）、vmdirty を replay。
        let m2 = open_spill(&zip_path, 0);
        assert_eq!(fs::metadata(&zip_path).unwrap().len(), old_len); // ガベージが消えた。
        assert!(!commit_intent_path(&sidecar_dir(&zip_path)).exists());
        assert_eq!(m2.read("a.bin", 0, 8).unwrap(), b"ZZIGINAL"); // 旧 + dirty を復元。
    }

    /// **ADR 0017 で閉じた穴**: 追記の「サイズだけ durable・テール内容は未 durable」
    /// を模す（ファイル長は `new_len` だが末尾はゼロ）。
    ///
    /// 旧判定は `size == new_len && Archive::parse().is_ok()` だった。`find_eocd` は
    /// **後方走査**なので、ゼロのテールは読み飛ばされて**旧 EOCD** が拾われ、parse が
    /// 通ってしまう。結果「完了」と誤判定して vmdirty を捨て、書き込みが失われていた。
    ///
    /// intent の cd_hash 照合では、この状態は post とも pre とも一致しない
    /// （長さが pre と違い、cd_hash が post と違う）ので INCREMENTAL の巻き戻しに入り、
    /// `pre_size` へ truncate して vmdirty から dirty を回復する。
    #[test]
    fn incremental_rolls_back_when_tail_is_not_durable() {
        let dir = TempDir::new();
        let zip_path = dir.path().join("tail.zip");
        let orig = store_zip(&[("a.bin", b"ORIGINAL")]);
        fs::write(&zip_path, &orig).unwrap();
        let old_len = orig.len() as u64;

        // 書いて flush（vmdirty durable）、commit せず閉じる。
        {
            let m = open_spill(&zip_path, 0);
            m.write("a.bin", 0, b"ZZ").unwrap();
            m.flush().expect("flush");
        }

        // 追記のサイズだけが反映され、中身がゼロのまま残った状態。
        const TAIL: usize = 40;
        {
            let mut f = fs::OpenOptions::new().append(true).open(&zip_path).unwrap();
            f.write_all(&[0u8; TAIL]).unwrap();
        }
        // ゼロのテールでも「妥当な ZIP」として parse できてしまうことを確かめる
        // （旧判定が誤る条件そのもの）。
        let padded = fs::read(&zip_path).unwrap();
        assert_eq!(padded.len() as u64, old_len + TAIL as u64);
        assert!(
            Archive::parse(&padded).is_ok(),
            "backward EOCD scan still parses the old archive - this is why size+parse was not enough"
        );

        let pre_cd_hash = {
            let ar = Archive::parse(&orig).unwrap();
            hash_cd_block(ar.cd_block())
        };
        write_commit_intent(
            &sidecar_dir(&zip_path),
            &CommitIntent {
                mode: CommitMode::Incremental,
                pre_size: old_len,
                pre_cd_hash,
                post_size: old_len + TAIL as u64, // サイズだけは「完了」に見える
                post_cd_hash: [0xBBu8; 16],       // 本来の追記後 CD とは一致しない
            },
        )
        .unwrap();

        // 再 open: 巻き戻して dirty を回復する（捨てない）。
        let m2 = open_spill(&zip_path, 0);
        assert_eq!(fs::metadata(&zip_path).unwrap().len(), old_len);
        assert!(!commit_intent_path(&sidecar_dir(&zip_path)).exists());
        assert_eq!(m2.read("a.bin", 0, 8).unwrap(), b"ZZIGINAL");
    }

    #[test]
    fn incremental_commit_discards_stale_vmdirty_when_completed() {
        let dir = TempDir::new();
        let zip_path = dir.path().join("cp.zip");
        fs::write(&zip_path, store_zip(&[("a.bin", b"abcdefgh")])).unwrap();

        // 正常な incremental commit（intent / vmdirty は消え、archive = new）。
        {
            let m = open_spill(&zip_path, 0);
            m.write("a.bin", 0, b"ZZ").unwrap();
            m.commit_incremental().expect("commit");
        }
        let committed = fs::read(&zip_path).unwrap();
        let post_cd_hash = {
            let ar = Archive::parse(&committed).unwrap();
            hash_cd_block(ar.cd_block())
        };

        // 「追記完了後・後始末前にクラッシュ」を模す: 完了一致の INTENT + stale vmdirty。
        write_commit_intent(
            &sidecar_dir(&zip_path),
            &CommitIntent {
                mode: CommitMode::Incremental,
                pre_size: 0,
                pre_cd_hash: [0u8; 16],
                post_size: committed.len() as u64,
                post_cd_hash,
            },
        )
        .unwrap();
        fs::write(vmdirty_path(dir.path(), "cp.zip"), b"stale junk").unwrap();

        // 再 open: INTENT 完了判定 → stale vmdirty を捨て INTENT も消し、新アーカイブを CLEAN で開く。
        let m2 = FileMount::open(&zip_path).expect("reopen");
        assert!(!m2.is_dirty());
        assert_eq!(m2.read("a.bin", 0, 8).unwrap(), b"ZZcdefgh");
        assert!(!vmdirty_path(dir.path(), "cp.zip").exists());
        assert!(!commit_intent_path(&sidecar_dir(&zip_path)).exists());
    }

    // ───────────── M4 刻み3: INCREMENTAL/FULL 選択ポリシー（bloat 閾値）─────────────

    fn open_with_gc(zip_path: &Path, gc_threshold: f64) -> FileMount {
        FileMount::open_with_options(
            zip_path,
            OpenOptions {
                gc_threshold,
                ..OpenOptions::default()
            },
        )
        .expect("open with gc")
    }

    /// クリーン（dead 無し）なアーカイブは bloat_ratio ≈ 1.0 で、既定閾値（2.0）未満。
    /// よって commit は INCREMENTAL を選ぶ。
    #[test]
    fn bloat_ratio_clean_archive_below_threshold() {
        let dir = TempDir::new();
        let zip_path = dir.path().join("clean.zip");
        fs::write(
            &zip_path,
            store_zip(&[("a.txt", b"AAAAAAAA"), ("b.txt", b"BBBBBBBBBBBBBBBB")]),
        )
        .unwrap();

        let m = FileMount::open(&zip_path).expect("open");
        let b = m.bloat();
        assert!(b.bloat_ratio < 2.0, "clean ratio {} should be < 2.0", b.bloat_ratio);
        assert!(b.bloat_ratio >= 1.0);
        assert!(!m.should_full_commit());
    }

    /// 閾値未満（クリーン）への小編集は INCREMENTAL を選ぶ: ファイルは末尾追記で伸び、
    /// 先頭の既存バイトは保たれる（未変更エントリは元オフセットのまま）。
    #[test]
    fn commit_picks_incremental_below_threshold() {
        let dir = TempDir::new();
        let zip_path = dir.path().join("auto_inc.zip");
        let orig = store_zip(&[("keep.txt", b"KEEP"), ("edit.txt", b"0123456789")]);
        fs::write(&zip_path, &orig).unwrap();

        let m = FileMount::open(&zip_path).expect("open");
        m.write("edit.txt", 0, b"XY").unwrap();
        assert_eq!(m.commit().expect("commit"), CommitOutcome::Incremental);

        // append-only の痕跡: prefix 保持 + ファイル増。
        let grown = fs::read(&zip_path).unwrap();
        assert!(grown.len() > orig.len());
        assert_eq!(&grown[..orig.len()], &orig[..]);

        let m2 = FileMount::open(&zip_path).expect("reopen");
        assert!(!m2.is_dirty());
        assert_eq!(m2.read("keep.txt", 0, 4).unwrap(), b"KEEP");
        assert_eq!(m2.read("edit.txt", 0, 10).unwrap(), b"XY23456789");
    }

    /// INCREMENTAL の積み重ねで dead が live を超え bloat_ratio ≥ 2.0 になったら、
    /// commit は FULL（全書き直し）を選び、dead を回収して最小アーカイブに戻す。
    #[test]
    fn commit_picks_full_when_bloated() {
        let dir = TempDir::new();
        let zip_path = dir.path().join("auto_full.zip");
        // 単一エントリ（= live 全体）を rewrite すると旧コピーがそのまま dead になり、
        // 1 回の INCREMENTAL で bloat_ratio が 2.0 を超える。
        let big: Vec<u8> = (0..=255u8).cycle().take(300).collect();
        fs::write(&zip_path, store_zip(&[("big.bin", &big)])).unwrap();

        // 1 回 INCREMENTAL commit して dead を積む。
        {
            let m = FileMount::open(&zip_path).expect("open");
            m.write("big.bin", 0, b"ZZZZ").unwrap();
            m.commit_incremental().expect("incremental commit");
        }
        let bloated_len = fs::metadata(&zip_path).unwrap().len();

        // 再 open: dead が積もり ratio ≥ 2.0 → FULL を選ぶべき。
        let m2 = FileMount::open(&zip_path).expect("reopen");
        assert!(
            m2.should_full_commit(),
            "bloat_ratio {} should be >= 2.0",
            m2.bloat().bloat_ratio
        );
        m2.write("big.bin", 4, b"WWWW").unwrap();
        assert_eq!(m2.commit().expect("commit"), CommitOutcome::Full);

        // FULL で回収: ファイルは縮み、再 open 時の ratio は閾値未満（最小化）。
        let compacted_len = fs::metadata(&zip_path).unwrap().len();
        assert!(compacted_len < bloated_len, "FULL should shrink the archive");

        let m3 = FileMount::open(&zip_path).expect("reopen after full");
        assert!(!m3.should_full_commit());
        assert!(m3.bloat().bloat_ratio < 2.0);
        assert_eq!(m3.read("big.bin", 0, 8).unwrap(), b"ZZZZWWWW");
        assert_eq!(m3.read("big.bin", 8, 4).unwrap(), &big[8..12]);
    }

    /// 単一エントリを 1 回 INCREMENTAL commit して dead を積んだ（bloated な）
    /// アーカイブを作るヘルパ。返り値は積んだ後のファイル長。
    fn make_bloated_single_entry(zip_path: &Path, data: &[u8]) -> u64 {
        fs::write(zip_path, store_zip(&[("big.bin", data)])).unwrap();
        let m = FileMount::open(zip_path).expect("open");
        m.write("big.bin", 0, b"ZZZZ").unwrap();
        m.commit_incremental().expect("incremental commit");
        fs::metadata(zip_path).unwrap().len()
    }

    /// プリミティブは温存: 閾値超（policy なら FULL）でも `commit_incremental` を直接
    /// 呼べば INCREMENTAL を強制でき、ファイルは（縮まず）追記で伸びる。
    #[test]
    fn force_incremental_against_bloat_policy() {
        let dir = TempDir::new();
        let zip_path = dir.path().join("force_inc.zip");
        let big: Vec<u8> = (0..=255u8).cycle().take(300).collect();
        let bloated_len = make_bloated_single_entry(&zip_path, &big);

        let m = FileMount::open(&zip_path).expect("reopen");
        assert!(m.should_full_commit(), "policy would pick FULL");
        m.write("big.bin", 0, b"QQQQ").unwrap();
        // policy を無視して INCREMENTAL を強制。
        m.commit_incremental().expect("forced incremental");

        let after = fs::metadata(&zip_path).unwrap().len();
        assert!(after > bloated_len, "forced INCREMENTAL appends (FULL would shrink)");
        let m2 = FileMount::open(&zip_path).expect("reopen2");
        assert_eq!(m2.read("big.bin", 0, 4).unwrap(), b"QQQQ");
    }

    /// 低い gc_threshold は commit() に（普通の小編集でも）FULL を選ばせる
    /// （設計 `commit_strategy=FULL` / `gc_threshold` に相当）。
    #[test]
    fn low_gc_threshold_forces_full() {
        let dir = TempDir::new();
        let zip_path = dir.path().join("low_gc.zip");
        fs::write(&zip_path, store_zip(&[("a.bin", b"abcdefghij")])).unwrap();

        // gc_threshold=1.0 → クリーン（ratio>1.0）でも FULL 判定。
        let m = open_with_gc(&zip_path, 1.0);
        assert!(m.should_full_commit());
        m.write("a.bin", 0, b"ZZ").unwrap();
        assert_eq!(m.commit().expect("commit"), CommitOutcome::Full);

        let m2 = FileMount::open(&zip_path).expect("reopen");
        assert_eq!(m2.read("a.bin", 0, 10).unwrap(), b"ZZcdefghij");
    }

    /// 変更が無ければ commit は Noop で、ファイルにもサイドカーにも触れない。
    #[test]
    fn commit_noop_when_clean() {
        let dir = TempDir::new();
        let zip_path = dir.path().join("noop.zip");
        let orig = store_zip(&[("a.bin", b"unchanged")]);
        fs::write(&zip_path, &orig).unwrap();

        let m = FileMount::open(&zip_path).expect("open");
        assert_eq!(m.commit().expect("commit"), CommitOutcome::Noop);
        assert_eq!(fs::read(&zip_path).unwrap(), orig);
    }

    /// `compact()` は CLEAN なマウントから（書き込み無しで）蓄積した dead を回収し、
    /// 最小アーカイブへ縮める（設計 `compact()`）。中身は保たれる。
    #[test]
    fn compact_reclaims_dead_from_clean() {
        let dir = TempDir::new();
        let zip_path = dir.path().join("compact.zip");
        let big: Vec<u8> = (0..=255u8).cycle().take(300).collect();
        let bloated_len = make_bloated_single_entry(&zip_path, &big);

        // CLEAN なマウント（dirty 無し）から compact。
        let m = FileMount::open(&zip_path).expect("reopen");
        assert!(!m.is_dirty());
        assert!(m.should_full_commit(), "bloated → compact が効くはず");
        m.compact().expect("compact");

        let compacted_len = fs::metadata(&zip_path).unwrap().len();
        assert!(compacted_len < bloated_len, "compact should shrink the archive");

        let m2 = FileMount::open(&zip_path).expect("reopen after compact");
        assert!(!m2.should_full_commit());
        // 最新内容（ZZ + 元データの続き）が保たれる。
        assert_eq!(m2.read("big.bin", 0, 4).unwrap(), b"ZZZZ");
        assert_eq!(m2.read("big.bin", 4, 4).unwrap(), &big[4..8]);
    }

    /// `compact()` は DIRTY なマウントでは拒否される（先に commit すること）。
    /// アーカイブには触れない。
    #[test]
    fn compact_refused_when_dirty() {
        let dir = TempDir::new();
        let zip_path = dir.path().join("compact_dirty.zip");
        let orig = store_zip(&[("a.bin", b"abcdefghij")]);
        fs::write(&zip_path, &orig).unwrap();

        let m = FileMount::open(&zip_path).expect("open");
        m.write("a.bin", 0, b"ZZ").unwrap();
        assert!(matches!(
            m.compact(),
            Err(FileMountError::CompactWhileDirty)
        ));
        // 拒否時はアーカイブに触れない。
        assert_eq!(fs::read(&zip_path).unwrap(), orig);
    }
}
