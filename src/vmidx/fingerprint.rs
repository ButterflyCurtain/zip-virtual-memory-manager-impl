//! fingerprint 照合（vmidx Section 7 step 3 = Diff Layer 仕様 Section 2）。
//!
//! open() 時、vmidx ヘッダに記録した fingerprint を archive.zip の現在値と
//! 照合して、インデックスがまだそのアーカイブを表しているかを判定する。
//! 単一メトリック（mtime だけ / inode だけ）はファイルシステムやネットワーク
//! マウントを跨ぐと当てにならないため、コスト順のカスケードを使う:
//!
//! - **step1 FAST（O(1)）**: file_size と inode。一致でもしなくても step2 へ。
//!   inode 不一致は内容差の証拠にならない（inode 再利用、バックアップ復元、
//!   AOT 配布インデックスは同一バイトでも別 inode になる）。
//! - **step2 MEDIUM（O(cd_size)）**: cd_hash（Central Directory ブロックの
//!   XXH3-128）。**ここが妥当性の確定要因。** 一致＝有効。step1 が食い違って
//!   いたらヘッダの file_size/inode/mtime_ns をローカル値へ refresh する
//!   （in-place、Section 8）。不一致＝無効。
//! - **step3**: mtime_ns はヘッダに記録するが open() の妥当性ゲートには使わない
//!   （NFS/FAT 等で不安定）。実行時 ESTALE チェックの変化トリガとしてのみ働く。
//!
//! strict-fingerprint モード（毎 open() で全体 XXH3-128）は対象外。
//! 無効時の CONFLICT（vmdirty 在りで再構築できない）か再構築かの判断は
//! マウント層の責務で、この関数は判定（verdict）だけを返す。
//!
//! cd_hash は事故的破損・帯域外改変に対する整合性チェックであって MAC では
//! ない（真正性は対象外）。XXH3-128 はスループットのために選択。

use super::Header;

/// XXH3-128 のオンディスク表現長（バイト）。`source_cd_hash` の先頭 16 バイトに
/// 入り、残り 4 バイトはゼロ詰め。
pub const CD_HASH_SIZE: usize = 16;

/// archive.zip の現在の stat 値と、計算した Central Directory ハッシュ。
/// CD ブロックの所在特定は `archive` レイヤの責務で、ここには算出済みの値を渡す。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceStat {
    pub file_size: u64,
    pub inode: u64,
    pub mtime_ns: u64,
    /// Central Directory ブロックの XXH3-128（[`hash_cd_block`]）。
    pub cd_hash: [u8; CD_HASH_SIZE],
}

/// Central Directory ブロックの XXH3-128 を、オンディスク表現（リトルエンディアン
/// 16 バイト）で返す。インデックス構築時と open() 照合時で同じ関数を使う限り、
/// 表現の取り決めは内部に閉じる。
pub fn hash_cd_block(cd_block: &[u8]) -> [u8; CD_HASH_SIZE] {
    xxhash_rust::xxh3::xxh3_128(cd_block).to_le_bytes()
}

/// fingerprint 照合の判定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FingerprintVerdict {
    /// 完全一致。マウント可、ヘッダ更新は不要。
    Valid,
    /// cd_hash は一致するが file_size/inode が食い違う（AOT 配布 / inode 再利用 /
    /// バックアップ復元）。マウントは可だが、ヘッダの file_size/inode/mtime_ns を
    /// ローカル値へ refresh すべき（[`refresh_header`]、Section 2 step2 / Section 8）。
    ValidStale,
    /// cd_hash 不一致。vmidx 無効 → 破棄して再構築（vmdirty 在りなら呼び出し側で
    /// CONFLICT 判定）。
    Invalid,
}

/// open() の fingerprint 照合カスケード。strict-fingerprint（全体ハッシュ）は
/// 対象外。
pub fn check_fingerprint(header: &Header, live: &SourceStat) -> FingerprintVerdict {
    // step2 MEDIUM: cd_hash が確定要因。source_cd_hash は 16B ハッシュ + 4B ゼロ詰め。
    if header.source_cd_hash[..CD_HASH_SIZE] != live.cd_hash {
        return FingerprintVerdict::Invalid;
    }
    // step1 FAST: cd_hash 一致のうえで size+inode を見る。食い違えば、内容は同一
    // だがメタデータが変わったケース（refresh が必要）。
    let fast_match =
        header.source_file_size == live.file_size && header.source_inode == live.inode;
    if fast_match {
        FingerprintVerdict::Valid
    } else {
        FingerprintVerdict::ValidStale
    }
}

