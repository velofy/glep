use crate::trigram;

#[derive(Debug, PartialEq)]
pub enum Plan {
    /// OR of AND-groups: a file is a candidate if, for some group,
    /// it contains every trigram in that group.
    Groups(Vec<Vec<u32>>),
    /// The index cannot narrow this pattern; scan all indexed files.
    All,
}

/// How the matcher compares letters, which decides the trigrams of a
/// required literal that the index can look up (`Index::postings_for`
/// looks a trigram up as is, or, case-insensitively, as every ASCII case
/// variant of it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Case {
    /// Byte for byte: every trigram is looked up as is.
    Sensitive,
    /// ASCII case folding only (--no-unicode): the ASCII variants of an
    /// all-ASCII trigram cover every match of it.
    Ascii,
    /// Unicode simple case folding, both engines' -i (and -S) in their
    /// default Unicode mode. ASCII letters fold among themselves except
    /// two: `k`/`K` also match KELVIN SIGN (U+212A) and `s`/`S` LONG S
    /// (U+017F), multibyte characters no ASCII variant covers, so a
    /// trigram holding one of them, or any non-ASCII byte, cannot be
    /// looked up.
    Unicode,
}

impl Case {
    /// Whether lookups use the ASCII case variants of each trigram.
    pub fn insensitive(self) -> bool {
        self != Case::Sensitive
    }

    /// Whether every match of the literal bytes `t` contains one of the
    /// index's lookups for them.
    fn covers(self, t: &[u8]) -> bool {
        match self {
            Case::Sensitive => true,
            Case::Ascii => t.is_ascii(),
            Case::Unicode => t
                .iter()
                .all(|b| b.is_ascii() && !matches!(b, b'k' | b'K' | b's' | b'S')),
        }
    }
}

/// The trigrams of a required literal the index can look up under `case`.
fn trigrams_of(lit: &[u8], case: Case) -> Vec<u32> {
    lit.windows(3)
        .filter(|w| case.covers(w))
        .map(trigram::pack)
        .collect()
}

/// A piece of a required-literal group. `Lit` carries a substring that
/// must appear in any match plus edge flags saying whether a literal
/// neighbor may fuse into it; `Boundary` contributes nothing but blocks
/// fusion (e.g. `.*`, `x?`, lookarounds, unexpandable classes) so the
/// literals on either side are known-required but not adjacent.
#[derive(Clone, Debug)]
enum Piece {
    Boundary,
    Lit {
        fuse_left: bool,
        fuse_right: bool,
        s: Vec<u8>,
    },
}

/// DNF: OR of groups; each group is a sequence of pieces (all literals in
/// a group are AND-required for a match that takes that alternative).
type Dnf = Vec<Vec<Piece>>;

/// Cap on expanded variants (class ranges, alternation products,
/// repetition multiplicities). Past it the element degrades to
/// `Boundary`, which is always sound: it just narrows less.
const MAX_VARIANTS: usize = 64;

fn anything() -> Dnf {
    vec![vec![Piece::Boundary]]
}

fn lit(s: Vec<u8>) -> Dnf {
    vec![vec![Piece::Lit {
        fuse_left: true,
        fuse_right: true,
        s,
    }]]
}

/// Merge group `a` and `b` for concatenation; when `a`'s last literal
/// fuses right and `b`'s first fuses left, their strings concatenate
/// (longer literals produce better trigram narrowing).
fn concat_groups(a: &[Piece], b: &[Piece]) -> Vec<Piece> {
    let mut out = a.to_vec();
    match (out.last_mut(), b.first()) {
        (
            Some(Piece::Lit {
                fuse_right: true,
                s: ref mut s0,
                ..
            }),
            Some(Piece::Lit {
                fuse_left: true,
                s: s1,
                fuse_right,
            }),
        ) => {
            s0.extend_from_slice(s1);
            if !fuse_right {
                // Keep the fused literal's right edge closed.
                if let Some(Piece::Lit { fuse_right: fr, .. }) = out.last_mut() {
                    *fr = false;
                }
            }
            out.extend_from_slice(&b[1..]);
        }
        _ => out.extend_from_slice(b),
    }
    out
}

