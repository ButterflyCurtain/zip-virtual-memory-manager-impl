# ZIP Virtual Memory Manager — Implementation

**English** | [日本語](#japanese)

Implementation of the "ZIP Virtual Memory Manager": treating a ZIP archive as a
backing store for virtual memory, with random read/write access into compressed
entries, copy-on-write buffering, and crash-safe commits.

The design specifications this implementation is based on live in a separate
repository:

- **Design documents:** https://github.com/ButterflyCurtain/zip-virtual-memory-manager

## Status

Early work in progress. See the design repository above for the full
specification.

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

着手したばかりです。仕様の全体像は上記の設計リポジトリを参照してください。
