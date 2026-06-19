# 設計仕様との差分

実装が公開設計仕様（別リポジトリ `zip-virtual-memory-manager` の `docs/*.txt`）と
食い違う点を、**理由と決定ログ（ADR）へのリンク付き**で一覧化する。

方針: 差分が実装側の明確な ADR 決定に基づく場合、**設計リポを書き換えるのではなく
ここに記録する**。設計テキストは「あるべき仕様」、本書は「実装がどこをどう変えたか」を
表す。各項目は ADR を一次情報とし、本書はその索引として機能する。

> 注: 「まだ実装していないだけ（仕様どおりにする予定）」のものは差分ではない。それらは
> [`HANDOFF.md`](HANDOFF.md) の「未実装」や各 ADR の「未了」に置く。本書は**意図的に
> 仕様と異なる**箇所だけを扱う。

> 整合済み（差分ではない・再追加しないこと）: commit / compact 系 API は設計の
> WRITE STRATEGY SELECTION / vmdirty Spec §7 に**名前ごと合わせた**（ADR 0013）。
> `FileMount::commit()` = bloat 閾値で自動選択（spec `commit()`）、`commit_full()` /
> `commit_incremental()` = 明示プリミティブ（spec `commit_strategy=FULL` / `force_compact`
> 相当）、`compact()` = CLEAN からのアーカイブ FULL compaction（spec `compact()`）、
> `compact_journal()` = vmdirty ジャーナル compaction（spec `compactJournal()`、内部保守）。

---

## 1. Dead Space Freelist / in-place 穴再利用を実装しない

- **設計**: `ZIP_Virtual_Memory_Manager.txt` の INCREMENTAL commit 手順は、変更エントリを
  まず **Dead Space Freelist の穴**（`≥ sizeof(LFH) + recompressed_size`）に in-place で
  書き戻し、入らなければ EOF へ追記する、と規定する。remove は "data region becomes dead
  space, reclaimed via the Dead Space Freelist at next open()" とする。
- **実装**: 穴再利用・freelist・近傍シャッフルを**一切持たない**。INCREMENTAL は常に EOF へ
  純追記し、dead は「次の FULL compaction で捨てるだけの未追跡バイト」として中間に残す
  （再利用する free block ではない）。空間回収は FULL compaction のみが行う。
- **理由 / ADR**: [ADR 0011](DECISIONS.md)（in-place 穴再利用は本アーキの不変条件「live を
  in-place で壊さない」と矛盾するため不採用。mmap スナップショット / ESTALE / vmidx fingerprint /
  rename 原子性の 4 本柱と衝突する 5 点を整理）、[ADR 0012](DECISIONS.md)（append-only INCREMENTAL の
  追記レイアウトと truncate ロールバック）。特許の整理は [`PRIOR_ART.md`](PRIOR_ART.md)。

## 2. bloat メトリクスは LFH バイトを live に数えない

- **設計**: `bloat_bytes = file_size − Σ(compressed_size) − cd_size − eocd_size`。
- **実装**: 同一式をそのまま採用（= LFH のバイトは live から漏れ、近似的に bloat 側へ含む）。
  これは設計式に**忠実**であり差分ではないが、「最小アーカイブでも bloat_ratio が厳密な 1.0 に
  ならない（LFH 分だけ上回る）」という直感に反する挙動の出所なので、誤読防止に記録する。
- **ADR**: [ADR 0013](DECISIONS.md)。LFH 長は読まないと分からず、設計の "no additional I/O" を
  守るための意図的な近似。既定閾値 2.0 には届かない小ささ。

## 3. 回復 Section 3「自動解決しない」宣言と決定木の食い違い

- **設計**: `vmdirty_Journal_Spec` の回復 Section 3 は冒頭で「VMM は自動解決しない」と宣言する一方、
  決定木自身が 1 枝に `auto: recover_committed (safe default)`、別枝に `silently discard` を許す。
- **実装**: 素直に「**曖昧でない枝は自動・残りは呼び出し側へ委譲**」と解釈し、`DefaultRecoveryHandler`
  が曖昧でない枝のみ自動・データを失いうる枝は `Abort`（`RecoveryRequired`）とする。全枝制御は
  `open_with_recovery`。
- **理由 / ADR**: [ADR 0009](DECISIONS.md)。これは設計テキスト側の文言が自己矛盾している箇所で、
  公開リポでの文言明確化が候補（別 push）。実装の解釈は決定木の `auto`/`silently` 枝に一致する。
