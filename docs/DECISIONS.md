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
