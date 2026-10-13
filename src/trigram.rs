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

/// Every trigram a Unicode case-insensitive match of the ASCII trigram
/// `t` puts in a file, for -i lookups. Under Unicode simple case folding
/// (both engines' -i) an ASCII letter matches its two ASCII cases, and
/// `k` and `s` also match KELVIN SIGN (U+212A) and LONG S (U+017F), which
/// are multibyte. For each way the three characters can match, the
/// window starting at the last byte of the first one is a substring of
/// the matched text, so a file holding a match holds one of these. At
/// most 27.
pub fn fold_variants(t: u32) -> Vec<u32> {
    let options = |b: u8| -> Vec<Vec<u8>> {
        let mut o = vec![vec![b.to_ascii_lowercase()]];
        if b.is_ascii_alphabetic() {
            o.push(vec![b.to_ascii_uppercase()]);
        }
        match b.to_ascii_lowercase() {
            b'k' => o.push("\u{212A}".as_bytes().to_vec()),
            b's' => o.push("\u{17F}".as_bytes().to_vec()),
            _ => {}
        }
        o
    };
    let [a, b, c] = [(t >> 16) as u8, (t >> 8) as u8, t as u8].map(options);
    let mut out = Vec::new();
    for x in &a {
        for y in &b {
            for z in &c {
                let text = [&x[..], y, z].concat();
                let start = x.len() - 1;
                out.push(pack(&text[start..start + 3]));
            }
        }
    }
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

    #[test]
    fn fold_variants_cover_kelvin_and_long_s() {
        // No k or s: exactly the ASCII case variants.
        assert_eq!(fold_variants(pack(b"a1b")), case_variants(pack(b"a1b")));
        let v = fold_variants(pack(b"kfr"));
        assert!(v.contains(&pack(b"kfr")) && v.contains(&pack(b"KFR")));
        // KELVIN SIGN (E2 84 AA) first: its last byte and the next two.
        assert!(v.contains(&pack(b"\xaafr")) && v.contains(&pack(b"\xaaFr")));
        assert_eq!(v.len(), 12);
        let v = fold_variants(pack(b"ask"));
        // LONG S (C5 BF) in the middle, KELVIN SIGN last.
        assert!(v.contains(&pack(b"a\xc5\xbf")));
        assert!(v.contains(&pack(b"as\xe2")) && v.contains(&pack(b"AS\xe2")));
        // Every way "kiss" can match holds one of each trigram's variants.
        for text in ["kiss", "\u{212A}i\u{17F}s", "KI\u{17F}\u{17F}", "Kiss"] {
            let tris = extract(text.as_bytes());
            for t in [pack(b"kis"), pack(b"iss")] {
                assert!(
                    fold_variants(t).iter().any(|v| tris.contains(v)),
                    "{text}: {t:06x}"
                );
            }
        }
    }
}
