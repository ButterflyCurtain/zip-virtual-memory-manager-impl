//! archive → vmidx の構築レイヤ（[`archive`](crate::archive) と
//! [`vmidx`](crate::vmidx) を結合する）。
//!
//! [`Archive`] の Central Directory エントリ群から、完全な vmidx 像
//! （[`VmidxBuilder::serialize`] が返すバイト列）を組み立てる。これは設計の
//! Section 6.3「構造的書き直し（リビルド）」の像生成にあたり、open() の検証
//! カスケード（Section 7）が無効と判定したときに走る「破棄して再構築」の中身
//! でもある。fingerprint（Section 7 step 3）はここで埋めた `source_cd_hash` と
//! stat 値を後続の open() が照合する。
//!
//! このレイヤは純メモリ変換に閉じる:
//! - **mmap した archive バイト列を入力に取り、vmidx 像バイト列を返す**だけで、
//!   ファイル I/O（vmidx.tmp 書き出し → rename、Section 6.3 a/b）はしない。
//!   呼び出し側（マウント層）が像を受け取って書き出す。
//! - **チェックポイントは生成しない**。チェックポイントはセッション中の read で
//!   蓄積される追記情報で、初回ビルド時点では存在しない。各エントリは
//!   `chunk_head_offset = 0`（チャンク無し）で入る。
//! - **`flags` に `VMM_GENERATED` を立てない**。これは「アーカイブが最後に VMM に
//!   よって完全に書き直された」来歴フラグで、既存アーカイブへの索引付けは
//!   それに当たらない。DEFLATE_VMM への昇格も構造的書き直し時に決まるため、
//!   ここではメソッドコード由来の [`ProviderType`](crate::vmidx::ProviderType)
//!   （標準 DEFLATE は `Deflate`）をそのまま記録する。

use crate::archive::{Archive, CdEntry, ZipError};
use crate::provider::{builtin_provider, ProviderError};
use crate::vmidx::{hash_cd_block, EntryRecord, VmidxBuilder, CD_HASH_SIZE};
use std::fmt;

/// vmidx 構築の入力パラメータ。`source_*` は archive.zip の現在の stat 値
/// （fingerprint のうち size/inode/mtime に入る）で、`page_size` /
/// `checkpoint_interval` はマウントが決めるレイアウトパラメータ。`cd_hash` は
/// 構築時に archive の [`Archive::cd_block`] から算出するので入力に含めない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildParams {
    pub source_file_size: u64,
    pub source_inode: u64,
    pub source_mtime_ns: u64,
    pub page_size: u32,
    /// 展開バイト単位のチェックポイント基準間隔（ヘッダに記録するのみ。
    /// ビルド時点ではチェックポイントは生成しない）。
    pub checkpoint_interval: u64,
}

impl Default for BuildParams {
    fn default() -> BuildParams {
        // page_size / checkpoint_interval は VmidxBuilder の既定と揃える。
        BuildParams {
            source_file_size: 0,
            source_inode: 0,
            source_mtime_ns: 0,
            page_size: 4096,
            checkpoint_interval: 1_048_576,
        }
    }
}

/// vmidx 構築の失敗。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildError {
    /// ローカルヘッダ解決など、archive の parse 段で失敗した。
    Zip(ZipError),
    /// エントリ名が UTF-8 でない。NAME HEAP は UTF-8 を前提とし、ルックアップも
    /// `&str` で行うため、現状は非 UTF-8 名を索引化できない（既知の制約）。
    NonUtf8Name(Vec<u8>),
    /// EAGER 索引でチェックポイント生成（プロバイダの解凍走査）に失敗した。
    Provider(ProviderError),
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BuildError::Zip(e) => write!(f, "index build: {e}"),
            BuildError::NonUtf8Name(name) => {
                write!(f, "index build: non-UTF-8 entry name ({} bytes)", name.len())
            }
            BuildError::Provider(e) => write!(f, "index build: {e}"),
        }
    }
}

impl std::error::Error for BuildError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            BuildError::Zip(e) => Some(e),
            BuildError::NonUtf8Name(_) => None,
            BuildError::Provider(e) => Some(e),
        }
    }
}

