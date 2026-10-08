use crate::trigram;

#[derive(Debug, PartialEq)]
pub enum Plan {
    /// OR of AND-groups: a file is a candidate if, for some group,
    /// it contains every trigram in that group.
    Groups(Vec<Vec<u32>>),
    /// The index cannot narrow this pattern; scan all indexed files.
    All,
}

fn trigrams_of(lit: &[u8]) -> Option<Vec<u32>> {
    if lit.len() < 3 {
        return None;
    }
    let mut tris: Vec<u32> = lit.windows(3).map(trigram::pack).collect();
    tris.sort_unstable();
    tris.dedup();
    Some(tris)
}

/// A piece of a required-literal group. `Lit` carries a substring that
/// must appear in any match plus edge flags saying whether a literal
/// neighbor may fuse into it; `Boundary` contributes nothing but blocks
/// fusion (e.g. `.*`, `x?`, lookarounds, unexpandable classes) so the
/// literals on either side are known-required but not adjacent.
#[derive(Clone, Debug)]
enum Piece {
    Boundary,
    Lit { fuse_left: bool, fuse_right: bool, s: Vec<u8> },
}

/// DNF: OR of groups; each group is a sequence of pieces (all literals in
/// a group are AND-required for a match that takes that alternative).
type Dnf = Vec<Vec<Piece>>;

/// Cap on expanded variants (class ranges, alternation products,
/// repetition multiplicities). Past it the element degrades to
/// `Boundary`, which is always sound — it just narrows less.
const MAX_VARIANTS: usize = 64;

fn anything() -> Dnf {
    vec![vec![Piece::Boundary]]
}

fn lit(s: Vec<u8>) -> Dnf {
    vec![vec![Piece::Lit { fuse_left: true, fuse_right: true, s }]]
}

/// Merge group `a` and `b` for concatenation; when `a`'s last literal
/// fuses right and `b`'s first fuses left, their strings concatenate
/// (longer literals produce better trigram narrowing).
fn concat_groups(a: &[Piece], b: &[Piece]) -> Vec<Piece> {
    let mut out = a.to_vec();
    match (out.last_mut(), b.first()) {
        (
            Some(Piece::Lit { fuse_right: true, s: ref mut s0, .. }),
            Some(Piece::Lit { fuse_left: true, s: s1, fuse_right }),
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
            // a boundary. The accumulated groups keep their literals —
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
/// a substring of it. Anything unrecognized is `Boundary` — conservative.
fn req(hir: &regex_syntax::hir::Hir) -> Dnf {
    use regex_syntax::hir::HirKind;
    match hir.kind() {
        // Empty contributes nothing but is NOT a boundary — literals on
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
            let copies: Vec<Dnf> = std::iter::repeat_n(sub, r.min as usize)
                .collect();
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
/// vacuous (no literal ≥3 bytes), which must fail the whole plan to All
/// because that alternative constrains nothing.
fn group_trigrams(group: &[Piece]) -> Option<Vec<u32>> {
    let mut tris = Vec::new();
    for p in group {
        if let Piece::Lit { s, .. } = p {
            if let Some(t) = trigrams_of(s) {
                tris.extend(t);
            }
        }
    }
    if tris.is_empty() {
        return None;
    }
    tris.sort_unstable();
    tris.dedup();
    Some(tris)
}

pub fn build(pattern: &str, fixed: bool, case_insensitive: bool) -> Plan {
    let dnf: Dnf = if fixed {
        lit(pattern.as_bytes().to_vec())
    } else {
        let hir = match regex_syntax::parse(pattern) {
            Ok(h) => h,
            Err(_) => return Plan::All, // engine will surface the real error
        };
        req(&hir)
    };

    // Literal bytes seen for the non-ASCII guard.
    let literals: Vec<&[u8]> = dnf
        .iter()
        .flat_map(|g| g.iter())
        .filter_map(|p| match p {
            Piece::Lit { s, .. } => Some(&s[..]),
            _ => None,
        })
        .collect();

    // ASCII case variants cannot cover Unicode case folding; with -i and
    // any non-ASCII literal byte, only a full scan is sound.
    if case_insensitive
        && literals.iter().any(|l| l.iter().any(|&b| b >= 0x80))
    {
        return Plan::All;
    }

    let mut groups = Vec::with_capacity(dnf.len());
    for g in &dnf {
        match group_trigrams(g) {
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
        match build("hello", true, false) {
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
        match build("fn main", false, false) {
            Plan::Groups(g) => assert!(g[0].contains(&pack(b"mai"))),
            Plan::All => panic!("expected groups"),
        }
    }

    #[test]
    fn alternation_yields_or_groups() {
        match build("foobar|bazqux", false, false) {
            Plan::Groups(g) => {
                assert_eq!(g.len(), 2);
                assert!(g[0].contains(&pack(b"foo")) || g[1].contains(&pack(b"foo")));
            }
            Plan::All => panic!("expected groups"),
        }
    }

    #[test]
    fn unnarrowing_patterns_are_all() {
        assert!(matches!(build(".*", false, false), Plan::All));
        assert!(matches!(build("a", false, false), Plan::All));
        assert!(matches!(build("ab", true, false), Plan::All));
        assert!(matches!(build("[", false, false), Plan::All)); // unparseable: fall back
        // `x?yz`: yz is required but too short for trigrams — vacuous.
        assert!(matches!(build("x?yz", false, false), Plan::All));
    }

    #[test]
    fn anything_arm_still_contributes_required_tail() {
        // `(foo|.*)bar`: every match ends in bar regardless of the arm —
        // narrowing on `bar` is sound.
        match build("(foo|.*)bar", false, false) {
            Plan::Groups(g) => {
                assert!(g.iter().any(|grp| grp.contains(&pack(b"bar"))));
            }
            Plan::All => panic!("expected groups"),
        }
    }

    #[test]
    fn prefix_of_wildcard_pattern_still_narrows() {
        match build("needle.*", false, false) {
            Plan::Groups(g) => assert!(g[0].contains(&pack(b"nee"))),
            Plan::All => panic!("expected groups"),
        }
    }

    #[test]
    fn literal_after_unexpandable_part_still_narrows() {
        // `-foo` is required even though the digit run is too big to expand.
        match build("[0-9][0-9][0-9]-foo", false, false) {
            Plan::Groups(g) => {
                assert_eq!(g.len(), 1);
                assert!(g[0].contains(&pack(b"-fo")));
            }
            Plan::All => panic!("required literal should still narrow"),
        }
    }

    #[test]
    fn literals_split_across_boundary_both_required() {
        // `abc\w+def`: every match contains abc AND def — one group, both lit
        // sets (stronger than keeping only the prefix).
        match build("abc\\w+def", false, false) {
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
        match build("a[bc]d", false, false) {
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
        match build("x(ab)+y", false, false) {
            Plan::Groups(g) => {
                assert!(g[0].contains(&pack(b"xab")));
                assert!(!g[0].contains(&pack(b"aby")));
            }
            Plan::All => panic!("expected groups"),
        }
    }

    #[test]
    fn unicode_case_insensitive_falls_back_to_all() {
        assert!(matches!(build("caf\u{e9}", false, true), Plan::All));
        assert!(matches!(build("caf\u{e9}", true, true), Plan::All));
        match build("caf\u{e9}", false, false) {
            Plan::Groups(_) => {}
            Plan::All => panic!("case-sensitive non-ASCII should still narrow"),
        }
    }
}


