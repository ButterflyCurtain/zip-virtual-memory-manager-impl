//! ZIP Virtual Memory Manager
//!
//! ZIP アーカイブを仮想メモリのバッキングストアとして扱う実装。
//! 設計仕様は別リポジトリを参照:
//! <https://github.com/ButterflyCurtain/zip-virtual-memory-manager>
//!
//! モジュール構成は設計の章立てに対応する。現状は `vmidx` のみ実装、
//! 他は役割を示すスタブ。

pub mod archive;
pub mod commit;
pub mod difflayer;
pub mod disk;
pub mod index_build;
pub mod lock;
pub mod mount;
pub mod page;
pub mod provider;
pub mod tier2;
pub mod vmdirty;
pub mod vmidx;
