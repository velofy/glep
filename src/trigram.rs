/// A trigram is 3 consecutive raw bytes packed into the low 24 bits of a u32.
pub fn pack(b: &[u8]) -> u32 {
    (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32
}

/// All distinct trigrams in `content`, sorted ascending.
pub fn extract(content: &[u8]) -> Vec<u32> {
    if content.len() < 3 {
        return Vec::new();
    }
    let mut tris: Vec<u32> = content.windows(3).map(pack).collect();
    tris.sort_unstable();
    tris.dedup();
    tris
}

/// Every ASCII case combination of a trigram, for -i lookups. At most 8.
pub fn case_variants(t: u32) -> Vec<u32> {
    let bytes = [(t >> 16) as u8, (t >> 8) as u8, t as u8];
    let mut variants: Vec<Vec<u8>> = vec![Vec::new()];
    for b in bytes {
        let mut next = Vec::new();
        for v in &variants {
            let mut lo = v.clone();
            lo.push(b.to_ascii_lowercase());
            next.push(lo);
            if b.is_ascii_alphabetic() {
                let mut up = v.clone();
                up.push(b.to_ascii_uppercase());
                next.push(up);
            }
        }
        variants = next;
    }
    let mut out: Vec<u32> = variants.iter().map(|v| pack(v)).collect();
    out.sort_unstable();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_is_big_endian_bytes() {
        assert_eq!(pack(b"abc"), 0x616263);
    }

    #[test]
    fn extract_windows_sorted_deduped() {
        // "abcabc" windows: abc, bca, cab, abc, bca -> {abc, bca, cab}
        let tris = extract(b"abcabc");
        assert_eq!(tris, vec![pack(b"abc"), pack(b"bca"), pack(b"cab")]);
    }

    #[test]
    fn extract_short_input_is_empty() {
        assert!(extract(b"ab").is_empty());
        assert!(extract(b"").is_empty());
    }

    #[test]
    fn case_variants_two_letters_one_digit() {
        // "a1b" -> a/A x 1 x b/B = 4 variants
        let v = case_variants(pack(b"a1b"));
        assert_eq!(v.len(), 4);
        assert!(v.contains(&pack(b"A1B")));
        assert!(v.contains(&pack(b"a1b")));
    }

    #[test]
    fn case_variants_no_letters_is_identity() {
        assert_eq!(case_variants(pack(b"123")), vec![pack(b"123")]);
    }
}
