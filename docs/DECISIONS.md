# 決定ログ

実装に関する決定を時系列で記録する。各項目は「決定 / 選択肢 / 理由 / 備考」で構成する。

---

## 0001. 実装言語に Rust を採用

- 日付: 2026-06-18
- ステータス: 採用
- 選択肢: Rust / Zig（参考: Java）

### 決定

実装言語を **Rust** とする。

### 理由

本プロジェクトはシステムプログラミング寄りで、設計上の難所が次に集中している。

- 並行アクセス（`LOCK_EX` ほか）下での状態管理
- COW のページバッファの所有権と寿命の管理
- クラッシュセーフな commit（journal / fsync / rename）と、mmap 領域の無効化（ESTALE）

Rust を選ぶ根拠は以下。

- 所有権・借用検査により、dirty ページの所有や mmap 領域の有効期間といった本設計の中心的な不変条件をコンパイル時に表現でき、データ競合をビルド時に検出できる。
- 必要なライブラリが揃っている（mmap: `memmap2`、圧縮: `zstd` / `flate2`、ハッシュ: `xxhash-rust`、CRC、低レベル syscall: `nix` / `libc`、バイナリ処理: `zerocopy` / `bytes` など）。
- `cargo` によるビルド・依存管理・テストの一貫した環境。

### 備考（Zig を見送った理由）

Zig も思想的には適合する（明示的アロケータ、`defer` / `errdefer`、syscall を隠さない設計、C 相互運用の容易さ）。一方で本時点では次の懸念があり、見送る。

- 言語が pre-1.0 で仕様・標準ライブラリが変動する。
- エコシステムが小さく、圧縮など一部は C ライブラリへのバインディングを自前で用意する場面が増える。
- 借用検査に相当する仕組みがなく、本設計の最難関である並行性・バッファのエイリアス安全性に対する静的な保証が Rust より弱い。

Java は技術的には可能だが（FFM API による mmap、`FileChannel.force` / `FileLock`）、mmap の扱いが間接的で、メモリマネージャ用途では GC が不利に働きやすいため、本プロジェクトでは優位性が薄い。

---

## 0002. CRC-32C 実装に crc32c クレートを採用

- 日付: 2026-06-18
- ステータス: 採用
- 選択肢: crc32c クレート / 自前実装

### 決定

vmidx の FILE HEADER・ENTRY RECORD・CHECKPOINT CHUNK の整合性検出に使う
CRC-32C（Castagnoli）の実装として **`crc32c` クレート**を用いる。

### 理由

- vmidx 仕様が整合性検出に CRC-32C を明示的に指定している。
- `crc32c` クレートは SSE4.2 のハードウェアアクセラレーションに対応し、
  未対応環境ではソフトウェア実装にフォールバックする。読み取りパスの
  ホットな検証で有利。
- 自前実装より検証済みで保守の手間が少ない。

### 備考

- fingerprint（`source_cd_hash`）と `name_hash` に必要な XXH3-128 / XXH3-64 は、
  該当処理を実装する段階で別途クレートを選定し、`0003` 以降で記録する。
- 圧縮（zstd / DEFLATE）のクレート選定も同様に、archive レイヤ着手時に記録する。

---

## 0003. XXH3 実装に xxhash-rust クレートを採用

- 日付: 2026-06-18
- ステータス: 採用
- 選択肢: xxhash-rust / twox-hash

### 決定

`name_hash`（XXH3-64）および将来の fingerprint `source_cd_hash`（XXH3-128）の
実装として **`xxhash-rust` クレート（`xxh3` フィーチャ）**を用いる。

### 理由

- vmidx 仕様が `name_hash` に XXH3-64、`source_cd_hash` に XXH3-128 を指定。
  両者を 1 クレートで賄える。
- `xxhash-rust` は純 Rust 実装で C 依存がなく、`const`/ワンショット関数
  （`xxh3_64` / `xxh3_128`）を提供しルックアップ経路から呼びやすい。
- フィーチャゲートで XXH3 のみを有効化でき、依存範囲を絞れる。

### 備考

- 現状は `name_hash` 用に `xxh3_64` のみを使用。`xxh3_128`（fingerprint）は
  open() の検証カスケードを実装する段階で使う。

---

## 0004. DEFLATE 解凍に libz-rs-sys（zlib-rs）を採用

- 日付: 2026-06-18
- ステータス: 採用
- 選択肢: libz-rs-sys（zlib-rs, 純Rust）/ libz-sys（C zlib）/ flate2（安全API）/ 自前 inflate

### 決定

標準 DEFLATE プロバイダの解凍に **`libz-rs-sys`（zlib-rs）** を用いる。

### 背景・理由

DEFLATE の任意地点からの再開（seek index の本体）は zlib の `zran.c` 方式が
枯れた正攻法で、3 つのプリミティブを要する:

- `inflateInit2(strm, -15)`（raw inflate、ZIP は zlib ヘッダ無し）
- `inflatePrime(bits, value)`（DEFLATE はビットストリームでブロックがバイト
  境界に揃わないため、再開地点の端数ビットを再注入する）
- `inflateSetDictionary(window 32KB)`（直前 32 KB の展開出力を辞書として復元）

`flate2` の安全 API はこのうち `prime` / `set_dictionary` を露出しておらず、
純 Rust の既存クレートに信頼できる中途再開の実装は無い。方針としては
「純 Rust を基本に、C の方が良い実装になる部分は C に振り切る」を採り、本件は
zlib 互換 API が必要と判断した。

採用した `libz-rs-sys` は zlib の C API を **純 Rust（zlib-rs）で**実装したもので、
`inflatePrime` / `inflateSetDictionary` / `inflateMark` まで揃い、zran のコードを
C zlib と同一に書ける。C ツールチェーン不要でどこでもビルドでき、メモリ安全。

### 備考（libz-sys を見送った理由）

- 本物の C zlib（`libz-sys`）も候補だったが、当環境（`x86_64-pc-windows-gnu`）の
  MSYS2 gcc で cc-rs のコンパイラ検出が落ちてビルドできず、MSVC ターゲットへの
  切り替えは過剰と判断した。`libz-rs-sys` が同一の zran 能力をより少ない摩擦で
  提供するため、現時点ではそちらを優先する。将来 C zlib 固有の最適化が要れば
  再評価する。
- `total_in` / `total_out` は zlib ABI の `c_ulong` で Windows では 32 ビット
  （4 GiB 超で wrap）。圧縮オフセットは自前で `in_pos - avail_in` から算出して
  回避している。
- unsafe FFI は `provider::deflate::RawInflater`（RAII で `inflateEnd`）に隔離する。

---

## 0005. ファイル mmap に memmap2 を採用

- 日付: 2026-06-18
- ステータス: 採用
- 選択肢: memmap2 / ファイル全読み（`std::fs::read` で `Vec<u8>`）/ 自前 mmap FFI

### 決定

ディスク上の `archive.zip` のメモリマッピングに **`memmap2`** を用いる
（`disk::FileMount`）。

### 理由

- 本プロジェクトの中核は「ZIP を仮想メモリのバッキングストアとして扱う」ことで、
  アーカイブは OS のページングで遅延ロードしたい。全読みは大きなアーカイブで
  RAM を食い、設計の前提（mmap + `MADV_RANDOM`）に反する。
- `memmap2` は Windows / Unix 双方に対応し、C ツールチェーン不要。0001 で想定
  していた mmap ライブラリ。
- 設計方針「mmap は外から渡す」に沿い、mmap の所有は `disk` 層に閉じ、`mount` /
  `vmidx` / `archive` は `&[u8]` だけを見る。`Mmap::map` は本質的に unsafe
  （外部書き換えで観測内容が変わりうる）で、read-only マウントとして扱い変更検出は
  fingerprint / ESTALE に委ねる。

### 備考

- 当面 mmap するのは `archive.zip` のみ。サイドカー `vmidx` は `Vec<u8>` で読み
  （無効時は再構築して別 `Vec` になる）、`vmidx` 自体の mmap（`MADV_RANDOM`、
  巨大な標準 DEFLATE 索引向け）は後段の最適化。
- Windows の安定 inode（`MetadataExt::file_index`）は nightly 限定のため、stable
  では inode を 0 とする。fingerprint の確定要因は cd_hash なので影響しない。

---

## 0006. M3（durability）を vmdirty 形式 + 回復読み取りから着手する

- 日付: 2026-06-19
- ステータス: 採用
- 選択肢: (a) vmdirty バイナリ形式 + 回復 walk を先に純データ構造として固める
  / (b) Tier 2 spill の I/O 配線（O_DSYNC / fdatasync / spill ポリシー）から着手

### 決定

M3 の最初の増分を **vmdirty のバイナリ形式（FILE HEADER / DATA RECORD /
COMMIT MARKER / METADATA RECORD の encode）と回復読み取り walk（`read_vmdirty`
→ `RecoveryResult`）** とし、`vmdirty.rs` に純データ構造として実装する。実ファイル
I/O・spill ポリシー・generation_id 生成・compaction は後続に分ける。

### 理由

- vmidx を「形式と CRC をバイト列で先に固める→後で I/O 配線」の順で作った実績が
  あり（`vmidx/` 各モジュール）、同じ流儀が `&[u8]` 上で完結してテストしやすい。
  本リポの方針「mmap/バイト列は外から渡す」とも一致する（回復 walk も `&[u8]`）。
- M3 は設計上いちばん正しさが宿る所で、IMPLEMENTATION_NOTES が
  「`fdatasync` の順序こそ設計」「回復 walk は最初の失敗で止まりそれ以降を信用
  しない」と釘を刺す。**回復 walk の分類ロジック**（torn write の切り捨て、
  COMMIT MARKER 境界、generation_id 不一致での停止）はディスクを触らずに
  クラッシュシナリオを再現してテストできる。耐久性配線（次の増分）の前に、ここを
  テストオラクルとして固める価値が高い。

### 備考

- **CRC は全フィールド CRC-32C（Castagnoli）**。commit のエントリ CRC は ZIP 標準の
  ISO-HDLC で、別物（混同が IMPLEMENTATION_NOTES の罠として明記されている）。
  vmidx の各構造体と同じ `crc32c` クレート（0002）を流用する。
