const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

pub fn hash64(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    for &byte in bytes {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    mix(hash)
}

fn mix(mut hash: u64) -> u64 {
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xff51_afd7_ed55_8ccd);
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    hash ^= hash >> 33;
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_are_pinned() {
        assert_eq!(hash64(b""), 0xefd0_1f60_ba99_2926);
        assert_eq!(hash64(b"a"), 0x82a2_a958_a9be_ce5b);
        assert_eq!(hash64(b"cat"), 0x98e2_5a30_2c6e_b1d4);
        assert_eq!(hash64(b"key00000042"), 0x2b41_3d9f_a190_dbb1);
    }

    #[test]
    fn is_deterministic() {
        assert_eq!(hash64(b"lsmrs"), hash64(b"lsmrs"));
    }

    #[test]
    fn distinguishes_by_length_not_just_content() {
        assert_ne!(hash64(b"ab"), hash64(b"ab\0"));
        assert_ne!(hash64(b""), hash64(b"\0"));
    }

    #[test]
    fn both_halves_avalanche() {
        for i in 0..256u32 {
            let a = hash64(format!("key{:08}", i).as_bytes());
            let b = hash64(format!("key{:08}", i + 1).as_bytes());

            let low_flipped = (a as u32 ^ b as u32).count_ones();
            let high_flipped = ((a >> 32) as u32 ^ (b >> 32) as u32).count_ones();

            assert!(
                low_flipped >= 6,
                "key{:08}: low half moved {low_flipped}",
                i
            );
            assert!(
                high_flipped >= 6,
                "key{:08}: high half moved {high_flipped}",
                i
            );
        }
    }
}