impl From<ZipError> for BuildError {
    fn from(e: ZipError) -> BuildError {
        BuildError::Zip(e)
    }
}

/// archive の Central Directory から vmidx 像を組み立てて返す（チェックポイント
/// 無し＝メタデータのみ）。チェックポイントをこの場で生成しないケース
/// （構造的書き直し Section 6.3、LAZY の初期像など）に使う。EAGER 索引は
/// [`build_vmidx_eager`]。
///
/// 各 CD エントリにつき、メソッド・サイズ・`local_header_offset` をそのまま写し、
/// 圧縮データ先頭は [`Archive::data_offset`]（必ずローカルヘッダを読む）で確定する。
pub fn build_vmidx_image(archive: &Archive, params: &BuildParams) -> Result<Vec<u8>, BuildError> {
    let mut builder = new_builder(archive, params);
    for entry in archive.entries() {
        let (name, record) = map_record(archive, entry)?;
        builder.push(name, record, Vec::new());
    }
    Ok(builder.serialize())
}

/// EAGER 索引（設計 FIRST-OPEN の EAGER 戦略、IMPLEMENTATION_NOTES の M1）。
/// 各エントリの圧縮ストリームを [`provider`](crate::provider) で走査して
/// チェックポイントを生成し、像に含めて返す。プロバイダを持たない種別
/// （STORE / 未対応）はチェックポイント無しで入る。
///
/// 1 エントリでもチェックポイント生成に失敗すると索引構築全体を
/// [`BuildError::Provider`] で失敗させる（壊れエントリの単離＝当該のみ
/// UNSUPPORTED 化は後段の改良）。
pub fn build_vmidx_eager(archive: &Archive, params: &BuildParams) -> Result<Vec<u8>, BuildError> {
    let mut builder = new_builder(archive, params);
    for entry in archive.entries() {
        let (name, record) = map_record(archive, entry)?;
        let checkpoints = match builtin_provider(entry.provider_type) {
            Some(provider) => {
                let compressed = archive.entry_data(entry)?;
                provider
                    .build_checkpoints(compressed, entry.uncompressed_size, params.checkpoint_interval)
                    .map_err(BuildError::Provider)?
            }
            None => Vec::new(),
        };
        builder.push(name, record, checkpoints);
    }
    Ok(builder.serialize())
}

/// fingerprint とレイアウトパラメータを設定した空のビルダ。
fn new_builder(archive: &Archive, params: &BuildParams) -> VmidxBuilder {
    let mut builder = VmidxBuilder::new();
    builder.flags = 0; // VMM_GENERATED は立てない（モジュールコメント参照）。
    builder.page_size = params.page_size;
    builder.checkpoint_interval = params.checkpoint_interval;
    builder.source_file_size = params.source_file_size;
    builder.source_inode = params.source_inode;
    builder.source_mtime_ns = params.source_mtime_ns;
    builder.source_cd_hash[..CD_HASH_SIZE].copy_from_slice(&hash_cd_block(archive.cd_block()));
    builder
}