/// ヘッダの file_size/inode/mtime_ns をローカルの stat 値へ更新する
/// （[`FingerprintVerdict::ValidStale`] への応答）。cd_hash は内容が同一なので
/// 触らない。呼び出し側はこの後ヘッダを再エンコード（CRC 再計算）して
/// 128 バイトを in-place で書き戻す。
pub fn refresh_header(header: &mut Header, live: &SourceStat) {
    header.source_file_size = live.file_size;
    header.source_inode = live.inode;
    header.source_mtime_ns = live.mtime_ns;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header_for(cd_block: &[u8]) -> Header {
        Header {
            flags: 0,
            page_size: 4096,
            checkpoint_interval: 1 << 20,
            source_file_size: 1000,
            source_inode: 42,
            source_mtime_ns: 1_700_000_000,
            source_cd_hash: {
                let mut h = [0u8; 20];
                h[..CD_HASH_SIZE].copy_from_slice(&hash_cd_block(cd_block));
                h
            },
            entry_count: 0,
            entry_table_offset: 128,
            name_heap_offset: 128,
            name_heap_size: 0,
            advisory_offset: 128,
            advisory_size: 0,
        }
    }

    fn live_for(cd_block: &[u8]) -> SourceStat {
        SourceStat {
            file_size: 1000,
            inode: 42,
            mtime_ns: 1_700_000_000,
            cd_hash: hash_cd_block(cd_block),
        }
    }

    #[test]
    fn valid_when_everything_matches() {
        let cd = b"central directory bytes";
        let h = header_for(cd);
        let live = live_for(cd);
        assert_eq!(check_fingerprint(&h, &live), FingerprintVerdict::Valid);
    }

    #[test]
    fn invalid_when_cd_hash_differs() {
        let h = header_for(b"original cd");
        // size/inode は一致させても、cd_hash 不一致で無効。
        let live = live_for(b"modified cd");
        assert_eq!(check_fingerprint(&h, &live), FingerprintVerdict::Invalid);
    }

    #[test]
    fn stale_when_cd_matches_but_inode_differs() {
        let cd = b"same content";
        let h = header_for(cd);
        let mut live = live_for(cd);
        live.inode = 999; // inode 再利用 / 復元
        assert_eq!(check_fingerprint(&h, &live), FingerprintVerdict::ValidStale);
    }

    #[test]
    fn stale_when_cd_matches_but_size_differs() {
        // file_size 不一致でも cd_hash が一致するのは普通あり得ないが、
        // FAST チェック単独では確定しないという仕様の性質をテストで固定する。
        let cd = b"same content";
        let h = header_for(cd);
        let mut live = live_for(cd);
        live.file_size = 2000;
        assert_eq!(check_fingerprint(&h, &live), FingerprintVerdict::ValidStale);
    }

    #[test]
    fn mtime_is_not_a_validity_gate() {
        let cd = b"same content";
        let h = header_for(cd);
        let mut live = live_for(cd);
        live.mtime_ns = 0; // mtime が食い違っても、size+inode+cd 一致なら Valid。
        assert_eq!(check_fingerprint(&h, &live), FingerprintVerdict::Valid);
    }

    #[test]
    fn refresh_updates_metadata_not_cd_hash() {
        let cd = b"same content";
        let mut h = header_for(cd);
        let original_cd = h.source_cd_hash;
        let live = SourceStat {
            file_size: 2000,
            inode: 999,
            mtime_ns: 1_800_000_000,
            cd_hash: hash_cd_block(cd),
        };
        refresh_header(&mut h, &live);
        assert_eq!(h.source_file_size, 2000);
        assert_eq!(h.source_inode, 999);
        assert_eq!(h.source_mtime_ns, 1_800_000_000);
        assert_eq!(h.source_cd_hash, original_cd); // 内容ハッシュは不変
        // refresh 後は完全一致になる。
        assert_eq!(check_fingerprint(&h, &live), FingerprintVerdict::Valid);
    }

    #[test]
    fn hash_cd_block_is_deterministic_and_content_sensitive() {
        assert_eq!(hash_cd_block(b"abc"), hash_cd_block(b"abc"));
        assert_ne!(hash_cd_block(b"abc"), hash_cd_block(b"abd"));
    }
}
