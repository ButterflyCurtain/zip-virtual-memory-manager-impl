//! ディスク上の `archive.zip` を mmap し、サイドカー `archive.zip.vmm/vmidx` を
//! 読み書きする薄いファイル I/O 層（設計 SIDECAR FILES / FIRST-OPEN）。
//!
//! [`FileMount`] は実ファイルを read-only で mmap し、その `&[u8]` を
//! [`mount`](crate::mount) のロジック（`resolve_index` / `read_entry`）に渡す。
//! 設計方針「mmap は外から渡す」に沿い、mmap の所有はこの層に閉じ、`mount` /
//! `vmidx` / `archive` はバイトスライスだけを見る。
//!
//! open() の流れ:
//! 1. `archive.zip` を mmap、stat（size / inode 相当 / mtime）から [`BuildParams`]。
//! 2. `archive.zip.vmm/vmidx` があれば読み、`mount::resolve_index` で検証
//!    （fingerprint）。無効・不在なら EAGER で再構築。
//! 3. 再構築したら `vmidx.tmp` に書いて `rename`（Section 6.3 a/b）。
//!
//! 既定プロファイルでは fsync しない（Section 6.3 c。vmidx はキャッシュで、
//! 失われても再構築できる）。クラッシュ安全な durability は後段（M3）。

use crate::index_build::BuildParams;
use crate::mount::{read_cached, resolve_index, OpenError, ReadError};
use crate::page::{PageCache, PageConfig};
use memmap2::Mmap;
use std::cell::RefCell;
use std::fmt;
use std::fs::{self, File, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// [`FileMount::open`] の失敗。
#[derive(Debug)]
pub enum FileMountError {
    /// ファイル操作（open / read / mmap / サイドカー書き込み）の失敗。
    Io(io::Error),
    /// マウントを開く処理（parse / 索引構築）の失敗。
    Open(OpenError),
}

impl fmt::Display for FileMountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FileMountError::Io(e) => write!(f, "file mount: {e}"),
            FileMountError::Open(e) => write!(f, "file mount: {e}"),
        }
    }
}

impl std::error::Error for FileMountError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            FileMountError::Io(e) => Some(e),
            FileMountError::Open(e) => Some(e),
        }
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

/// ディスク上の `archive.zip` に対する読み取り専用マウント。
pub struct FileMount {
    archive: Mmap,
    vmidx_image: Vec<u8>,
    cfg: PageConfig,
    cache: RefCell<PageCache>,
}

impl FileMount {
    /// `archive_path` の ZIP を開く。サイドカー vmidx を検証し、無効・不在なら
    /// EAGER で再構築して `archive.zip.vmm/vmidx` に書き戻す。ページ設定は既定。
    pub fn open(archive_path: impl AsRef<Path>) -> Result<FileMount, FileMountError> {
        FileMount::open_with_page_config(archive_path, PageConfig::default())
    }

    /// [`FileMount::open`] にページ設定を指定する版。
    pub fn open_with_page_config(
        archive_path: impl AsRef<Path>,
        cfg: PageConfig,
    ) -> Result<FileMount, FileMountError> {
        let archive_path = archive_path.as_ref();
        let file = File::open(archive_path)?;
        let md = file.metadata()?;
        let params = BuildParams {
            source_file_size: md.len(),
            source_inode: file_id(&md),
            source_mtime_ns: mtime_ns(&md),
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
        let cache = RefCell::new(PageCache::from_config(&cfg));
        Ok(FileMount {
            archive,
            vmidx_image: image,
            cfg,
            cache,
        })
    }

    /// エントリ `path` の展開ストリーム `[offset, offset + len)` を読む。
    /// ページキャッシュ経由（[`read_cached`]）。
    pub fn read(&self, path: &str, offset: u64, len: usize) -> Result<Vec<u8>, ReadError> {
        let mut cache = self.cache.borrow_mut();
        read_cached(
            &self.archive,
            &self.vmidx_image,
            &mut cache,
            &self.cfg,
            path,
            offset,
            len,
        )
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

/// vmidx 像を `vmidx.tmp` に書いて `vmidx` へ rename（Section 6.3 a/b）。既定
/// プロファイルにつき fsync しない。
fn write_sidecar_index(sidecar: &Path, vmidx_path: &Path, image: &[u8]) -> io::Result<()> {
    fs::create_dir_all(sidecar)?;
    let tmp = sidecar.join("vmidx.tmp");
    fs::write(&tmp, image)?;
    fs::rename(&tmp, vmidx_path)?;
    Ok(())
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
}