/// 1 つの CD エントリを (name, EntryRecord) に写す（チェックポイント以外）。
/// `name_hash` / `name_offset` / `name_len` / `chunk_head_offset` は
/// [`VmidxBuilder::serialize`] がレイアウト確定時に設定するので 0 のまま。
fn map_record(archive: &Archive, entry: &CdEntry) -> Result<(String, EntryRecord), BuildError> {
    let name = std::str::from_utf8(&entry.name)
        .map_err(|_| BuildError::NonUtf8Name(entry.name.clone()))?
        .to_owned();
    let data_offset = archive.data_offset(entry)?;
    let record = EntryRecord {
        name_hash: 0,
        name_offset: 0,
        name_len: 0,
        chunk_head_offset: 0,
        provider_type: entry.provider_type,
        entry_flags: 0,
        method_code: entry.method_code,
        local_header_offset: entry.local_header_offset,
        data_offset,
        compressed_size: entry.compressed_size,
        uncompressed_size: entry.uncompressed_size,
        checkpoint_count: 0,
        commit_count_for_entry: 0,
    };
    Ok((name, record))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vmidx::{flags, FingerprintVerdict, ProviderType, SourceStat, Vmidx};

    /// STORE エントリだけからなる最小 ZIP を手組みする（archive.rs のテスト
    /// フィクスチャと同じ構造。ここでは index_build のために独立に持つ）。
    struct StoreZip {
        body: Vec<u8>,
        cd: Vec<u8>,
        count: u16,
    }

    impl StoreZip {
        fn new() -> StoreZip {
            StoreZip {
                body: Vec::new(),
                cd: Vec::new(),
                count: 0,
            }
        }

        /// 名前バイトを直接受け取り、STORE エントリを 1 件足す（非 UTF-8 名の
        /// テストのため `&[u8]`）。
        fn add(&mut self, name: &[u8], data: &[u8]) -> &mut Self {
            let lho = self.body.len() as u32;
            // ローカルファイルヘッダ。
            push_u32(&mut self.body, 0x0403_4b50);
            for _ in 0..2 {
                push_u16(&mut self.body, 0);
            } // version / flags
            push_u16(&mut self.body, 0); // method = STORE
            push_u16(&mut self.body, 0); // mod time
            push_u16(&mut self.body, 0); // mod date
            push_u32(&mut self.body, 0); // crc32
            push_u32(&mut self.body, data.len() as u32);
            push_u32(&mut self.body, data.len() as u32);
            push_u16(&mut self.body, name.len() as u16);
            push_u16(&mut self.body, 0); // extra len
            self.body.extend_from_slice(name);
            self.body.extend_from_slice(data);

            // Central Directory ファイルヘッダ。
            push_u32(&mut self.cd, 0x0201_4b50);
            push_u16(&mut self.cd, 20); // version made by
            push_u16(&mut self.cd, 20); // version needed
            push_u16(&mut self.cd, 0); // flags
            push_u16(&mut self.cd, 0); // method = STORE
            push_u16(&mut self.cd, 0); // mod time
            push_u16(&mut self.cd, 0); // mod date
            push_u32(&mut self.cd, 0); // crc32
            push_u32(&mut self.cd, data.len() as u32);
            push_u32(&mut self.cd, data.len() as u32);
            push_u16(&mut self.cd, name.len() as u16);
            push_u16(&mut self.cd, 0); // extra len
            push_u16(&mut self.cd, 0); // comment len
            push_u16(&mut self.cd, 0); // disk start
            push_u16(&mut self.cd, 0); // internal attrs
            push_u32(&mut self.cd, 0); // external attrs
            push_u32(&mut self.cd, lho);
            self.cd.extend_from_slice(name);
            self.count += 1;
            self
        }

        fn finish(mut self) -> Vec<u8> {
            let cd_offset = self.body.len() as u32;
            let cd_size = self.cd.len() as u32;
            self.body.extend_from_slice(&self.cd);
            push_u32(&mut self.body, 0x0605_4b50); // EOCD
            push_u16(&mut self.body, 0); // disk
            push_u16(&mut self.body, 0); // cd start disk
            push_u16(&mut self.body, self.count); // entries this disk
            push_u16(&mut self.body, self.count); // total entries
            push_u32(&mut self.body, cd_size);
            push_u32(&mut self.body, cd_offset);
            push_u16(&mut self.body, 0); // comment len
            self.body
        }
    }

    fn push_u16(v: &mut Vec<u8>, x: u16) {
        v.extend_from_slice(&x.to_le_bytes());
    }
    fn push_u32(v: &mut Vec<u8>, x: u32) {
        v.extend_from_slice(&x.to_le_bytes());
    }

    fn sample_zip() -> Vec<u8> {
        let mut z = StoreZip::new();
        z.add(b"hello.txt", b"hi");
        z.add(b"dir/second.bin", b"payload");
        z.finish()
    }

    #[test]
    fn builds_parseable_image_with_every_entry() {
        let zip = sample_zip();
        let ar = Archive::parse(&zip).expect("zip parses");
        let image = build_vmidx_image(&ar, &BuildParams::default()).expect("build ok");

        let v = Vmidx::parse(&image).expect("vmidx parses");
        assert_eq!(v.entry_count(), 2);
        // VMM_GENERATED は立っていない（既存アーカイブへの索引付け）。
        assert_eq!(v.header().flags & flags::VMM_GENERATED, 0);
    }

    #[test]
    fn entry_fields_carry_over_and_data_offset_resolved() {
        let zip = sample_zip();
        let ar = Archive::parse(&zip).expect("zip parses");
        let image = build_vmidx_image(&ar, &BuildParams::default()).expect("build ok");
        let v = Vmidx::parse(&image).expect("vmidx parses");

        // CD 由来の値が ENTRY RECORD に写っていること。data_offset は archive 側の
        // 算出値と一致すること（LFH を読んだ確定値）。
        for cd in ar.entries() {
            let name = std::str::from_utf8(&cd.name).unwrap();
            let (_, rec) = v.lookup(name).expect("lookup ok").expect("entry present");
            assert_eq!(rec.provider_type, ProviderType::Store);
            assert_eq!(rec.method_code, cd.method_code);
            assert_eq!(rec.local_header_offset, cd.local_header_offset);
            assert_eq!(rec.compressed_size, cd.compressed_size);
            assert_eq!(rec.uncompressed_size, cd.uncompressed_size);
            assert_eq!(rec.data_offset, ar.data_offset(cd).unwrap());
            // STORE はチェックポイント無し。
            assert_eq!(rec.chunk_head_offset, 0);
            assert_eq!(rec.checkpoint_count, 0);
        }
    }

    #[test]
    fn fingerprint_validates_against_same_archive() {
        let zip = sample_zip();
        let ar = Archive::parse(&zip).expect("zip parses");
        let params = BuildParams {
            source_file_size: 4096,
            source_inode: 7,
            source_mtime_ns: 111,
            ..BuildParams::default()
        };
        let image = build_vmidx_image(&ar, &params).expect("build ok");
        let v = Vmidx::parse(&image).expect("vmidx parses");

        // 同じアーカイブ・同じ stat なら Valid。
        let live = SourceStat {
            file_size: 4096,
            inode: 7,
            mtime_ns: 111,
            cd_hash: hash_cd_block(ar.cd_block()),
        };
        assert_eq!(v.check_fingerprint(&live), FingerprintVerdict::Valid);

        // CD が変わった（別アーカイブ）なら Invalid。
        let other = SourceStat {
            cd_hash: hash_cd_block(b"different central directory"),
            ..live
        };
        assert_eq!(v.check_fingerprint(&other), FingerprintVerdict::Invalid);
    }

    #[test]
    fn header_records_layout_params() {
        let zip = sample_zip();
        let ar = Archive::parse(&zip).expect("zip parses");
        let params = BuildParams {
            page_size: 8192,
            checkpoint_interval: 2 << 20,
            ..BuildParams::default()
        };
        let image = build_vmidx_image(&ar, &params).expect("build ok");
        let v = Vmidx::parse(&image).expect("vmidx parses");
        assert_eq!(v.header().page_size, 8192);
        assert_eq!(v.header().checkpoint_interval, 2 << 20);
    }

    #[test]
    fn empty_archive_builds_empty_index() {
        let zip = StoreZip::new().finish();
        let ar = Archive::parse(&zip).expect("empty zip parses");
        let image = build_vmidx_image(&ar, &BuildParams::default()).expect("build ok");
        let v = Vmidx::parse(&image).expect("vmidx parses");
        assert_eq!(v.entry_count(), 0);
    }

    #[test]
    fn non_utf8_name_is_rejected() {
        let mut z = StoreZip::new();
        z.add(&[0xff, 0xfe, 0x00], b"x"); // 不正な UTF-8 シーケンス
        let zip = z.finish();
        let ar = Archive::parse(&zip).expect("zip parses");
        match build_vmidx_image(&ar, &BuildParams::default()) {
            Err(BuildError::NonUtf8Name(name)) => assert_eq!(name, vec![0xff, 0xfe, 0x00]),
            other => panic!("expected NonUtf8Name, got {other:?}"),
        }
    }
}
