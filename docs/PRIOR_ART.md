# Prior Art and Patent Landscape

_Last updated: 2026-06-19. This document is a factual engineering note, not legal
advice. For any concrete legal question, consult a qualified patent attorney._

## Purpose

The ZIP Virtual Memory Manager (zip-vmm) does **not** claim invention of any of
its core mechanisms. It is an engineering combination of well-established,
publicly documented techniques. This document records that prior art explicitly,
both for honesty and as a defensive-publication record.

## No novelty claimed

Every layer of zip-vmm maps onto an existing, widely-known technique:

| zip-vmm component | Established prior art it builds on |
|---|---|
| `vmidx` seek index (checkpointed random access into a DEFLATE stream) | zlib's `examples/zran.c` by Mark Adler — the canonical technique of periodically snapshotting inflate state so decoding can resume from an arbitrary point. Reused by `indexed_gzip`, `zindex`, `gztool`, and the `zran` package on PyPI. The design notes explicitly say to read `zran.c` first. |
| Copy-on-write diff layer over an immutable base (Tier 1 / Tier 2) | Backing-file / overlay model of QEMU `qcow2`, Linux OverlayFS, Docker/OCI image layers, and union mounts: writes are buffered in an overlay and the base is never mutated until an explicit flatten/commit. |
| `vmdirty` journal (crash-safe commit via write-ahead logging + replay) | Write-ahead logging as in SQLite WAL and PostgreSQL WAL; the redo-on-recovery discipline is ARIES-style. |
| Page cache with LRU eviction | Standard operating-system / database buffer-cache technique. |
| FULL commit (`archive.new.zip` then `rename()`) | The classic POSIX atomic-replace idiom (write a temp file, `fsync`, atomically `rename` over the original). |

In short: checkpointed inflate (Adler), COW overlays (qcow2/OverlayFS), and WAL
(SQLite/ARIES) are mature, decades-old ideas. zip-vmm's contribution is the
specific assembly for random-access read/write over ZIP archives, not any
underlying primitive.

## Patent landscape

One patent is worth recording because part of the project's roadmap is
conceptually adjacent to it.

- **US 8,024,382 B2 — "Dynamic Manipulation of Archive Files"** (Autodesk, Inc.;
  filed 2009-01-20; granted 2011-09-20; estimated expiry ~2030-03 per Google
  Patents).
- **Territory:** United States only. No JP / EP / PCT family member is on record,
  so there is no corresponding patent in Japan.
- **What the independent claims (1 and 12) require, in essence:** editing a ZIP
  archive *in place* — saving a modified block back into the archive **without
  rewriting the whole file**; if the block no longer fits, copying a neighbouring
  block to the end of the data section and marking the vacated space as a **free
  block**; then updating the central directory and end-of-central-directory.
  Dependent claims add the **free-block (low/high watermark) list** (claims 6–7,
  17–18) and **soft/hard delete with recovery** (claims 9–11, 20–22).

### How the project relates to it

- **M1 (read core), M2 (FULL commit), M3 (durability journal), and the entry
  operations (create / remove / truncate / rename) are outside these claims.**
  The FULL commit path deliberately writes a brand-new archive and `rename()`s it
  — i.e. it *rewrites the entire archive*, which is precisely the prior-art
  behaviour the patent distinguishes itself against (see the patent's FIG. 9A).
  None of these milestones performs in-place block editing or free-block reuse.
- **M4 (INCREMENTAL commit + Dead Space Freelist) is conceptually adjacent.** Its
  planned hole-reuse + end-of-file append + central-directory update overlaps with
  the in-place-edit and free-block-list ideas of the independent and free-block
  claims. Note a contextual difference: in the patent, orphaned space arises from
  directly editing entries; in zip-vmm, "dead space" is the residue of prior
  INCREMENTAL commits, and all writes go to the diff layer first (the archive is
  touched only at commit). Whether that difference falls inside or outside the
  claim language is a matter of claim construction and is not resolved here.
- A formal Freedom-to-Operate (FTO) analysis has **not** been performed.

This section is descriptive only. It is not an admission of infringement of any
claim, nor legal advice.

## References

- M. Adler, zlib `examples/zran.c`. <https://github.com/madler/zlib/blob/master/examples/zran.c>
- `indexed_gzip`. <https://github.com/pauldmccarthy/indexed_gzip>
- `zindex`. <https://github.com/mattgodbolt/zindex>
- `gztool`. <https://github.com/circulosmeos/gztool>
- QEMU qcow2 backing files / OverlayFS / OCI image layers (general background).
- SQLite Write-Ahead Logging. <https://www.sqlite.org/wal.html>
- C. Mohan et al., "ARIES: A Transaction Recovery Method…", ACM TODS, 1992.
- US 8,024,382 B2. <https://patents.google.com/patent/US8024382B2/>
