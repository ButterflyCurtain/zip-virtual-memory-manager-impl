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

## 3. 回復 Section 3「自動解決しない」宣言と決定木の食い違い（**解決済み**）

> **2026-09-02: この差分は解消した。** 設計リポの Section 3 前文は現在
> 「The VMM auto-resolves only the two unambiguous branches: a stale empty
> journal is silently discarded, and a clean commit boundary … For every branch
> that could lose data, the VMM does not auto-resolve」と読める。実装の
> `DefaultRecoveryHandler` の挙動とちょうど一致するので、もはや差分ではない。
> 番号は他所から参照されるため欠番にせず残す。以下は当時の記録。

- **設計**: `vmdirty_Journal_Spec` の回復 Section 3 は冒頭で「VMM は自動解決しない」と宣言する一方、
  決定木自身が 1 枝に `auto: recover_committed (safe default)`、別枝に `silently discard` を許す。
- **実装**: 素直に「**曖昧でない枝は自動・残りは呼び出し側へ委譲**」と解釈し、`DefaultRecoveryHandler`
  が曖昧でない枝のみ自動・データを失いうる枝は `Abort`（`RecoveryRequired`）とする。全枝制御は
  `open_with_recovery`。
- **理由 / ADR**: [ADR 0009](DECISIONS.md)。これは設計テキスト側の文言が自己矛盾している箇所で、
  公開リポでの文言明確化が候補（別 push）。実装の解釈は決定木の `auto`/`silently` 枝に一致する。

## 4. vmdirty の durability を O_DSYNC ではなく明示 `sync_data()` で表現

- **設計**: `vmdirty_Journal_Spec` Section 4.1「fsync policy」と Section 7「VmdirtyWriter」は
  「`vmdirty is opened with O_DSYNC`」と記述する。Sync モードでは各書き込みが復帰前に durable
  になることを `O_DSYNC` で保証する設計。
- **実装**: `O_DSYNC` を使わず、書き込みごとに明示 `File::sync_data()` を呼ぶ方式で同じ
  durability を実現する（`vmdirty.rs:587-628`、Sync モード）。Lazy モードは COMMIT MARKER の
  ときだけ `sync_data()`。
- **理由 / ADR**: [ADR 0007](DECISIONS.md)。Windows と Unix で `O_DSYNC` 相当を FFI 無しに
  統一するため。本リポの「純 Rust を基本に、必要な所だけ C へ」(ADR 0004) 方針に整合する。
- **耐久性に違いはない** (`sync_data()` ≒ `fdatasync()`)が、性質が異なる:
  `O_DSYNC` は OS 強制 (どの書き込み経路も自動同期)、明示 `sync_data()` は呼び忘れたら静かに
  保証が崩れる規律ベース。**現在の VmdirtyWriter は単一の `appendRecord` / `appendCommitMarker`
  経由に閉じているため呼び忘れリスクは低い**が、将来コードが増えるときは新しい書き込み経路に
  必ず `sync_data()` を入れる規律を維持する必要がある。
- **fsyncgate**: `sync_data()` の失敗は retryable ではない (IMPLEMENTATION_NOTES の
  「2018 fsyncgate」)。上位で ERROR 状態に倒す配線は M3 ③ 完了時点では未配線で、
  HANDOFF.md の「未了」に残っている。

---

## 5. `flush()` は Tier 1 からページを退避しない（durability だけを担う）

- **設計**: `vmdirty_Journal_Spec` 4.2 の `flush()` 手順は
  「a. Spill all Tier 1 pages that exceed dirty_limit / b. Spill all remaining
  Tier 1 pages」。4.1 が「A DATA RECORD is appended exactly when a dirty page is
  **evicted** from Tier 1 to Tier 2」と定めているので、ここでの spill は
  **Tier 1 から外すこと**を含む。`flush()` を終えた Tier 1 は空になる。
- **実装**: 常駐ページを全て vmdirty へ書き COMMIT MARKER を打つが、
  **Tier 1 からは外さない**（`tier2.rs` の `flush`）。`dirty_current` は変わらない。
- **理由**: `flush()` を「メモリ圧の解放」ではなく「**耐久性のバリア**」に純化した。
  外さない利点は 2 つ: flush 後の読みが Tier 1 ヒットのまま速い（vmdirty からの
  pread に落ちない）、commit 時の rehydrate が要らない。メモリ圧の解放は
  `dirty_limit` 超過時の spill が担当し、責務が分かれる。