- 不正 UTF-8 のエントリ名は設計どおり置換（U+FFFD）でデコードする。
- LE 読み取りヘルパは `vmidx` の `pub(crate)` 版に依存させず `vmdirty` 内に閉じた
  ものを持つ（モジュール独立を優先）。
- 残り（後続増分）: `VmdirtyWriter`（O_DSYNC / 単一 write / fdatasync）、Tier 1↔
  Tier 2 の FIFO spill と `dirty_limit`、generation_id の CSPRNG 生成（getrandom
  等、依存追加時に 0007 で記録）、回復決定木の mount/disk 配線、compaction。

---

## 0007. generation_id の生成に getrandom を採用し、O_DSYNC を `sync_data()` で近似

- 日付: 2026-06-19
- ステータス: 採用
- 選択肢: generation_id 乱数源 = getrandom / rand / 自前 OS 呼び出し。
  durability = O_DSYNC をプラットフォーム別 FFI で再現 / 明示 `sync_data()` で近似

### 決定

vmdirty の `VmdirtyWriter`（Section 7）を実装するにあたり:

- **generation_id（128bit UUIDv4、Section 6）の乱数源に `getrandom`** を用いる。
- **sync-spill モードの durability は `File::sync_data()` の明示呼び出しで表現**する
  （設計の `O_DSYNC` を移植性のため近似）。各 DATA/METADATA レコードを 1 回の
  `write_all` で書いた直後に `sync_data()`、COMMIT MARKER は常に `sync_data()`。

### 理由

- generation_id は「セッションごとに一意・連番でない・推測不能」が要件で、設計が
  CSPRNG を明示する（Section 6）。`getrandom` は OS のエントロピー源を薄く呼ぶ
  だけの最小クレートで、`rand` のような分布生成器は要らない。version=4 / variant
  ビットは自前で立てる（UUID クレートも不要）。
- `O_DSYNC` 相当を Windows / Unix で揃えるには FFI（`FILE_FLAG_WRITE_THROUGH` /
  `O_DSYNC`）が要るが、durability の本質は「レコード復帰前にディスクに到達して
  いること」。書き込み直後の `sync_data()` で同じ保証が立ち、設計の
  「crash-before/after」テストオラクル（IMPLEMENTATION_NOTES）も満たせる。本リポは
  「純 Rust を基本に、必要な所だけ C へ」方針（0004）で、ここは FFI を避けられる。

### 備考

- `sync_data()`（≒ `fdatasync`）の **失敗は retryable ではない**（fsyncgate）。
  上位で ERROR 状態に倒す配線は回復決定木の増分で入れる。現状の `VmdirtyWriter` は
  `io::Result` を素通しするだけ。
- IMPLEMENTATION_NOTES が指す **rename 後の親ディレクトリ fsync**（compaction /
  commit の原子性 durability）はまだ。Unix 限定の後続課題として残す。
- 依存連番: 0001 Rust / 0002 crc32c / 0003 xxhash-rust / 0004 libz-rs-sys /
  0005 memmap2 / **0007 getrandom**。

---

## 0008. Tier 2 スピルのポリシー核を Diff Layer に「victim 返却型」で置く

- 日付: 2026-06-19
- ステータス: 採用
- 選択肢: (a) `DiffLayer` に dirty_limit と FIFO victim 選択だけ持たせ、退避ページを
  返して **I/O は呼び出し側**（mount/disk が `VmdirtyWriter` へ）/ (b) 設計の
  `Tier1DirtyStore` を新設し、Tier 1 + vmdirty 書き出し + Tier 2 索引を 1 つに束ねる

### 決定

`DiffLayer` に `dirty_limit` / `dirty_bytes` 会計 / FIFO の victim 選択
（`take_spill_victims` が退避すべき `SpilledPage` を古い順に返す）を持たせる。
**vmdirty への実書き込みと Tier 2 読み戻しは持ち込まない**（(a)）。既定は無制限
（`UNLIMITED`）で M2 互換（spill しない）。

### 理由

- `difflayer.rs` は「I/O も圧縮も持たない純データ構造」という確立した役割があり
  （M2）、これを保つと spill ポリシー（どのページを退かすか）をディスク無しで
  ユニットテストできる。設計 Section 5.1 のスピル選択はまさにこの純ロジック。
- 設計の `Tier1DirtyStore` は vmdirty 書き出しまで束ねるが、**vmdirty ファイルの
  ライフサイクル（生成・generation_id・header 指紋・close 時の扱い）は回復読み取り
  （open 時の `read_vmdirty` → 決定木）と同じ場所＝ mount/disk に宿る**。スピル
  書き出しと回復読み取りは同じ vmdirty ハンドル/Tier 2 索引を共有するので、両者を
  同じ配線増分（③）で入れる方が、durability の順序（IMPLEMENTATION_NOTES の
  crash-before/after）を 1 箇所で見切れて安全。先に I/O を difflayer へ散らすと
  half-wired になる。

### 備考

- FIFO は**挿入順**（最古に書かれたページから退避、設計 5.1 既定）。write hit による
  「最新へ再スタンプ」（5.1 の write amplification 軽減）は正しさに無関係な最適化で、
  spill を実 I/O に繋ぐ③で write 経路に置く。
- **未配線ゆえの注意（③で対応）**: spill 後のページへの write hit は、現状の
  `mount::write_into` が `has_page=false` を見てソースから COW 復元してしまう
  （Tier 2 の変更を取りこぼす）。Tier 2 索引と write-through（設計 4.1「Tier 2
  ページへの write hit は新 DATA RECORD を追記」）は③で入れるまで、mount/disk の
  `dirty_limit` は無制限のまま（spill を起こさない）に保つ。
- `take_spill_victims` の返り値は呼び出し側が `append_data_record` で vmdirty へ
  書き、(entry,page)→offset を Tier 2 索引（設計 `VmdirtyIndex`）に積む。

---

## 0009. M3 ③ の配線: 既定回復ハンドラ + 耐久性コアのみ（エントリ操作は分離）

- 日付: 2026-06-19
- ステータス: 採用
- 選択肢:
  - 回復 API = (a) 既定ハンドラで安全枝を自動・曖昧枝は委譲 / (b) 常に呼び出し側へ
    委譲（自動なし）/ (c) 当面は自動のみ（曖昧枝は黙って recover_committed）
  - 範囲 = (d) 耐久性コア（spill 配線 + 三段読み + open 回復）のみ /
    (e) エントリ操作（create/remove/truncate/rename）も同時

### 決定

③（mount/disk への Tier 2 spill 配線 + 回復読み取り）を **(a) + (d)** で実装する。

- **回復 API**: `FileMount::open` は [`DefaultRecoveryHandler`] で設計 Section 3 の
  決定木を回す。**曖昧でない枝のみ自動**（commit 境界ありで `last_commit_seq>0` かつ
  uncommitted 空 → `recover_committed`、stale 空ファイル → silently discard）、
  **データを失いうる枝は `Abort`**（CONFLICT / ヘッダ破損 / version 非対応 /
  commit マーカー無しで未コミットあり）。Abort は `FileMountError::RecoveryRequired`
  として `RecoveryResult` を呼び出し側へ返す。全枝を制御したい場合は
  `open_with_recovery(path, options, handler)` で任意の `RecoveryHandler` を渡す。
- **範囲**: spill 配線（write 経路の Tier1→Tier2→source 三段化、`Tier2`＝writer +
  in-memory `VmdirtyIndex` + read ハンドル）、三段読み、flush（STRICT）、open 回復
  （決定木・`vmdirty.bak.{gen}` rename・新 gen 開始・回復分の flush）まで。エントリ
  操作（METADATA RECORD の生成/replay）は次増分（④近辺）へ分離。spill は **opt-in**
  （`OpenOptions::dirty_limit` 既定 `UNLIMITED`＝従来挙動不変）、sync は **Sync 既定**。

### 理由

- 設計 Section 3 は「VMM は自動解決しない」と宣言する一方、決定木自身が 1 枝に
  `auto: recover_committed（safe default）`、別枝に `silently discard` を許す。素直に
  読めば「**曖昧でない枝は自動・残りは委譲**」であり、(a) がこれに一致する。(b) は
  決定木の `auto`/`silently` と矛盾し全 open にハンドラを強制する。(c) は曖昧枝で
  uncommitted を黙って捨て、不変条件 **I-2「dirty ページは commit/discard/recover
  以外で消えない」**に反する。
- エントリ操作は immutable な vmidx の上に in-memory エントリ表を被せて
  read/write/commit の意味論を変える別物で、耐久性コア（既存エントリのページを
  `entry_name` キーで扱うだけで自己完結）と混ぜると配線が二重化する。設計が「正しさ
  が宿る」とする crash-before/after はコア側に集約されるので、まずそこを緑で固める。

### 備考

- **logical_size の復元は DATA RECORD のテール短長で行う**（設計 Section 2 の max
  ルール `page_index×page_size + data_len`）。spill / write-hit / flush でページを
  書く際、末尾ページは `logical_size` でクランプして書き、読み戻し
  （`read_page_at`）は `page_size` までゼロ埋めする。RESIZE METADATA は使わない
  （implicit extension も max ルールで復元できる）。
- **commit の耐久性**: `build_full` は Tier 1 のみ読むため、commit 前に flush
  （Tier1 を全 durable 化、Tier1 には残す）→ `rehydrate_into`（Tier2 のみのページを
  Tier1 へ読み戻す）してから組み立てる。commit 成功で vmdirty 削除、`vmdirty.bak.*`
  は forensics 用に残す。
- **未了（後続）**: エントリ操作（④）、compaction（dead>live で発火、⑤）、rename 後の
  親ディレクトリ fsync（Unix、⑤）、fsync 失敗を ERROR 状態へ倒す配線（fsyncgate）、
  FIFO の write-hit 再スタンプ最適化（正しさ無関係）。
- 設計テキスト側の Section 3（「自動解決しない」宣言と決定木の `auto`/`silently`
  枝の食い違い）は公開リポで文言を明確化する候補（別 push）。

---

