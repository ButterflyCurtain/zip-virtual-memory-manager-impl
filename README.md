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

### Know these before you use it

- **There is no concurrency control.** The `vmlock` layer — 742 lines of the
  Concurrent Access specification — is not implemented; `src/lock.rs` is a stub.
  Opening the same archive from more than one process is undefined behaviour.
  Within a process, callers must serialise access themselves.
- **Archives larger than 4 GB cannot be written back.** Reading is ZIP64-clean,
  but commit has no ZIP64 output and returns `CommitError::TooLarge` once any
  offset, size or entry count exceeds 32 bits.
- **There are no defences against hostile archives.** No limits on entry count,
  name length, declared size or compression ratio, and no detection of
  overlapping regions or duplicate names. Validate untrusted input before
  handing it to this crate.
- **A FULL commit does not preserve per-entry metadata.** Timestamps,
  permissions, entry comments, extra fields and the UTF-8 name flag are dropped
  when the archive is rewritten. "Verbatim" applies to the compressed stream,
  not to the directory record.

### Not implemented yet

The `DEFLATE_VMM` and Zstd providers, background commit, and the `sidecar_dir`
option with its registry. The dead-space freelist is **not** on this list: it
was deliberately rejected in favour of append-only INCREMENTAL commits plus a
separate FULL compaction.

Deliberate deviations from the specification are recorded in
[`docs/SPEC_DIVERGENCE.md`](docs/SPEC_DIVERGENCE.md); the reasoning behind the
implementation — including decisions that were later corrected — is in
[`docs/DECISIONS.md`](docs/DECISIONS.md).

## About this project

Built through agentic development with Claude Code. AI assistance was used for
implementation, tests, review and documentation.
Design and implementation decisions @ButterflyCurtain

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

### 使う前に知っておいてほしいこと

- **並行アクセス制御がありません。** `vmlock` 層（Concurrent Access 仕様の 742 行）
  は未実装で、`src/lock.rs` はスタブです。**同じアーカイブを複数プロセスから開くのは
  未定義動作**です。プロセス内でも、呼び出し側でシリアライズする必要があります。
- **4 GB を超えるアーカイブは書き戻せません。** 読み取りは ZIP64 対応済みですが、
  commit は ZIP64 出力を持たず、オフセット・サイズ・件数のいずれかが 32 ビットを
  超えると `CommitError::TooLarge` を返します。
- **悪意あるアーカイブへの防御がありません。** エントリ数・名前長・申告サイズ・
  圧縮率の上限も、領域の重なりや名前重複の検出もありません。信頼できない入力は
  このクレートに渡す前に検証してください。
- **FULL commit はエントリごとのメタデータを保存しません。** アーカイブを書き直す際、
  タイムスタンプ・パーミッション・エントリコメント・extra field・UTF-8 名フラグが
  失われます。「verbatim」なのは圧縮ストリームであって、ディレクトリレコードでは
  ありません。

### 未実装

`DEFLATE_VMM` / Zstd プロバイダ、background commit、`sidecar_dir` オプションと
そのレジストリ。Dead Space Freelist は**この一覧に入りません** —— append-only な
INCREMENTAL commit と別機構の FULL compaction を採る判断をして、意図的に外しました。

仕様から意図的に外した点は [`docs/SPEC_DIVERGENCE.md`](docs/SPEC_DIVERGENCE.md)、
実装上の判断は —— 後から訂正したものも含めて ——
[`docs/DECISIONS.md`](docs/DECISIONS.md) に記録しています。

## 制作について

Claude Code によるエージェント型の開発。AI 支援を利用（実装・テスト・レビュー・文書整備）。
設計・実装判断 @ButterflyCurtain