- **影響（把握しておくこと）**: `flush()` は**メモリ使用量を一切下げない**。
  また繰り返し呼ぶと**毎回すべての常駐ページを書き直す**ので、ジャーナルが線形に
  伸びる（仕様の意味論なら 2 回目の flush は Tier 1 が空なので何も書かない）。
  伸びた分は dead になり、`compact_journal()`（`should_compact_journal()` が
  dead > live で真）が回収する。
- **ADR**: 無し。本書が一次記録。

## 6. Windows の file identity を `0` 固定にする（仕様は volume serial + file index を指定）

- **設計**: `PLATFORM ASSUMPTIONS` は Windows について「volume serial number plus
  file index from `GetFileInformationByHandle` in place of `st_ino`」と明示する。
- **実装**: `disk.rs` の `file_id` は Unix で `st_ino`、それ以外は **`0` 固定**。
  `std::os::windows::fs::MetadataExt::file_index` が nightly 限定のため。
- **影響**: Windows では指紋の FAST 判定と、vmdirty provenance の
  `header.source_inode == st_ino` が**実質無効化**される。`cd_hash` と `file_size`
  だけが実効的な関門になる。ADR 0017 の commit intent が `inode` を載せないのは、
  この非対称を持ち込まないため。
- **ADR**: [ADR 0005](DECISIONS.md) 備考。安定化 or FFI 追加のいずれかで解消できる。

## 7. 未対応の圧縮種別でも rename でき、FULL commit を通る

- **設計**: 未対応メソッドのエントリは "unavailable" とされ、rename できるとも
  FULL commit で保存されるとも書かれていない。
- **実装**: rename ターゲットが未 dirty なら、ソースの**圧縮ストリームを verbatim
  コピー**して現在名で出力する（`commit.rs` の別名ループ）。プロバイダを必要としない
  ので、BZIP2 等のエントリも改名でき、compaction を生き延びる。
- **理由 / ADR**: [ADR 0010](DECISIONS.md) ④b。「読めない」ことと「触れない」ことを
  分けた。データを再材料化しない操作（改名・そのままの再配置）に解凍は要らない。
- **影響**: 仕様だけを読むと `UNSUPPORTED_COMPRESSION` がこれらを塞ぐと読める。
  実装は塞がない（意図的な拡張）。

## 8. commit 系 API はマウントを消費する

- **設計**: `commit()` は「On success: deletes vmdirty; updates vmidx」とし、
  マウントが生き続ける前提で書かれている。BACKGROUND COMMIT は
  「The caller continues writing without interruption」とまで言う。
- **実装**: `commit` / `commit_full` / `commit_incremental` / `compact` は
  すべて `self` を取る。呼び出し側は commit 後に開き直す。
- **理由**: Windows では**マップ中のファイルを切り詰め・上書きできない**ため、
  アーカイブを変更する前に mmap を解放する必要がある（下記 9 と同根）。所有権で
  それを型に落とすと、解放し忘れが起こらない。
- **影響**: BACKGROUND COMMIT（未実装）を入れるときは、この形と両立する設計が要る。
- **ADR**: 無し。本書が一次記録。

## 9. FULL commit のスクラッチ名は `archive.zip.new`（仕様は `archive.new.zip`）

- **設計**: `SIDECAR FILES` は `archive.new.zip` と明記し、orphan 掃除規則も
  その名前で書かれている。
- **実装**: `commit_tmp` はファイル名全体に `.new` を足すので `archive.zip.new`。
- **理由**: 拡張子を差し替える規則は**拡張子の無い名前に対して未定義**（`archive`
  → `archive.new`? `archive` の前に何を入れる?）。末尾に足す規則は全ての名前で
  一意に決まり、`vmdirty.compact` の付け方とも揃う。
- **影響**: 実害は無いが、**仕様が名指ししているサイドカーパスなので放置は良くない**。
  実装を仕様に合わせるか、仕様を全域的な規則へ直すかの判断が要る（後者を推す）。
- **ADR**: 無し。本書が一次記録。

## 10. `commit.intent` の version 1 レコードは読まない

- **実装**: version 2（ADR 0017）以外の INTENT は「無い」ものとして扱う。
  version 1（ADR 0012 の 20 バイト、`old_len`/`new_len` のみ）で書かれた
  クラッシュ窓のファイルは、巻き戻しに使われない。
- **理由**: クレートは未公開（`publish = false`）で、INTENT はクラッシュ窓にしか
  存在しない一時ファイル。on-disk 互換を保つ価値より、判定を 1 本にする価値を採った。
- **影響**: **旧バイナリで中断した commit を新バイナリで開くと、巻き戻しが行われない。**
  同一バージョン内でのみ安全。公開版を出すときはこの割り切りを見直すこと。
- **ADR**: [ADR 0017](DECISIONS.md)。
