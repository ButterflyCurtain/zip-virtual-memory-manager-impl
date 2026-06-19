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

use crate::archive::Archive;
use crate::commit::{build_full, CommitError};
use crate::difflayer::{DiffLayer, UNLIMITED};
use crate::entrytable::EntryTable;
use crate::index_build::BuildParams;
use crate::mount::{
    entry_create, entry_remove, entry_truncate, read_cached, read_dirty, resolve_entry,
    resolve_index, write_into, EntryError, OpenError, ReadError, WriteError,
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
use std::io;
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
}

impl Default for OpenOptions {
    fn default() -> OpenOptions {
        OpenOptions {
            page: PageConfig::default(),
            dirty_limit: UNLIMITED,
            sync: SyncPolicy::Sync,
            estale_interval: 1,
        }
    }
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

        let sidecar = sidecar_dir(archive_path);
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
        // dirty（overlaid / 書き込み済み）または created は Diff Layer から
        // （設計 READ PATH の Tier 1 → Tier 2）。
        if self.diff.borrow().is_dirty(path) || resolved.source.is_none() {
            let t2 = self.tier2.borrow();
            return read_dirty(
                &self.archive,
                &self.vmidx_image,
                &self.diff.borrow(),
                t2.as_ref(),
                path,
                resolved.source.as_deref(),
                resolved.original_size,
                offset,
                len,
            );
        }
        let mut cache = self.cache.borrow_mut();
        read_cached(
            &self.archive,
            &self.vmidx_image,
            &mut cache,
            &self.cfg,
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
        write_into(
            &self.archive,
            &self.vmidx_image,
            &mut self.diff.borrow_mut(),
            t2.as_mut(),
            path,
            resolved.source.as_deref(),
            resolved.original_size,
            offset,
            data,
        )
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

    /// dirty な変更、または構造変更（create / remove）があるか。
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

    /// FULL commit（設計 commit() FLOW の FULL path）。Diff Layer を反映した新しい
    /// 完全な ZIP を `archive.new.zip` に書き、`archive.zip` へ `rename` で原子的に
    /// 差し替える。マウントを消費する（mmap を解放）。クラッシュ前は元の
    /// `archive.zip` が無傷で残り、後は新 ZIP が有効（POSIX rename の原子性）。
    ///
    /// 既定プロファイルでは fsync しない（M2。durability は M3）。サイドカー
    /// vmidx は更新せず残すが、次回 open 時に fingerprint 不一致で再構築される
    /// （vmidx はキャッシュ）。
    pub fn commit(self) -> Result<(), FileMountError> {
        if self.diff.borrow().is_empty() && self.entries.borrow().is_empty() {
            return Ok(());
        }

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
        let tmp = commit_tmp(&self.archive_path);
        fs::write(&tmp, &new_zip)?;
        fs::rename(&tmp, &self.archive_path)?;

        // commit 成功 → dirty 状態は新 archive に在る。vmdirty を削除する（設計
        // commit(): "On success: deletes vmdirty"）。`vmdirty.bak.*` は forensics 用に残す。
        if self.vmdirty_path.exists() {
            let _ = fs::remove_file(&self.vmdirty_path);
        }
        Ok(())
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
        if tick % self.estale_interval != 0 {
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

/// vmidx 像を `vmidx.tmp` に書いて `vmidx` へ rename（Section 6.3 a/b）。既定
/// プロファイルにつき fsync しない。
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
                let base = original_size(vmidx_image, &p.entry_name).unwrap_or(0);
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
                    let base = original_size(vmidx_image, &o.entry_name).unwrap_or(0);
                    diff.ensure_entry(&o.entry_name, base);
                    let cur = diff.logical_size(&o.entry_name).unwrap_or(base);
                    if *new_size < cur {
                        diff.truncate_pages(&o.entry_name, *new_size);
                    }
                    diff.set_logical_size(&o.entry_name, *new_size);
                }
                // RENAME（④b）は ④a の vmdirty には現れない。
                MetaOp::Rename { .. } => {}
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
    for name in table.tombstones() {
        t2.journal_op(name, &MetaOp::Remove)?;
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
            ..PageConfig::default()
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
        m.commit().expect("commit");

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
        m.commit().expect("noop commit");
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
        m.write("data.bin", 0, &vec![0xFFu8; 64]).unwrap();
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
        m.commit().expect("commit after spill");
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

    #[test]
    fn recover_committed_after_flush_then_crash() {
        let dir = TempDir::new();
        let zip_path = dir.path().join("r.zip");
        let original: Vec<u8> = (0..64u8).collect();
        fs::write(&zip_path, store_zip(&[("data.bin", &original)])).unwrap();

        // セッション 1: 書いて flush（durable）→ commit せずに「クラッシュ」（drop）。
        {
            let m = open_spill(&zip_path, 2 * 8);
            m.write("data.bin", 0, &vec![0x11u8; 64]).unwrap();
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
        m2.commit().expect("commit recovered");
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
            m.write("data.bin", 0, &vec![0x22u8; 64]).unwrap();
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
            m.write("data.bin", 0, &vec![0x33u8; 64]).unwrap();
            m.flush().expect("flush");
        }
        // ソース ZIP をサイズの違う内容へ差し替える（cd_hash / size 不一致 = CONFLICT）。
        replace_archive(&zip_path, store_zip(&[("data.bin", &vec![0u8; 80])]));

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
        m.commit().expect("commit");

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
        m.commit().expect("commit");
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
        m2.commit().expect("commit recovered");
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
        m2.commit().expect("commit recovered");
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
        m2.commit().expect("commit recovered");
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
}