fn concat(dnfs: Vec<Dnf>) -> Dnf {
    let mut acc: Dnf = vec![Vec::new()];
    for mut d in dnfs {
        if acc.len().saturating_mul(d.len()) > MAX_VARIANTS {
            // Product would explode past the cap: degrade THIS element to
            // a boundary. The accumulated groups keep their literals:
            // sound, just less fused.
            d = anything();
        }
        let mut next = Vec::with_capacity(acc.len() * d.len());
        for g in &acc {
            for e in &d {
                next.push(concat_groups(g, e));
            }
        }
        acc = next;
    }
    acc
}

/// Bounded expansion of a class to single-character alternatives.
/// Bytes ≥0x80 are skipped: trigram packs are byte-oriented and a
/// multibyte literal fused from a class edge would be unsound anyway.
fn class_alts(class: &regex_syntax::hir::Class) -> Dnf {
    use regex_syntax::hir::Class;
    let mut alts: Vec<Vec<u8>> = Vec::new();
    match class {
        Class::Unicode(uc) => {
            for r in uc.ranges() {
                for c in r.start()..=r.end() {
                    let mut b = [0u8; 4];
                    alts.push(c.encode_utf8(&mut b).as_bytes().to_vec());
                    if alts.len() > MAX_VARIANTS {
                        return anything();
                    }
                }
            }
        }
        Class::Bytes(bc) => {
            for r in bc.ranges() {
                for b in r.start()..=r.end() {
                    alts.push(vec![b]);
                    if alts.len() > MAX_VARIANTS {
                        return anything();
                    }
                }
            }
        }
    }
    if alts.is_empty() {
        return anything();
    }
    alts.into_iter().flat_map(lit).collect()
}

/// The required-literal analysis. Soundness invariant: for every string
/// that can match `hir`, every literal in at least one returned group is
/// a substring of it. Anything unrecognized is `Boundary`, conservatively.
fn req(hir: &regex_syntax::hir::Hir) -> Dnf {
    use regex_syntax::hir::HirKind;
    match hir.kind() {
        // Empty contributes nothing but is NOT a boundary: literals on
        // either side remain adjacent (`x(?:)y` is just "xy").
        HirKind::Empty => vec![Vec::new()],
        HirKind::Literal(l) => lit(l.0.to_vec()),
        HirKind::Class(c) => class_alts(c),
        HirKind::Look(_) => anything(),
        HirKind::Repetition(r) => {
            // A repetition desugars to `min` required copies concatenated,
            // plus a fusion-closing boundary whenever the tail is optional
            // (unbounded or max > min): a further copy may interpose bytes,
            // so `r+`'s right edge must not fuse into its neighbor.
            if r.min == 0 {
                return anything();
            }
            let sub = req(&r.sub);
            let copies: Vec<Dnf> = std::iter::repeat_n(sub, r.min as usize).collect();
            let mut d = concat(copies);
            if r.max.is_none_or(|max| max > r.min) {
                d = concat(vec![d, anything()]);
            }
            d
        }
        HirKind::Capture(c) => req(&c.sub),
        HirKind::Concat(subs) => concat(subs.iter().map(req).collect()),
        HirKind::Alternation(subs) => subs.iter().flat_map(req).collect(),
    }
}

/// Turn a requirement group into trigrams; None when the group is
/// vacuous (no literal holds a trigram the index can look up under
/// `case`), which must fail the whole plan to All because that
/// alternative constrains nothing.
fn group_trigrams(group: &[Piece], case: Case) -> Option<Vec<u32>> {
    let mut tris = Vec::new();
    for p in group {
        if let Piece::Lit { s, .. } = p {
            tris.extend(trigrams_of(s, case));
        }
    }
    if tris.is_empty() {
        return None;
    }
    tris.sort_unstable();
    tris.dedup();
    Some(tris)
}