## 0010. ④ エントリ操作を ④a(create/remove/truncate) と ④b(rename) に分割、エントリ表は「現在名キー + source 参照」

- 日付: 2026-06-19
- ステータス: 採用
- 選択肢:
  - 範囲 = (a) 4 操作（create/remove/truncate/rename）を 1 増分 / (b) create/remove/
    truncate を先に固め、rename を別増分に
  - rename の同一性モデル = (c) 現在名キー + source 参照（Diff/Tier2 は現在名で
    キー、エントリ表が各エントリの source 名を持つ）/ (d) 安定 internal identity +
    現在名オーバーレイ

### 決定

**(b) + (c)**。本増分（④a）で create / remove / truncate と journaling・回復 replay
を実装し、rename（④b）は別増分に分ける。エントリ表は **現在名キー + source 参照**。

- **エントリ表**（新規 `entrytable.rs`）は immutable な vmidx に被せる
  オーバーレイ（`Created` / `Tombstone`）。`kind(name, in_vmidx) -> Absent |
  Created | Source` が実効集合「vmidx ∪ created − tombstone」を判定する。論理サイズ
  と dirty ページは [`DiffLayer`] 側に残す純データ構造。④b では `Aliased { source }`
  を足して rename を表現する（それまで `Kind::Source` のソース名 = 現在名）。
- **read/write/commit の seam**: `mount::resolve_entry` がエントリ表 + vmidx を解決し
  `ResolvedEntry { source: Option<String>, original_size }` を返す。`read_dirty` /
  `write_into` / `build_full` は vmidx を名前で引き直さず、この `source`
  （None = created）を受け取る。rename(④b) は resolve の写像を変えるだけで
  read/write 本体に触れない。
- **truncate-shrink の正しさ**: `DiffLayer` に **source high-water**（`source_size`）
  を持たせる。初期値はソースの元サイズ、truncate-shrink で単調に縮む。縮小で捨てた
  末尾領域は後で extend してもソースから蘇らずゼロになる（実 FS の truncate 意味論）。
  read/write/commit のソース読み出し上限はこの `source_size` を使う。
- **created エントリの commit 既定メソッド = DEFLATE**。空〜任意サイズで素直、ZIP の
  一般的既定。STORE 固定は大きい created でアーカイブを膨らませる。
- **回復**: ページとエントリ操作（METADATA）を **sequence 順に統合 replay**
  （`disk::replay_recovered`）して Diff Layer + エントリ表を復元（設計
  ENTRY OPERATIONS「replays records strictly in sequence order」）。回復後は新 gen に
  Create / Remove と各 dirty エントリの RESIZE を **再 journal**してから flush
  （`rejournal_recovered`）。`source_size < logical` のときは RESIZE(source_size) →
  RESIZE(logical) の順で 2 件書き、二次クラッシュでも「縮小して捨てた領域は extend
  してもゼロ」を保つ。これにより M3 ③ の `RecoveryRequired` でエントリ操作を弾く枝を
  実装で置換した。
- **journaling は spill 有効時のみ**（`tier2` が `Some` のとき）。spill 無効
  （`UNLIMITED`）では entry op もページ同様 commit まで non-durable で一貫。

### 理由

- ④a と rename はモジュール性が違う。rename だけが「現在名 ≠ source 名」の分離・
  Diff/Tier2 の再キー・回復での rename replay を要する。create/remove/truncate は
  名前アドレッシングが現在名で完結し、journal も METADATA が既に揃っている。先に
  ここを crash-before/after 込みで緑に固めると、rename を独立に重ねられる。
- 現在名キー + source 参照は設計の (path, page_index) 名前アドレッシングに忠実で、
  journal/回復（名前ベース）とも一致する。安定 identity 案は read/write 内部を
  identity に書き換える侵襲が大きく、journal が名前ベースな点ともズレる。
- source high-water は journal で表現できる状態（RESIZE の縮小→拡大列）に落とせる
  最小の追加状態で、truncate の「捨てた領域は蘇らない」を正しく保つ。

### 備考

- 新規モジュール `entrytable.rs`。`mount` に `resolve_entry` / `entry_create` /
  `entry_remove` / `entry_truncate` / `ResolveError` / `ResolvedEntry` / `EntryError`。
  `difflayer` に `remove_entry` / `truncate_pages` / `source_size`。`tier2` に
  `journal_op` / `purge_entry` / `purge_pages_beyond`。`commit::build_full` は
  `&EntryTable` を取り、tombstone をスキップ・created を新規 LFH/CD で出す。
- 依存追加なし。`cargo test` 166 緑・警告なし。
- **④b rename 実装済み（同 decision、コミット `82a8cd4`）**: 設計どおり `entrytable` に
  `Overlay::Aliased{source}` + `apply_rename`/`aliased_source`/`aliases`/`is_aliased`、
  `difflayer`/`tier2` に `rename_entry`（現在名キーの再キー）、`mount` に `entry_rename`
  （ENOENT/EEXIST、未対応圧縮種別でも通す）/`resolve_entry` の別名解決、`commit::build_full`
  の alias ループ（未 dirty=verbatim コピー／dirty=ソース元メソッドで再圧縮）、`disk` の
  `replay_recovered`(`MetaOp::Rename` + `base_for`)/`rejournal_recovered`（RENAME を RESIZE
  前に出し rename 元 REMOVE を省く）。連鎖 rename は究極のソースへ畳む。`cargo test`
  184 緑・警告なし。
- **未了（⑤ 以降）**: compaction（⑤）、rename 後の親 dir fsync
  （⑤）、fsyncgate。CD-only 改名の INCREMENTAL は M4。非 UTF-8 名のエントリ操作は
  対象外（エントリ表は UTF-8 名のみ）。

## 0011. M4 = append-only INCREMENTAL + 別機構の FULL compaction（in-place 穴再利用は不採用）

- 日付: 2026-06-19
- ステータス: 採用（設計方針。実装は ⑤ 以降）
- 選択肢（空間回収モデル）:
  - (a) **in-place 穴再利用 + フリーブロックリスト**（既存ブロックをその場で書き換え、
    入らなければ近傍ブロックを EOF へ逃がして空きをマーク、空きを best-fit 再利用する
    ハイブリッド。US8024382 型）
  - (b) **append-only INCREMENTAL コミット**（変更/新規エントリを EOF に追記し新 CD を
    EOF に書く。古いデータは dead）＋ **別機構の FULL compaction**（dead/live 比トリガで
    全書き直し + rename）

### 決定

**(b)**。M4 は append-only INCREMENTAL と FULL compaction を**別々の機構**として用意し、
**アーカイブ本体の live バイトを in-place で書き換える穴再利用は採らない**。穴再利用が要る
回収は compaction の中（全書き直し時の再配置）だけに閉じ、incremental 経路には
穴再利用・近傍シャッフル・CD ギャップ由来の freelist を一切持ち込まない。両者は
dead/live 比トリガで繋ぐ（コピー GC / LSM / SQLite auto-vacuum と同型）。

### 理由

**根源的理由: このアーキの不変条件「live データを in-place で壊さない」と一致する。**
zip-vmm は (i) ソースを不変スナップショットとして read-only mmap、(ii) 書き込みは COW で
diff layer、(iii) commit は rename で原子置換、という「既存バイトを破壊的に書き換えない」
設計。append-only も compaction(rename) もこの原則を保つが、in-place 穴再利用は破る。
M4 を append-only にするのは diff layer の COW / FULL commit の rename と**同じ原則を
アーカイブ本体に適用しただけ**で、設計に一貫する。

in-place 穴再利用がこのアーキで具体的に壊すもの（不採用の根拠）:

1. **不変スナップショット mmap + ESTALE**: 自分の commit が size/mtime/cd_hash を変え、
   自前の ESTALE/fingerprint 検知を**自分で誤発火**させる。特に Windows はマップ中
   ファイルの上書き・短縮が制限される。
2. **vmidx（fingerprint 検証つきキャッシュ）**: 穴再利用は既存エントリの**オフセットを
   動かす** → cd_hash 変化で vmidx 無効 → EAGER 再構築（再 inflate で CP 生成）。
   「全書き直しを避けた」利得を seek index 再構築が食う。append-only はオフセット不変で
   vmidx を**追記拡張**できる。
3. **クラッシュ安全**: rename の無料の原子性を失う。CD/EOCD の in-place 更新は原子的でなく、
   vmdirty WAL を**アーカイブ本体の変更**まで広げる（shadow-CD 等）必要があり複雑度が跳ねる。
4. **fingerprint 同一性**: in-place は inode 同じ・cd_hash 変化 → 回復決定木の CONFLICT 枝と
   衝突する。
5. **dead zone と ZIP 妥当性**: 穴を署名でマークすれば特許の raw-block 機構そのもの、
   しなければ厳格な線形バリデータが嫌う場合がある。append-only + tail 寄り dead が単純。

→ このアーキでは in-place 再利用の旨味が 2・3 で大きく相殺される。append-only は4本柱
すべてと素直に噛み合う。

**append-only と compaction の役割分担（どちらも優れる面があるから両方用意する）:**

| | append-only INCREMENTAL（前者） | FULL compaction（後者） |
|---|---|---|
| 強み | 低レイテンシな頻繁・小コミット、シーケンシャル書き込み、vmidx 追記拡張、書込中も旧データ無傷 | dead space 完全回収・ファイルサイズ有界化、断片化解消、正準な clean ZIP 生成、vmidx を新鮮に再構築、rename で原子安全 |
| 弱み | 単調増加（dead 蓄積） | O(filesize) バースト・一時 2 倍ディスク |
| 役割 | 通常パス | 閾値トリガの回収パス |

定常オーバーヘッドは閾値で有界、1 バイトあたり償却 O(1) 回の書き直しに収まる。

### 特許に関する備考（主目的ではない副次効果）

- 本方針は **US8024382B2（[`PRIOR_ART.md`](PRIOR_ART.md)）の芯**（in-place ブロック編集 +
  CD 由来 freelist + 近傍シャッフル回収）を**実施しない**ので結果的にクレーム外に収まる。
  ただし**不採用の主因はエンジニアリング（上記アーキ不整合）であり、回避が目的ではない**。
  「組合せで適用範囲を曖昧化」は all-elements rule で効かない（周辺機能を足しても claim の
  全要素を踏めば踏む）ため、最初から狙わない。
