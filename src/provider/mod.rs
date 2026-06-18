//! 圧縮プロバイダ層（設計: 本体 `ZIP_Virtual_Memory_Manager` の
//! CompressionProvider interface）。
//!
//! 圧縮メソッド固有のシーク挙動・チェックポイント形式・デコード手順を
//! [`CompressionProvider`] の背後に閉じ込め、vmidx や VMM コアを特定の
//! アルゴリズムに結合させない。設計のインターフェースとの対応:
//!
//! | 設計 | 本実装 |
//! |---|---|
//! | `build_index(stream, size)` | [`CompressionProvider::build_checkpoints`] |
//! | `restore_state` + `decompress_page` | [`CompressionProvider::read_range`] |
//!
//! 設計は `restore_state`（チェックポイントから生のデコーダ文脈を復元）と
//! `decompress_page`（skip して 1 ページ返す）を分け、デコーダ状態を呼び出し側
//! （ページキャッシュのプリフェッチ）で持ち回らせる。本実装は当面、状態の
//! 持ち回りを必要としない [`read_range`](CompressionProvider::read_range)
//! （最近チェックポイントを起点に展開後オフセット範囲を返す）に畳む。状態を
//! 跨いで保持するプリフェッチ最適化は page キャッシュ層を実装する段で足す。
//!
//! このモジュールは `archive` / `vmidx` に依存しない。入力は「エントリの圧縮
//! バイト列」と（あれば）[`Checkpoint`]、出力は展開バイト列で、archive から
//! 圧縮データを取り出し vmidx から最近チェックポイントを引く配線は読み取り層
//! （マウント）の責務。

mod store;

pub use store::StoreProvider;

use crate::vmidx::{Checkpoint, ProviderType};
use std::fmt;

/// 圧縮プロバイダのデコード・索引構築失敗。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderError {
    /// このプロバイダが扱えない種別（[`ProviderType::Unsupported`] など）。
    Unsupported(ProviderType),
    /// 要求した展開オフセット範囲がエントリの `uncompressed_size` を超える。
    OutOfRange {
        offset: u64,
        len: u64,
        uncompressed_size: u64,
    },
    /// 渡されたチェックポイントのプロバイダ種別がこのプロバイダと一致しない。
    CheckpointMismatch {
        expected: ProviderType,
        found: ProviderType,
    },
    /// 圧縮ストリームの展開に失敗した（壊れたデータ・想定外の終端など）。
    CorruptStream(&'static str),
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProviderError::Unsupported(p) => write!(f, "provider: unsupported {p:?}"),
            ProviderError::OutOfRange {
                offset,
                len,
                uncompressed_size,
            } => write!(
                f,
                "provider: range [{offset}, {offset}+{len}) exceeds uncompressed size {uncompressed_size}"
            ),
            ProviderError::CheckpointMismatch { expected, found } => write!(
                f,
                "provider: checkpoint provider {found:?} does not match {expected:?}"
            ),
            ProviderError::CorruptStream(why) => write!(f, "provider: corrupt stream ({why})"),
        }
    }
}

impl std::error::Error for ProviderError {}

/// 圧縮メソッド固有のシーク・デコードを抽象化する。
pub trait CompressionProvider {
    /// このプロバイダが扱うエントリ種別。
    fn provider_type(&self) -> ProviderType;

    /// このプロバイダが対応する生の ZIP 圧縮メソッドコード。
    fn method_code(&self) -> u16;

    /// 圧縮ストリームを先頭から順次走査し、`interval`（展開バイト単位）ごとに
    /// チェックポイントを出して列を返す。索引構築（Section 5）時に 1 エントリ
    /// 1 回呼ぶ。チェックポイント形式を持たない STORE は空列を返す。
    fn build_checkpoints(
        &self,
        compressed: &[u8],
        uncompressed_size: u64,
        interval: u64,
    ) -> Result<Vec<Checkpoint>, ProviderError>;

    /// 展開ストリームの `[offset, offset + len)` を返す。`from` は
    /// `uncompressed_offset ≤ offset` の最近チェックポイント（無ければ `None` =
    /// 先頭から）。`from` の起点から `offset` まで前進デコードし、`len` バイトを
    /// 切り出す（読み取り手順: nearest checkpoint → 前進デコード）。
    fn read_range(
        &self,
        compressed: &[u8],
        from: Option<&Checkpoint>,
        offset: u64,
        len: usize,
        uncompressed_size: u64,
    ) -> Result<Vec<u8>, ProviderError>;
}

/// `ProviderType` に対応する組み込みプロバイダを返す。未対応種別は `None`。
///
/// 現状は STORE のみ。DEFLATE / DEFLATE_VMM / Zstd は解凍依存の追加とあわせて
/// 順次足す。
pub fn builtin_provider(pt: ProviderType) -> Option<Box<dyn CompressionProvider>> {
    match pt {
        ProviderType::Store => Some(Box::new(StoreProvider)),
        ProviderType::Deflate
        | ProviderType::DeflateVmm
        | ProviderType::Zstd
        | ProviderType::Unsupported => None,
    }
}

/// 展開範囲がエントリサイズ内に収まることを検査する共通ヘルパ。
pub(crate) fn check_range(
    offset: u64,
    len: usize,
    uncompressed_size: u64,
) -> Result<(), ProviderError> {
    let end = offset.checked_add(len as u64);
    match end {
        Some(end) if end <= uncompressed_size => Ok(()),
        _ => Err(ProviderError::OutOfRange {
            offset,
            len: len as u64,
            uncompressed_size,
        }),
    }
}