/// The index plan for `pattern`. The required literals come from the
/// pattern as written; under a case-insensitive `case` each literal
/// character matches its case-fold partners too, so only the trigrams
/// whose every match `case` covers are kept (see `Case`).
pub fn build(pattern: &str, fixed: bool, case: Case) -> Plan {
    let dnf: Dnf = if fixed {
        lit(pattern.as_bytes().to_vec())
    } else {
        let hir = match regex_syntax::parse(pattern) {
            Ok(h) => h,
            Err(_) => return Plan::All, // engine will surface the real error
        };
        req(&hir)
    };

    let mut groups = Vec::with_capacity(dnf.len());
    for g in &dnf {
        match group_trigrams(g, case) {
            Some(t) => groups.push(t),
            None => return Plan::All,
        }
    }
    if groups.is_empty() {
        return Plan::All;
    }
    // Expanded classes can leave many groups with identical trigram sets
    // (a 1-byte class char contributes none); collapse duplicates.
    groups.sort();
    groups.dedup();
    Plan::Groups(groups)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trigram::pack;

    #[test]
    fn fixed_string_is_single_and_group() {
        match build("hello", true, Case::Sensitive) {
            Plan::Groups(g) => {
                assert_eq!(g.len(), 1);
                assert!(g[0].contains(&pack(b"hel")));
                assert!(g[0].contains(&pack(b"llo")));
            }
            Plan::All => panic!("expected groups"),
        }
    }

    #[test]
    fn literal_regex_extracts_trigrams() {
        match build("fn main", false, Case::Sensitive) {
            Plan::Groups(g) => assert!(g[0].contains(&pack(b"mai"))),
            Plan::All => panic!("expected groups"),
        }
    }

    #[test]
    fn alternation_yields_or_groups() {
        match build("foobar|bazqux", false, Case::Sensitive) {
            Plan::Groups(g) => {
                assert_eq!(g.len(), 2);
                assert!(g[0].contains(&pack(b"foo")) || g[1].contains(&pack(b"foo")));
            }
            Plan::All => panic!("expected groups"),
        }
    }

    #[test]
    fn unnarrowing_patterns_are_all() {
        assert!(matches!(build(".*", false, Case::Sensitive), Plan::All));
        assert!(matches!(build("a", false, Case::Sensitive), Plan::All));
        assert!(matches!(build("ab", true, Case::Sensitive), Plan::All));
        // unparseable: fall back
        assert!(matches!(build("[", false, Case::Sensitive), Plan::All));
        // `x?yz`: yz is required but too short for trigrams, so vacuous.
        assert!(matches!(build("x?yz", false, Case::Sensitive), Plan::All));
    }

    #[test]
    fn anything_arm_still_contributes_required_tail() {
        // `(foo|.*)bar`: every match ends in bar regardless of the arm, so
        // narrowing on `bar` is sound.
        match build("(foo|.*)bar", false, Case::Sensitive) {
            Plan::Groups(g) => {
                assert!(g.iter().any(|grp| grp.contains(&pack(b"bar"))));
            }
            Plan::All => panic!("expected groups"),
        }
    }

    #[test]
    fn prefix_of_wildcard_pattern_still_narrows() {
        match build("needle.*", false, Case::Sensitive) {
            Plan::Groups(g) => assert!(g[0].contains(&pack(b"nee"))),
            Plan::All => panic!("expected groups"),
        }
    }

    #[test]
    fn literal_after_unexpandable_part_still_narrows() {
        // `-foo` is required even though the digit run is too big to expand.
        match build("[0-9][0-9][0-9]-foo", false, Case::Sensitive) {
            Plan::Groups(g) => {
                assert_eq!(g.len(), 1);
                assert!(g[0].contains(&pack(b"-fo")));
            }
            Plan::All => panic!("required literal should still narrow"),
        }
    }

    #[test]
    fn literals_split_across_boundary_both_required() {
        // `abc\w+def`: every match contains abc AND def: one group, both lit
        // sets (stronger than keeping only the prefix).
        match build("abc\\w+def", false, Case::Sensitive) {
            Plan::Groups(g) => {
                assert_eq!(g.len(), 1);
                assert!(g[0].contains(&pack(b"abc")));
                assert!(g[0].contains(&pack(b"def")));
            }
            Plan::All => panic!("expected groups"),
        }
    }

    #[test]
    fn alternation_inside_concat_fuses_variants() {
        match build("a[bc]d", false, Case::Sensitive) {
            Plan::Groups(g) => {
                assert_eq!(g.len(), 2);
                // fused full strings abd / acd
                assert!(g.iter().any(|grp| grp.contains(&pack(b"abd"))));
                assert!(g.iter().any(|grp| grp.contains(&pack(b"acd"))));
            }
            Plan::All => panic!("expected groups"),
        }
    }

    #[test]
    fn repetition_min_fuses_left_not_right() {
        // x(ab)+y: "xab" is required but "aby" is not (the repetition may
        // repeat before y).
        match build("x(ab)+y", false, Case::Sensitive) {
            Plan::Groups(g) => {
                assert!(g[0].contains(&pack(b"xab")));
                assert!(!g[0].contains(&pack(b"aby")));
            }
            Plan::All => panic!("expected groups"),
        }
    }

    /// Under Unicode case folding a literal's ASCII trigrams still narrow
    /// (an ASCII character other than k and s folds only to its ASCII
    /// case partner); trigrams with a non-ASCII byte are dropped.
    #[test]
    fn unicode_case_insensitive_keeps_only_ascii_trigrams() {
        for fixed in [false, true] {
            match build("caf\u{e9}", fixed, Case::Unicode) {
                Plan::Groups(g) => assert_eq!(g, vec![vec![pack(b"caf")]]),
                Plan::All => panic!("the ASCII trigram should still narrow"),
            }
        }
        assert!(matches!(
            build("\u{e9}t\u{e9}", false, Case::Unicode),
            Plan::All
        ));
        match build("caf\u{e9}", false, Case::Sensitive) {
            Plan::Groups(_) => {}
            Plan::All => panic!("case-sensitive non-ASCII should still narrow"),
        }
    }

    /// `k` and `s` also match KELVIN SIGN and LONG S under Unicode case
    /// folding, which no ASCII case variant covers, so their trigrams
    /// never narrow a case-insensitive Unicode search. ASCII-only folding
    /// (--no-unicode) and case-sensitive searches keep them.
    #[test]
    fn unicode_case_insensitive_drops_k_and_s_trigrams() {
        match build("kfree", false, Case::Unicode) {
            Plan::Groups(g) => assert_eq!(g, vec![vec![pack(b"fre"), pack(b"ree")]]),
            Plan::All => panic!("expected groups"),
        }
        match build("KFREE", true, Case::Unicode) {
            Plan::Groups(g) => assert_eq!(g, vec![vec![pack(b"FRE"), pack(b"REE")]]),
            Plan::All => panic!("expected groups"),
        }
        match build("mismatch", false, Case::Unicode) {
            Plan::Groups(g) => assert_eq!(g, vec![vec![pack(b"atc"), pack(b"mat"), pack(b"tch")]]),
            Plan::All => panic!("expected groups"),
        }
        // Every trigram holds a k or an s: nothing is left to narrow on.
        assert!(matches!(build("kiss", false, Case::Unicode), Plan::All));
        for case in [Case::Ascii, Case::Sensitive] {
            match build("kfree", false, case) {
                Plan::Groups(g) => assert!(g[0].contains(&pack(b"kfr"))),
                Plan::All => panic!("expected groups"),
            }
        }
    }

    /// `Case::Unicode` matches the regex engine in use: under its simple
    /// case folding the only non-ASCII characters equivalent to an ASCII
    /// letter are KELVIN SIGN (k) and LONG S (s).
    #[test]
    fn only_k_and_s_have_non_ascii_fold_partners() {
        use regex_syntax::hir::{Class, HirKind};
        for c in (b'a'..=b'z').chain(b'A'..=b'Z') {
            let hir = regex_syntax::ParserBuilder::new()
                .case_insensitive(true)
                .build()
                .parse(&(c as char).to_string())
                .unwrap();
            let HirKind::Class(Class::Unicode(cls)) = hir.kind() else {
                panic!("{}: not a class: {hir:?}", c as char);
            };
            let non_ascii: Vec<char> = cls
                .iter()
                .flat_map(|r| r.start()..=r.end())
                .filter(|ch| !ch.is_ascii())
                .collect();
            let expect: &[char] = match c.to_ascii_lowercase() {
                b'k' => &['\u{212A}'],
                b's' => &['\u{17F}'],
                _ => &[],
            };
            assert_eq!(non_ascii, expect, "{}", c as char);
            assert_eq!(Case::Unicode.covers(&[c, b'x', b'y']), expect.is_empty());
        }
    }
}