- in-place ハイブリッドを将来試すなら: アーカイブ本体変更を vmdirty WAL の一級市民にする
  前提で**実験モジュール・既定 off**。claim 8（freed extent への modified データ配置）が
  境界＝**FTO 対象**。米国のみ・~2030 満了の文脈を踏まえ、確実性が要るなら弁理士。
- 特定特許の「認識」を公開文書に明記すると willful 主張を呼びうるため、本判断ログは
  private リポの DECISIONS に留め、公開候補の `PRIOR_ART.md` は prior art の記述に限定する。
  （以上は法的助言ではない。）

## 0012. M4 append-only INCREMENTAL commit の設計（追記レイアウト / truncate ロールバック / ディスク効率）

> インプレース処理…やりたかったなあ　\^\） 

- 日付: 2026-06-19
- ステータス: 採用（設計。実装はこれから）
- 文脈: ADR 0011 で M4 = append-only INCREMENTAL + 別機構 FULL compaction と決めた。本 ADR は
  append-only 経路の具体設計。**本プロジェクトの第一目標はディスク効率の最大化**（in-place を
  志向した動機の核）。

### 決定（追記レイアウト）

INCREMENTAL commit は既存アーカイブのバイトを保ったまま差分を末尾に積む:

1. **未変更エントリは元のオフセットのまま**（既存バイト再利用 → 追記コスト 0、vmidx も有効）。
2. 変更/新規エントリの LFH + データを **EOF へ追記**。
3. 全 live エントリを指す **新 CD を追記**（未変更 = 元オフセット、変更 = 追記オフセット）。
4. **新 EOCD を末尾**に。旧 CD/EOCD・変更前バイトは dead として中間に取り残す。

結果: 1 編集あたり全書き直しなし。大きなアーカイブ + 小さな編集で I/O が `O(変更 + 件数)` に
（FULL の `O(filesize)` に対して）。dead の累積は FULL compaction（既存 `build_full` +
`durable_replace`）で回収し、定常サイズを上界する。

### クラッシュ安全（truncate ロールバック）

**旧バイト `[0, old_len)` を一切書き換えないので、ロールバック = `old_len` への truncate で
旧アーカイブ（妥当な ZIP）に戻る。** よって WAL は軽い:

- 追記前に `old_len` を sidecar に INTENT として記録 + fsync。
- 新データ + 新 CD + 新 EOCD を書いて fsync し、COMMIT を記録。
- 回復: INTENT あり・COMMIT なし → アーカイブを `old_len` へ truncate（旧へ復帰）。COMMIT あり
  → 新を採用。
- in-place のような before-image ログは不要（上書きしないから）。これが append-only を選ぶ
  実利の一つ（「安全な WAL が複雑」という懸念は append-only では小さい）。

### mmap / プラットフォーム

- INCREMENTAL commit も FULL commit と同じく **`self` を消費し mmap を先に解放**してから
  アーカイブを開いて追記する。よって live な read-only mmap を変更する問題（Windows の
  マップ中ファイル制約を含む）を回避する。呼び出し側は commit 後に開き直す。

### ディスク効率のレバー（第一目標に直結）

- 未変更エントリは追記ゼロ（既存バイト再利用）。
- **INCREMENTAL / FULL の閾値**: 追記分 + dead が bloat 係数を超えたら FULL compaction で回収。
  定常サイズの上界 = この閾値。
- **content-addressed dedup（後段・任意）**: 追記前に圧縮バイト列の XXH3 が既存と一致したら
  追記せず CD で既存オフセットを指す。dead を増やさず重複を排除（特許の free-block 再利用とは
  別軸、クレーム外）。
- **vmidx 追記拡張**: 未変更エントリのオフセットが不変なので、将来は索引を丸ごと作り直さず
  追記分だけ拡張できる（v1 は cache 再構築でも可）。

### 特許

ADR 0011 のとおり incremental 経路に **穴再利用・近傍シャッフル・free-block list を持ち込まない**
ので claim 1/2/6/7 外（dead は「次の FULL compaction で捨てるだけの未追跡バイト」で、再利用する
"free block" ではない）。in-place は実験トラック / 満了後（~2030）昇格の道を残す。

### 実装の刻み（予定）

1. `build_incremental`（既存アーカイブ + diff/table から「追記計画 + 新 CD/EOCD バイト」を生成。
   未変更は元オフセット、変更/新規は追記オフセットで CD を組む）。
2. `FileMount::commit_incremental`（mmap 解放 → INTENT 記録 → 追記 → COMMIT。回復に
   truncate ロールバックを配線）。
3. INCREMENTAL / FULL 選択ポリシー（閾値）。
4. （後段）dedup、vmidx 追記拡張。

### 背景

in-place は当初の方向で、動機はディスク効率の最大化。append-only はその約束（全書き直しを
しない編集）を継ぎ、空間回収は compaction に委ねる。in-place は削除せず実験 / post-2030
トラックとして残す（ADR 0011）。

## 0013. M4 刻み3 = INCREMENTAL/FULL 選択ポリシー（bloat 閾値、設計の忠実実装）

- 日付: 2026-06-19
- ステータス: 採用
- 文脈: ADR 0011/0012 で M4 = append-only INCREMENTAL + 別機構 FULL compaction と決め、刻み1/2 で
  `build_incremental` と `FileMount::commit_incremental` を実装した。刻み3 は「いつ INCREMENTAL、
  いつ FULL を選ぶか」の層。

### 決定

設計仕様 `ZIP_Virtual_Memory_Manager.txt` の **WRITE STRATEGY SELECTION**（"Bloat tracking" /
"Compaction thresholds" / "Strategy selection"）が既にこのポリシーを厳密に定義している。よって
刻み3 は**新規設計ではなく仕様の忠実な実装**とする（この層自体は設計との差分なし）。

- **bloat メトリクス（CD だけから求まり追加 I/O 不要）**:

  ```
  bloat_bytes = file_size − Σ(compressed_size) − cd_size − eocd_size
  bloat_ratio = file_size / (file_size − bloat_bytes)   （= file_size / live_size）
  ```

  `archive::Archive::bloat() -> archive::Bloat{ file_size, live_size, bloat_bytes, bloat_ratio }`
  を新設。LFH のバイトは live に数えない（LFH 長は読まないと分からず "追加 I/O 不要" を守るため。
  設計どおり近似的に bloat 側へ含む）。最小アーカイブでも ratio は厳密な 1.0 でなく LFH 分だけ
  わずかに上回るが、データに対して小さく既定閾値（2.0）には届かない。

- **閾値（`OpenOptions` に追加・`FileMount` が保持）**:
  - `gc_threshold: f64`（既定 **2.0** = アーカイブが live の倍に膨らんだら回収）
  - `gc_max_bloat_bytes: u64`（既定 `UNLIMITED` = バイト数では発火しない）
  - 両条件は独立に評価し、一方でも満たせば FULL（設計 "either alone is sufficient"）。

- **API（spec の名前ごと合わせる。当初 `commit_auto`/`compact` で実装したが、同セッションで
  spec 整合へ改名した）**:
  - `FileMount::commit(self) -> Result<CommitOutcome, FileMountError>` = 標準の入口。
    `bloat_ratio ≥ gc_threshold || bloat_bytes ≥ gc_max_bloat_bytes` で **FULL**、それ以外 **INCREMENTAL**
    （設計 `commit()` = "Selects INCREMENTAL or FULL based on bloat_ratio and gc_threshold"）。
  - `FileMount::commit_full(self)` = 明示 FULL（設計 `commit_strategy=FULL` / `commit(force_compact=true)`）。
  - `FileMount::commit_incremental(self)` = 明示 INCREMENTAL プリミティブ。
  - `FileMount::compact(self)` = CLEAN からのアーカイブ FULL compaction（設計 `compact()`。dirty なら
    `CompactWhileDirty`）。中身は空 Diff での FULL commit。
  - `FileMount::compact_journal(&self)` = vmdirty ジャーナル compaction（設計 `compactJournal()`、⑤）。
    旧名 `compact`。対の predicate は `should_compact_journal()`（旧 `should_compact`）。
  - 補助: `should_full_commit()` / `bloat()`、`CommitOutcome{Noop,Incremental,Full}`。
  選択は **CD だけを見て**決めるので選んだ片方しか build せず、再圧縮の二度手間は起きない。`Mount`
  （メモリ版プリミティブ層）は閾値を持たず `commit_full` / `commit_incremental` のみ（自動選択は無し）。

### 理由

- 第一目標のディスク効率（ADR 0012）は、通常は安い INCREMENTAL（未変更は追記ゼロ）で進め、
  dead が積もった時だけ FULL で全回収する、というコピー GC / LSM / SQLite auto-vacuum 同型の
  閾値方式で達成できる。定常ファイルサイズは `gc_threshold × live` で上界される。
- メトリクスを CD だけから採るのは「commit のたびに O(1) で判定でき、追加 I/O を要さない」ため
  （設計が明記）。判定は INCREMENTAL の build より前に済むので、FULL を選ぶ時も incremental の
  追記分を作って捨てる無駄が無い。

### 命名の整合（当初の差分を解消）

刻み3 を最初 `commit_auto()`（自動選択）/ `commit()`（明示 FULL）/ `compact()`（vmdirty ジャーナル）で
実装したが、これは設計の `commit()`（自動選択）/ `compact()`（アーカイブ compaction）/ `compactJournal()`
（ジャーナル）と名前が食い違っていた。同セッション中に**本質的に spec へ寄せる**判断（本人合意）で上記
API へ改名し、差分を解消した。`commit()` を主入口にできるのが要点（設計が想定する標準フロー）。
残る意図的差分・整合メモは [`SPEC_DIVERGENCE.md`](SPEC_DIVERGENCE.md) に集約。

### 未実装（後続）

