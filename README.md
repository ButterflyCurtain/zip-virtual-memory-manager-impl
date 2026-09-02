# ZIP Virtual Memory Manager — Implementation

**English** | [日本語](#japanese)

Implementation of the "ZIP Virtual Memory Manager": treating a ZIP archive as a
backing store for virtual memory, with random read/write access into compressed
entries, copy-on-write buffering, and crash-safe commits.

The design specifications this implementation is based on live in a separate
repository:

- **Design documents:** https://github.com/ButterflyCurtain/zip-virtual-memory-manager

## Status

Work in progress, but the read path, the write path and crash recovery all
work end to end against real files. 215 tests pass on Windows and Linux.

| Milestone | State |
| --- | --- |
| M1 — read core (mmap, seek index, page cache, DEFLATE resume) | done |
| M2 — minimal write (Diff Layer Tier 1, FULL commit) | done |
| M3 — durability (spill to `vmdirty`, CRC-32C journal, crash recovery) | done |
| M4 — append-only INCREMENTAL commit | in progress (dedup remaining) |

Not implemented yet: the `DEFLATE_VMM` and Zstd providers, Zip64 output, and
the dead-space freelist. Deliberate deviations from the specification are
recorded in [`docs/SPEC_DIVERGENCE.md`](docs/SPEC_DIVERGENCE.md); the design
decisions behind the implementation are in
[`docs/DECISIONS.md`](docs/DECISIONS.md).

---

<a id="japanese"></a>

# ZIP Virtual Memory Manager — 実装

ZIP アーカイブを仮想メモリのバッキングストアとして扱う
「ZIP Virtual Memory Manager」の実装です。
圧縮されたエントリへのランダム読み書き、copy-on-write のバッファリング、
クラッシュセーフな commit あたりを扱います。

この実装が基づく設計仕様は、別リポジトリにあります:

- **設計ドキュメント:** https://github.com/ButterflyCurtain/zip-virtual-memory-manager

## 状況

作りかけですが、読み取り・書き込み・クラッシュ回復までは実ファイル相手に
end-to-end で動きます。テストは Windows / Linux の両方で 215 件通っています。

| マイルストーン | 状態 |
| --- | --- |
| M1 — 読み取りコア（mmap、シーク索引、ページキャッシュ、DEFLATE 中途再開） | 完了 |
| M2 — 最小書き込み（Diff Layer Tier 1、FULL commit） | 完了 |
| M3 — 耐久化（`vmdirty` へのスピル、CRC-32C ジャーナル、クラッシュ回復） | 完了 |
| M4 — append-only な INCREMENTAL commit | 進行中（dedup が残り） |

未実装は `DEFLATE_VMM` / Zstd プロバイダ、Zip64 出力、Dead Space Freelist です。
仕様から意図的に外した点は [`docs/SPEC_DIVERGENCE.md`](docs/SPEC_DIVERGENCE.md)、
実装上の設計判断は [`docs/DECISIONS.md`](docs/DECISIONS.md) に記録しています。
