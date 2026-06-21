//! ZIP Virtual Memory Manager
//!
//! ZIP アーカイブを仮想メモリのバッキングストアとして扱う実装。
//! 設計仕様は別リポジトリを参照:
//! <https://github.com/ButterflyCurtain/zip-virtual-memory-manager>
//!
//! モジュール構成は設計の章立てに対応する。現状の実装範囲は M1〜M4 まで:
//! 読み取り経路 (`archive` / `vmidx` / `provider` / `page` / `mount` /
//! `index_build`)、Diff Layer (`difflayer`) と Tier 2 spill / vmdirty
//! durability (`tier2` / `vmdirty`)、エントリ操作 (`entrytable`)、
//! ディスク I/O とマウントライフサイクル (`disk`)、FULL/INCREMENTAL
//! commit (`commit`) が緑で揃っている。
//!
//! まだスタブなのは `lock` のみ (Concurrent Access spec の vmlock 層;
//! 現状の実装は単一プロセス前提)。実装の進捗詳細・設計差分・未実装項目
//! は `docs/HANDOFF.md`、`docs/SPEC_DIVERGENCE.md`、`docs/DECISIONS.md`
//! にまとまっている。

pub mod archive;
pub mod commit;
pub mod difflayer;
pub mod disk;
pub mod entrytable;
pub mod index_build;
pub mod lock;
pub mod mount;
pub mod page;
pub mod provider;
pub mod tier2;
pub mod vmdirty;
pub mod vmidx;