- `background_compact`（背景スレッドでの FULL）と `commit(mode=BACKGROUND)` は未実装（M4 範囲外）。
- 刻み4: content-addressed dedup（XXH3、特許外）/ vmidx 追記拡張。
- 設計の INCREMENTAL 手順が挙げる **Dead Space Freelist / in-place 穴再利用は不実装**（ADR 0011）。
  dead は「次の FULL で捨てるだけの未追跡バイト」で、再利用する free block ではない。`SPEC_DIVERGENCE.md` 参照。

---

## 0014. VMM-native in-place commit の ZIP CRC-32 取扱い: per-block CRC キャッシュ + CD/LFH の crc32 point-write

- 日付: 2026-06-21
- ステータス: 採用（設計方針。実装は M5+ VMM-native DEFLATE 着手時）
- 文脈: 設計レビューで、VMM-native in-place commit と STORE 同サイズ in-place 書き戻しの両方で
  ZIP 標準の CRC-32（CD entry / LFH の crc32 フィールド、ISO-HDLC）の取扱いが**仕様にも実装にも
  記述されていない**ことが判明した。block 内のページを編集すれば uncompressed バイトが変わるので
  entry CRC は必然的に変わるが、設計テキストは "No CD update" と書いており、サードパーティの
  unzip/7-Zip が CRC エラーを返す → 互換表の "fully compatible" 主張と矛盾する。設計リポ側は
  本セッションで CRC-32 maintenance サブセクションを追加し "No CD structural update" へ書き換え
  済み（コミット `406f558`）。本 ADR はその採用方針を実装側に記録する。

### 決定

VMM-native in-place commit と STORE 同サイズ in-place 書き戻しで entry CRC-32 を維持する方法として、
以下を採る:

1. **vmidx の DEFLATE_VMM checkpoint に `block_crc32: u32`（ISO-HDLC）を追加**。
   checkpoint サイズ 24 → 28 バイト。設計仕様で更新済み（コミット `406f558`）。
2. **clean ブロックの CRC は vmidx キャッシュから再利用**、dirty ブロックのみ再計算。
3. **entry CRC は `crc32_combine` で per-block CRC を合成**（zlib の `crc32_combine()` 相当の
   閉形 GF(2) 演算）。clean ブロックの uncompressed バイトを読み戻す必要が無い。
4. **CD entry の crc32 フィールド（CD entry offset 16, 4 bytes）と LFH の crc32 フィールド
   （LFH offset 14, 4 bytes）のみを point-write**。CD 構造・順序・サイズ・オフセット・EOCD は
   一切変更しない。これが "No CD structural update" の意味。
5. **STORE 同サイズ in-place 書き戻し**も同じ point-write 経路。per-page CRC キャッシュは任意
   （I/O-bound なので entry 全体を再 CRC でも実用範囲）。
6. **クラッシュ安全性**は既存の block 完了 journaling barrier（VMM-native in-place commit
   step 1b）と vmdirty 再生で吸収。block 上書きと CRC point-write を冪等に再実行する。

### 理由

選択肢を以下の 3 つで評価した:

- **(a) per-block CRC キャッシュ + CD/LFH point-write**（採用）
- **(b) Data descriptor 強制（GPBF bit 3）**: LFH の crc32 を 0 にしてデータ末尾に data descriptor を
  置く。**CD 側の crc32 は依然として更新必要**（一部 extractor は CD のみ検査）。データ末尾の
  data descriptor 位置も in-place commit ごとに変動するため、padding と data descriptor の
  相互作用がさらに複雑化する。利得が無い。
- **(c) CRC を更新せず stale を許容**: 互換表 "fully compatible" と本文 "readable by any
  third-party tool without modification" と矛盾する。本プロジェクトの第一目標と相反する。

(a) を採る根拠:

1. **設計原理の対称性**: 本アーキの中核は Z_FULL_FLUSH による**ブロック単位の独立性**と vmidx の
   per-block メタデータ。CRC-32 はちょうど同じ「ブロック単位で独立にキャッシュできる量」で、
   既存の checkpoint が `(compressed_offset, uncompressed_offset, capacity)` を抱えているのに
   CRC だけ抱えていないのが**非対称**だった。block_crc32 追加は同じ原理を一段揃える。
2. **計算コストが in-place 経路の利得を壊さない**: dirty ブロックの CRC は再圧縮で uncompressed
   バイト列が手元にある時点でついでに計算される（実質追加コストゼロ）。clean ブロックはキャッシュ
   読みのみで I/O ゼロ。`crc32_combine` は閉形演算で O(log L) または O(1)（テーブル展開すれば）。
3. **CD への副作用は最小**: 4 バイトの point-write 2 箇所のみ。append/relocation/EOCD rewrite は
   発生しない。設計の "in-place の旨味" は維持される。
4. **クラッシュ安全が既存メカニズムで成立**: vmdirty の block 完了 journaling barrier (step 1b)
   と generation_id ベースの sequence 再生で、block 上書き + CRC point-write の冪等再実行が
   可能。新しい WAL 機構は不要。
5. **vmidx 増分が小さい**: 100 GB DEFLATE / 1 MB 間隔で 2.7 MB（24 bytes 時の 2.2 MB から 0.5 MB
   増）。実用上の影響は無い。

### 実装メモ（M5+ VMM-native 着手時）

- **CRC-32 ISO-HDLC のクレート**: vmidx と vmdirty は `crc32c` クレートで CRC-32C（Castagnoli、
  ADR 0002）を使う。ZIP の CRC-32 は別ポリノミアル（ISO-HDLC）なので**別クレートが必要**。
  IMPLEMENTATION_NOTES が「zlib の `crc32()` は ISO-HDLC、CRC-32C と混同しない」と既に釘を
  刺している。候補:
  - `crc32fast` クレート（純 Rust、SSE4.2 アクセラレーション、`hash_one` あり）
  - `flate2` 内部の `crc32` 関数（既に依存に入っているが、`flate2` の公開 API は限定的）
  - `libz-rs-sys` の `crc32`（既に依存に入っており ADR 0004、`crc32_combine` も提供）

  ADR 0004 で既に `libz-rs-sys` を採用しているため、**第一候補は `libz-rs-sys::crc32` と
  `crc32_combine`** とする。新規依存追加を回避でき、`crc32_combine` が標準で揃う点で有利。
  実装時に `libz-rs-sys` のシンボルエクスポートを確認し、出ていなければ `crc32fast` +
  自前の `crc32_combine` 実装（zlib のアルゴリズムの移植、~50 行）に切り替える。

- **vmidx checkpoint レイアウト変更**: `DEFLATE_VMM` の checkpoint encode/decode を
  24 → 28 バイトに拡張。`vmidx` の format バージョンを上げる必要があるかは要検討
  （現状 VMM-native は未実装なので互換性問題は起きない）。

- **in-place commit 手順での追加ステップ**:
  1. block recompress 時に block_crc32 を計算（`libz-rs-sys::crc32` を block の uncompressed
     バイト列に適用）。
  2. clean ブロックの block_crc32 を vmidx から読む。
  3. `crc32_combine(crc1, crc2, len2)` を block 順に畳んで entry_crc を得る。
  4. CD entry の絶対オフセット計算（`cd_offset + entry_cd_offset_within_cd + 16`）と LFH の
     絶対オフセット（`lfh_offset + 14`）に entry_crc を 4 バイト LE で point-write。
  5. fdatasync。

- **STORE 同サイズ in-place 書き戻し**: per-page CRC キャッシュは v1 で不要、entry 全体を
  再 CRC で十分（uncompressed バイトは既に手元）。`crc32_combine` 不要。

- **テスト**: IMPLEMENTATION_NOTES が指す「全クラッシュ境界で stock extractor が clean に
  open できる」プロパティテストを VMM-native の in-place 経路にも掛ける。`unzip -t` /
  Python `zipfile.testzip()` / 7-Zip CLI のいずれかで CRC 検証を回す。

### 設計差分

無し。設計仕様側を**先に**更新した（コミット `406f558`）ので、本 ADR は仕様準拠の実装方針を
記録するもの。SPEC_DIVERGENCE.md には追加しない。

### 関連

- 設計仕様の更新コミット: `zip-virtual-memory-manager` リポの `406f558`
  「仕様レビューによる整合性修正と in-place commit の CRC-32 取扱い明文化」。
- ADR 0002（CRC-32C / `crc32c` クレート）、ADR 0004（`libz-rs-sys`）。
- IMPLEMENTATION_NOTES.md「CRC-32C is Castagnoli, not zlib's crc32()」の注意。

### 未了

- 実装は M5+ VMM-native DEFLATE 着手時。M4 刻み4（dedup / vmidx 追記拡張）→ M5 の順で進む。
- `libz-rs-sys` の `crc32`/`crc32_combine` の Rust シンボル確認は実装着手時に行う。
- vmidx format version bump の要否判断（現状 VMM-native 未実装なので、初出時に新レイアウトで
  出せば bump 不要の可能性が高い）。

### 追記 (2026-06-21、設計レビュー第2陣を反映)

設計仕様側コミット `e3b61dd` (Tier 1) + `88ff7ef` (Tier 2) で in-place 経路の
不変条件と STORE in-place の CRC バリアが明文化された。本 ADR の実装方針は
そのまま有効。M5+ 着手時に追加で守るべき点を 3 つ記録する。

1. **in-place の Applicability チェックを entry 単位で行う** (設計 #4/#5)。
   - dirty entry ごとに `entry.logical_size == vmidx.uncompressed_size` を
     確認 (commit step 1 の前)。
   - 一致しない entry (truncate-shrink / implicit extension) は in-place を
     スキップし、INCREMENTAL append の overflow fallback (step 2d と同経路) に
     回す。一致する entry のみ in-place で進む。判定は entry 単位で、
     commit 全体ではない。
   - block 境界跨ぎ shrink は末尾 BFINAL の再アンカが必要なため in-place で
     扱わない。これも上の `logical_size` 一致チェックで自然に弾かれる。

2. **STORE 同サイズ in-place の CRC バリアを明示する** (設計 #6)。
   - 既に "全 dirty page を vmdirty に COMMIT MARKER + fdatasync で先に
     durable 化" は flush() STRICT (commit() 第一段) で成立しているが、
     これが crash safety の前提であることをコードコメントとテストに
     反映する。
   - データ overwrite と CRC point-write の **両方の後**に fdatasync を
     置き、その後で vmdirty を削除する順序を厳守 (ADR 本文 4 と同じ順序、
     データ書き込みと CRC point-write を分けて 2 回 fdatasync するのではなく
     両方完了後 1 回でよい)。

3. **ENOSPC during VMM-native in-place commit の扱い** (設計 Class 2 補完)。
   - step 1b の block 完了 journaling barrier (clean page を vmdirty に
     追記する段) が唯一 ENOSPC を出す経路。
   - ENOSPC → commit() を失敗させ vmdirty は retained。COMMIT MARKER 無しで
     終わるので recovery は uncommitted 扱いで自然処理。archive.zip は不変。
   - 既存の vmdirty 書き込み path の ENOSPC 経路 (M3 ③ で配線済み) が
     そのまま再利用できる。

---

## 0015. コード品質・テスト深度の棚卸し計画 (外部レビュー反映)

- 日付: 2026-06-21
- ステータス: 計画 (実施は M5 と並行 / 一部は機能実装に先んじて着手)
- 文脈: 外部レビュー + 内部追加調査で「コードベースが crash-safe ストレージ層
  として主張するには test infra と panic 面が浅い」という指摘を受けた。本 ADR は
  指摘を **棚卸しメソドロジと優先順** として受け止める。本 ADR 自体は方針記録で、
  本コミットには実コード変更を含まない。各項目はそれぞれ後続コミットで進める。

### 受け止めた指摘

1. **`.unwrap()` 397 / `.expect()` 204 = 計 601 箇所**。crash-safe を謳う以上、
   「到達不能な不変条件」と「外部入力で踏みうる」を分類する必要がある。
2. **mount.rs の `write_into` / `read_dirty` / `read_cached` の引数 9/9/8 個**。
   名前付き struct にまとめるべき。clippy の `too_many_arguments` が出る。
3. **commit.rs の `Vec<(u64, u16, u32, u64, u64, Vec<u8>)>` が 5 箇所**
   (line 94, 240, 362, 384, 464) で同じ匿名タプルを使い回している。
   名前付き struct + コンストラクタへ。
4. **テストはすべて in-process unit テスト** (`#[cfg(test)] mod tests`)。
   integration test ディレクトリ無し、`proptest` / `cargo-fuzz` / loom 無し。
   crash 整合性を主張する以上、性質ベース / fault injection / 簡易シミュレーション
   のいずれかは必要。

### 決定 (方針 + 優先順位)

**Phase A (短期、M4 残務と並行): 外形整理**

A-1. **`mount.rs` の引数群を struct 化** — `write_into` / `read_dirty` /
     `read_cached` をそれぞれ `WriteCtx<'_>` / `ReadCtx<'_>` 相当の名前付き struct で
     受ける。中身の処理は触らない (refactor のみ)。clippy `too_many_arguments`
     warning が消える + ドキュメントコメントが書きやすくなる。

A-2. **`commit.rs` の 6-tuple を struct 化** — `PlacedEntry { offset: u64,
     method: u16, crc32: u32, comp_size: u64, uncomp_size: u64, name: Vec<u8> }`
     的な struct を導入し、5 箇所すべてを置換。`name` の所有を含めるかは実装時
     判断 (現状 6-tuple は所有を含むが、struct 化のついでに参照取りに直せる
     場所があれば分ける)。

A-3. **`provider/deflate.rs:421` の `panic!("read at {offset} failed")`** が
     production 経路なのかテスト経路なのかを確認 (周辺コンテキスト依存)。
     production ならエラー返却に置換、テストならコメントで明示。
     `vmidx/checkpoint.rs:166` の `unreachable!` は不変条件が静的に成立する
     ことを doc 化。

A-4. **モジュールヘッダ docコメント** — 既に修正済み (コミット `258a5c7`)。
     継続的に「実装が進んだら該当コメントを更新」を PR レビューで効かせる。

**Phase B (中期、M5 着手前か並行): unwrap/expect 棚卸し**

B-1. **`rg '\.unwrap\(\)|\.expect\('` の全 601 箇所を 3 分類**:
     - (i) **到達不能 (`safe-by-construction`)**: 数学的・型的に panic 不能。
       例: `rd_u16` (archive.rs:413) の `b.get(off..off+2).map(|s| u16::from_le_bytes(s.try_into().unwrap()))` は
       `get(off..off+2)` が長さ 2 を返すので `[u8; 2]` への try_into は不能失敗。
       → コメントで `// SAFETY: get(off..off+2) guarantees len == 2` を残す。
     - (ii) **不変条件 (`invariant`)**: 直前の検査で保証される。
       → assert! か unwrap_or_else でエラー化 (パフォーマンス影響を見て選ぶ)。
     - (iii) **外部入力依存 (`external`)**: 攻撃者が踏みうる。
       → `?` でエラー伝播に置換。
     計測は CSV (`file:line, category, rationale`) で残し、ADR 0015-supplement
     として記録。

B-2. **テストコードの unwrap/expect** は対象外 (テストは panic で fail させて
     よい)。`grep -l '#\[cfg(test)\]'` でテストブロック内かどうかを文脈判定。

**Phase C (中期〜長期、新規依存追加を伴う): テスト深度**

C-1. **`proptest` を導入** — Cargo.toml の `[dev-dependencies]` に追加 (ADR
     依存連番に登録)。性質ベーステストの最初の標的:
     - `archive.rs::parse` — 任意のバイト列で panic しない (ARCHIVE_MALFORMED
       か Truncated を返す)。これは UNTRUSTED ARCHIVES の B (HANDOFF.md) と
       直結し、防御未実装の現状を proptest が浮かび上がらせる。
     - `vmdirty::encode_data_record` / `read_vmdirty` ラウンドトリップ。
     - `entrytable::apply_rename` の連鎖 rename 畳み込み (ADR 0010 ④b)。

C-2. **`cargo-fuzz` の評価** — proptest が一通り揃った後。`archive.rs` と
     `vmdirty::read_vmdirty` を harness に。crash 入力をコーパスに溜める。
     proptest との重複領域は多いが、long-running fuzz は別チャネル。

C-3. **Crash injection テスト** — 設計の crash-before/crash-after 境界
     (IMPLEMENTATION_NOTES が「テストオラクル」と呼ぶもの) を fault injector で
     実演する deterministic simulation を 1 つ書く。第一目標は INCREMENTAL
     commit (ADR 0012) と FULL commit の `durable_replace` の境界:
     - INTENT 記録 / 追記 / COMMIT 記録の各点で擬似クラッシュ → 再 open →
       正しい状態へ復旧 / ロールバックが起きるか。
     - vmdirty Section 2 の回復 walk が「最初の失敗で停止」を守るか。
     完全な deterministic simulation framework (loom 等) ではなく、まずは
     `cfg(test)` でフックされる fault injection trait + integration test
     ディレクトリで十分。

### 理由

- 段階分けの根拠: A は機能を増やさず外形だけ揃える純粋 refactor で、A 単独で
  「読みやすい」「PR レビューしやすい」が成立する。M5 (VMM-native DEFLATE,
  ADR 0014) 着手前にやっておくと、その上に乗る変更が clean に見えやすい。
- B は機能変更を伴わないが「panic しない契約」を初めて明文化する作業。
  外部レビューで指摘された「crash-safe を謳うなら panic 面の棚卸しを」への
  直接の回答。1 PR 1 ファイルでも進められる粒度。
- C は test infra 投資で初期コストが大きい。proptest は 1 つ書けばパターンが
  確立するので最初のテストを書く工数で「方法論を確立する」価値がある。

### 既存 ADR との関係

- ADR 0001-0007: 言語・クレート選定。本 ADR は dev-dependency の proptest /
  arbitrary 追加を C-1 で予告する (連番 0016 以降で個別記録)。
- ADR 0014: VMM-native in-place CRC 設計。M5 で実装着手するときは A-1/A-2
  refactor 後の signature で書く。
- IMPLEMENTATION_NOTES.md「全クラッシュ境界で stock extractor が clean に
  open できる」プロパティテスト — C-3 はこれの第一歩。

### Phase A の実施結果 (2026-08-31 追記)

Phase A は完了した (ブランチ `adr0015/phase-a`、`cargo test` 203 緑 /
windows-latest + ubuntu-latest)。計画からの差分:

- **A-1**: `WriteCtx` / `ReadCtx` の 2 つではなく `EntryCtx` 1 つに統合した。
  `write_into` と `read_dirty` が要る文脈 (archive / vmidx_image / path /
  source / original_size) が完全に一致し、分ける理由が無かったため。
  `read_cached` は性質が違うので `PageIo{cache, cfg}` を別に立てた。
  対象 3 関数は 9/9/8 引数 → 5/5/7 引数。`fill_run` も 8 → 7。
- **A-2**: `PlacedEntry` の offset フィールドは `archive::CdEntry` に合わせて
  `local_header_offset` とした。加えて出力 1 エントリの中身を表す
  `EntryPayload` を分け、`place_entry` / `place_appended` の重複本体を
  `write_placed` に寄せた。
- **A-3**: `provider/deflate.rs:421` の `panic!` は **テスト経路**だった
  (`#[test] fn seek_via_checkpoint_matches_original` の中) ので変更不要。
  `vmidx/checkpoint.rs` の `unreachable!` は不変条件を doc 化した。
- **副産物**: `too_many_arguments` は 3 件とも消えた。ただし clippy を CI に
  入れた結果、**それ以外の既存指摘が lib 9 件 + lib test 27 件**あることが
  判明した (collapsible_if / needless_update / needless_option_as_deref /
  useless_vec / manual_is_multiple_of / doc_lazy_continuation ほか)。
  Phase A の想定外だったため CI では clippy を**非ブロッキング**にしてある。
  → **A-5 として棚卸し対象に追加**する (Phase B の前でも後でもよい)。

### 未了

- ~~**A-5** clippy 既存指摘の棚卸しと、CI での `-D warnings` ブロッキング化~~
  → **完了**。28 件を解消し、CI の clippy をブロッキングに戻した（`ef4563c` / `66792c0`）。
- B-1 の棚卸し CSV と ADR 0015-supplement
- C-1 proptest 導入 + 最初のラウンドトリップテスト
- C-2 / C-3 は C-1 の経験を見てから判断

> **実施順の変更（ADR 0016）**: 本 ADR は Phase B → Phase C の順で書いているが、
> **ADR 0016 で C → B に入れ替えた**。601 箇所の unwrap/expect を人手で三分類するより、
> proptest / fuzz に「実際に踏める unwrap」を先に探させたほうが安く、かつ正確なため。
> 現在の本筋は C-3（クラッシュ注入テスト）。

---

## 0016. 開発順序の組み替え: 門 → 証明 → 構造 → 機能

- 日付: 2026-09-01
- ステータス: 採用
- 文脈: 二つの事情が重なった。(1) 開発機の `D:` ドライブが消失し、`CARGO_HOME=D:\cargo` /
  `RUSTUP_HOME=D:\rustup` が宙吊りになったため**ローカルに Rust ツールチェーンが無い**
  （`C:` の空きも 1.4 GB で入れ直せない）。以後の検証は GitHub Actions が唯一の経路になった。
  (2) ADR 0015 が「crash-safe を謳うには test infra と panic 面が浅い」という指摘を
  棚卸し計画として受け止めたまま、実施順は「M4 刻み4 が次の本筋」のままになっていた。
  この機に、何を先にやるべきかを原則から引き直す。

### 決定

**原則: 主張は、拡張する前に裏打ちする。** このリポジトリの主張は「ZIP をクラッシュ
セーフな仮想メモリのバッキングストアにする」の一点なので、順序はそこから決まる。

0. **門**: CI（`cargo test` × windows/ubuntu + clippy `-D warnings`）。以降のコードが
   劣化しない土台。→ **完了**（ADR 0015 Phase A + A-5）。
1. **クラッシュ注入テスト（ADR 0015 C-3）**。← 現在ここ
2. **proptest（C-1）**: `archive::parse` が任意バイト列で panic しないこと。
3. **unwrap/expect 棚卸し（ADR 0015 Phase B）**。
4. **lock 層（Concurrent Access）**。
5. **Zip64 出力**（`commit.rs` の 4 箇所）。
6. **UNTRUSTED ARCHIVES 防御**（`ArchiveLimits` + `parse_with_limits`）。
7. **機能: M4 刻み4（dedup）/ M5（ADR 0014 の VMM-native in-place）**。
8. **example CLI / ドキュメント**。

### 理由

- **1 が最初に来る理由**: 中心的な主張がクラッシュ安全なのに、実際にクラッシュ境界で
  落として再 open するテストが 1 つも無かった（203 テストは全部 in-process unit で、
  `tests/` ディレクトリすら無い）。ここに投資すると**以降の全機能（dedup / in-place /
  lock）が同じハーネスで検証できる**ので、投資が後段すべてに効く。
- **4 が機能（7）より前に来る理由**: lock 層は open / commit / 回復の各フローに
  割り込む**構造に触る最後の大物**。先に機能を積むと lock 導入で全部書き直しになる。
- **8 が最後でよい理由**: 1 の integration test が実質「動く証拠」になるので、
  見せるための工数はそこで大半が済んでいる。

### 既存計画との差分（2 点）

1. **ADR 0015 の B → C を C → B に入れ替えた。** 601 箇所の unwrap/expect を人手で
   三分類するより、proptest / fuzz に「実際に踏める unwrap」を先に探させたほうが安く、
   かつ正確。機械が実弾を教えてくれた後で、残りを机上で分類する。
2. **M4 刻み4 を「次の本筋」から 7 番へ下げた。** 土台が固まる前に機能を足すと
   1 と 4 で全部触り直しになるため。刻み1-3 が完了しているので中断による損失は無い。

### 検証（この順序が正しかったことの実測）

Step 1 に着手した初日に、**サイレントなデータ破損バグを 1 件発見・修正した**
（`6612fcf` / `5ee4b61` の red テスト 2 本 → `9e4fd33` の修正）。
`VmdirtyWriter::pos` の不変条件は「= 現在のファイル長」だが、
`append_commit_marker` だけが `write_record` を通さず直接 `write_all` していたため、
COMMIT MARKER 40 バイト分 `pos` が実位置より手前に取り残されていた。結果:

1. marker 以降に追記した DATA RECORD の `data_offset` がずれ、Tier 2 の読み戻しが壊れる。
2. commit は `rehydrate_into` でその索引を使ってページを Tier 1 へ戻し、`build_full` が
   それを新しい ZIP に焼き込む。つまり**ユーザーデータの代わりにジャーナルの
   レコードヘッダがアーカイブに入る**。実測で `[0x11; 16]` を書いた領域が
   `[75, 144, 164, 211, 248, 221, 235, 6, ...]` になっていた。

既存 203 テストが素通りしていたのは、**回復経路が無事だった**から。`read_vmdirty` の
walk は実ファイル位置からオフセットを再計算するので、クラッシュ後に復元される状態は
正しい。壊れるのは in-session の索引だけ、つまり「クラッシュしなかった場合」にだけ壊れる。
修正は追記の出口を `append_raw(bytes, force_sync)` 1 本に統一し、`pos` を進める場所を
そこだけに閉じ込めた（同じ迂回が二度と書けない形にした）。

もし順序を組み替えず M4 刻み4 へ進んでいたら、このバグは dedup で増えた
`write_hit` 経路の上に積み重なっていた。

### 開発の回し方（ローカルに cargo が無い前提）

- **コードを書いたら push して CI で確認する。** コンパイルエラーも CI で初めて分かる。
- バグ修正は「**先に red なテストを push して再現を示し、次のコミットで直す**」。
  ローカルで回せない以上、CI のログが唯一の証拠になる。
- CI のキャッシュは 7 日参照が無いと evict されるので、週 1 の schedule で温存する。

### 関連

- ADR 0015: Phase A / A-5 は本 ADR の Step 0。B/C の順序は本 ADR で入れ替えた。
- ADR 0017: Step 1 の監査から出た仕様矛盾の決着。
- `HANDOFF.md` の「次の着手候補」は本 ADR に合わせて書き換えてある。

### 未了

- Step 1 の本体（fault injection の seam と、seam 不要のジャーナル異常系）。
- Step 2 以降はすべて未着手。

---

## 0017. FULL commit のクラッシュ窓を commit intent で判定可能にする（設計仕様を修正する）

- 日付: 2026-09-01
- ステータス: 採用（**実装は完了**: 刻み1 = intent の一般化と 3 分岐、刻み2 = FULL への適用。
  設計リポ側の編集は未着手）
- 文脈: ADR 0016 Step 1（クラッシュ注入テスト）の前段として、設計仕様が定めるクラッシュ点と
  再 open 時の期待状態を洗い出したところ、**仕様が自己矛盾している**箇所が見つかった。
  HANDOFF に「残」として挙がっていた「アーカイブ durable 後・vmdirty 削除 durable 前」の
  クラッシュ窓（新アーカイブ + 旧 vmdirty で CONFLICT に見える）が、まさにその箇所。

### 発見: 仕様が三通りの答えを持っている

1. **vmdirty Journal Spec の決定木と provenance 節 → CONFLICT。**
   `open()` は最初の関門で fingerprint（`source_file_size` / `source_inode` /
   `source_cd_hash`）を照合し、「Fingerprint mismatches → vmdirty is stale (source
   changed). CONFLICT state. Caller must resolve.」。決定木では
   `offer: discard() | abort()` のみ。**現在の実装はこちら**（`DefaultRecoveryHandler`
   → `Abort` → `FileMountError::RecoveryRequired`）。
2. **本体仕様の BACKGROUND COMMIT → replay してよい。**
   「Crash after RENAME, before vmdirty deletion: … Recovery at next open() **applies
   vmdirty pages to the compacted archive.zip. This is correct**: FULL compaction does
   not alter logical content; any dirty page … results in an idempotent overwrite with
   the same data.」
3. **本体仕様の同期 FULL commit → 沈黙。**
   「Crash before rename / Crash after rename」しか書いておらず、生き残った vmdirty に
   ついて何も言わない。

FULL commit の `rename` は size / inode / cd_hash の**三つとも**変えるので、(1) の関門が
必ず先に発火する。**(2) は (1) の下では到達不能**であり、両者は調停されていない。

### 消去法: 先に潰れる選択肢

**(B) 順序を変えて窓を無くす — 原理的に不可能。**
vmdirty を先に削除してから rename すると、その間のクラッシュで dirty ページが失われる
（commit 時点で `rehydrate_into` は済んでいるが、その中身はメモリ上にしかない）。
`vmdirty.bak.{gen}` へ先に rename する変種も同じで、「旧アーカイブ + .bak だけ」の状態を
次の open は clean な旧アーカイブとしか読めず、dirty セッションが黙って消える。
**独立した 2 つのオブジェクトを一緒に変えねばならない以上、intent レコード無しには解けない。**

**(C) fingerprint 関門を緩めて冪等 replay を許す（＝仕様 (2) を採る）— 不健全。**

- ページについては (2) の主張は正しい。既に焼かれたページを同じ内容で上書きするだけ。
- しかしジャーナルには **METADATA レコード（CREATE / REMOVE / RENAME / RESIZE）**が入る。
  RENAME を rename 済みのアーカイブへ replay すると、alias が引くべきソース名が既に無い。
  RESIZE-shrink も source high-water の意味論があり、再適用が無害とは言えない。
  **(2) の段落はエントリ操作（④a/④b、ADR 0010）が入る前に書かれており、前提が古い。**
- 加えて fingerprint 関門を一般に緩めると「他人が編集したアーカイブに stale なページを
  適用する」という、関門が防いでいた当の事故を招く。「自分の commit による変化」だけを
  緩めるにはそれを識別する手段が要る ＝ 結局 (A) に戻る。

**(A2) アーカイブ側に commit epoch を持たせる — 却下。**
権威ある場所はアーカイブ自身なので EOCD コメント欄に置くことになるが、ZIP として合法でも
ユーザーから見えるメタデータを汚し、素の再構築とバイト同一でなくなる。

**(A3) vmdirty ヘッダの flag を「superseded」に落としてから rename — 却下。**
flag を立てた直後・rename 前のクラッシュで、実際には superseded でないジャーナルを
superseded と印してしまいデータを失う。「commit 進行中、期待結果は X」と書くならそれは
(A1) そのもの。なお header CRC が flags を覆うため、in-place な flag 書き換えと CRC の
関係は仕様が沈黙している（`RECOVERY_PENDING` ビットに既に存在する瑕疵）。

### 決定

**(A1) アーカイブを変える前に、期待される変化後の状態を durable に記録する**
（write-ahead intent）。`open()` の判断を 1 ビットの fingerprint 比較から 3 分岐にする。

| 実アーカイブの状態 | 判定 |
|---|---|
| = intent が記録した**変化後** fingerprint | commit は完了した → ジャーナルを捨てて CLEAN でマウント（**利用者への質問不要**） |
| = intent が記録した**変化前** fingerprint | commit は起きなかった → 通常の回復プロトコル |
| どちらでもない | 本物の CONFLICT（外部改変）→ 従来どおり discard / abort を提示 |

**窓が無くなるのではなく、窓の中のどの状態も判定可能になる**、という性質の解決である。
intent の書き込み自体にもクラッシュ点はあるが、その範囲のどの状態も曖昧でなくなる。

### intent レコードに載せるもの

- magic
- **変化前** fingerprint（`source_file_size` + `source_cd_hash`）
- **変化後** fingerprint（同上。FULL は `build_full` がメモリ上に新 ZIP を作り終えているので
  rename 前に算出できる。追加 I/O は不要）
- モード（FULL / INCREMENTAL）
- INCREMENTAL 用の `old_len` / `new_len`（truncate ロールバックに要る。ADR 0012 の現行 intent）
- CRC-32C

`inode` を載せないのは、Windows 実装で `inode = 0` 固定であり（`HANDOFF.md` の gotcha）、
移植性のある判定材料にならないため。size + cd_hash で十分。

### 設計リポ側の編集箇所

1. `SIDECAR FILES` のファイル一覧に intent ファイルを追加（寿命と orphan 規則つき）。
   現行一覧は `vmidx` / `vmlock` / `vmdirty` / `vmdirty.bak.{id}` / `vmdirty.compact` /
   `archive.new.zip` のみ。
2. レコード形式の新節（上記フィールド）。
3. `commit() FLOW`: 「intent を書く → fsync → 親 dir fsync → **それから**アーカイブを変える」
   というバリアを明記する。
4. Journal Spec Section 3 の決定木: fingerprint 関門の**前**に intent チェックを置き、
   上の 3 分岐にする。
5. Journal Spec の provenance 節: 「Fingerprint mismatches → CONFLICT」を
   「→ intent を参照し、どちらの fingerprint とも一致しないときだけ CONFLICT」へ。
5b. commit() FLOW の vmidx 更新ステップ: 「modified entries only + fingerprint
   refresh」は **FULL path では成立しない**（全オフセットが動く）。FULL は
   「vmidx を stale のまま残し、次の open で再構築させる」が正しい。INCREMENTAL に
   限って追記分の拡張 + refresh が成立する、と書き分ける（上記「別件」2）。
6. BACKGROUND COMMIT の「after RENAME」段落を intent の言葉で書き直す。
   **「冪等だから replay してよい」という主張は落とす**（上記 (C) の理由により、
   エントリ操作がある今は成り立たない）。

### 実装への影響

- 既存の `commit.intent`（ADR 0012、INCREMENTAL 専用・長さだけ）を**この形式へ一般化**し、
  FULL commit（`durable_replace`）も同じバリアを通す。
- `open_with_recovery` の先頭で行っている「intent を読んで truncate ロールバック」を、
  3 分岐の判定に置き換える。
- rename の前に durability バリアが 1 本増える。

### 副次的に閉じるもの

1. **未記録の設計差分。** `commit.intent` は ADR 0012 で実装が発明したもので、仕様の
   サイドカー一覧に存在しない。本 ADR の編集でこの差分自体が解消される。
2. **判定が「妥当な ZIP か」から「**その** ZIP か」になる。** 旧判定は
   `size == new_len && Archive::parse().is_ok()` で、長さが偶然一致する別の妥当な ZIP を
   「完了」と誤りうる。cd_hash 照合はアーカイブの同一性そのものを見る。

### 訂正 (2026-09-01、刻み1 の実装で判明)

**当初この ADR は「`completed` 判定に穴がある」と書いていたが、それは誤りだった。**

主張はこうだった: 「`find_eocd` は後方走査なので、末尾がゼロだと旧 EOCD を拾って
parse が通り、誤って完了と判定して vmdirty を捨てる」。実際には `find_eocd` は
`off + 22 + comment_len == data.len()` を要求する（`archive.rs`）。末尾にゼロが
付くと旧 EOCD はこの検査で落ち、parse は `NotZip` になる。したがって旧判定でも
**正しくロールバックされていた**。この穴を再現するテストを書こうとして、その
テスト自身のアサート（「ゼロ末尾でも parse が通る」）が CI で落ちたことで判明した。

一方、**刻み1 の実装のほうに本物の欠陥が見つかった**（同じテストを書く過程で）。
3 分岐の「どちらとも一致しない → truncate」を、照合せずに実行していた。INTENT と
無関係なアーカイブ（別 commit の結果や外部の差し替え）が置かれていた場合、
**先に切り詰めてから気づく**ことになる。INCREMENTAL は旧バイト `[0, pre_size)` を
一切書き換えないので、未完の追記ならその前置きは必ず妥当な旧 ZIP になっている。
`prefix_is_pre` でそれを**先に**確かめ、一致したときだけ truncate する。一致しなければ
何も触らず INTENT も残し、後段の fingerprint 関門で CONFLICT に倒す。

教訓として残す: **穴を主張するテストは、その穴の前提条件自体を assert すること。**
今回それを書いていたおかげで、前提が偽であることが CI で分かった。

### 設計リポを触る方針について

`SPEC_DIVERGENCE.md` は「**意図的な差分**を実装側に残す」ための場所であって、
**仕様側の欠陥・自己矛盾はここに溜めない**。上流を直す。前例として DEFLATE
チェックポイントの `bits` 追加（設計リポ `4aa0aa0`）と、in-place commit の CRC-32
maintenance 節の追加（同 `406f558`、ADR 0014）がある。本件は後者と同種で、
仕様が自分自身と矛盾している以上、実装側に「差分」として記録しても意味を成さない。

### 同じ監査で挙がった別件（本 ADR の対象外・要対応）

1. **orphan `archive.new.zip` が `open()` で削除されない。** 仕様は本体・Journal Spec の
   2 箇所で「無ければ silently remove」と要求している。`vmdirty.compact` の掃除は
   実装済みだが `.new` だけ抜けている。1 行修正で済む。
2. ~~commit 後に vmidx の fingerprint を更新しない（仕様は refresh を要求）~~
   → **仕様側の欠陥であり、実装は正しい**（2026-09-02 に確認）。仕様は commit()
   FLOW で「Update vmidx **for modified entries only**, and refresh the vmidx
   header fingerprint」と書くが、**FULL commit は全エントリのローカルヘッダ
   オフセットを動かす**（未変更エントリも verbatim コピーだが、前のエントリの
   サイズが変われば配置は変わる）。これを FULL で実行すると、移動したオフセットを
   指したまま「このアーカイブに一致する」と主張する vmidx ができ、読み取りが
   ゴミを返す。現在の実装（stale のまま残し、次の open で不一致を検出して再構築）は
   正しく安全で、遅いだけ。**この項目は「実装が追いつくべき」から「設計リポ側の
   編集」へ移す**（下記 5b）。INCREMENTAL では未変更エントリのオフセットが不変なので、
   そちらに限れば仕様の手順は成立する。
3. ~~`.vmm` の `mkdir` 後に親 dir fsync をしていない（仕様は要求）~~ → 完了。
   `create_sidecar_dir` に括り出し、新規作成時のみ親 dir を fsync する。
4. compaction の `rename` 失敗時、実装は temp パスへジャーナリングを続ける
   （仕様は沈黙）。その後のクラッシュで書き込みが失われる。

### 関連

- ADR 0012: `commit.intent` の初出（INCREMENTAL 専用）。本 ADR はその一般化。
- ADR 0010: エントリ操作（④a/④b）。(C) が成り立たなくなった原因。
- ADR 0007: `O_DSYNC` → `sync_data()`。durability バリアの実現手段。
- ADR 0016: この監査を Step 1 として位置づけた順序。

### 実装の進捗

- ~~刻み1: intent を version 2（60 バイト）へ一般化し、`open()` を 3 分岐に~~ 完了。
- ~~刻み2: FULL commit にも intent を通す（`durable_replace` の前）~~ 完了。
  **これでクラッシュ窓 b6 が閉じた。**
- 後始末の順序を `finish_commit_cleanup` に括り出し、**vmdirty の削除を durable に
  してから INTENT を消す**ようにした。1 度の dir fsync にまとめると「新アーカイブ +
  旧 vmdirty + INTENT 無し」が観測でき、閉じたはずの窓が再び開くため（監査項目 a8）。
  代償は dir fsync 1 回。Windows では両方 no-op なのでこの順序保証は Unix でのみ実効する。
- **示せていないこと**: 「INTENT がアーカイブに触れる**前に**書かれている」という
  時間的性質。現在のテストは状態を手で組んでいるので、順序そのものは検証していない。
  ADR 0016 Step 1 本体の fault injection seam が要る。

### 未了

- 設計リポ側の編集 1〜6。
- 上記「別件」2〜4（1 の orphan `archive.zip.new` 掃除は完了）。
- 監査で作ったクラッシュ点カタログ（(a) INCREMENTAL 9 点 / (b) FULL 8 点 /
  (c) ジャーナル 11 点 / (d) compaction 9 点）をテストへ落とす作業。seam 不要で書けるものが
  多く（ジャーナルの torn record / 壊れヘッダ / marker 無し / 空ジャーナル、compaction の
  orphan 掃除）、そこから着手する。
