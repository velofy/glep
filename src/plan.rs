use crate::trigram;

#[derive(Debug, PartialEq)]
pub enum Plan {
    /// OR of AND-groups: a file is a candidate if, for some group,
    /// it contains every trigram in that group.
    Groups(Vec<Vec<u32>>),
    /// The index cannot narrow this pattern; scan all indexed files.
    All,
}

/// How the matcher compares letters, which decides how the index looks a
/// required trigram up (`Index::postings_for`) and which trigrams of a
/// literal it can look up at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Case {
    /// Byte for byte: every trigram is looked up as is.
    Sensitive,
    /// ASCII case folding only (--no-unicode): an all-ASCII trigram is
    /// looked up as each of its ASCII case variants.
    Ascii,
    /// Unicode simple case folding, both engines' -i (and -S) in their
    /// default Unicode mode. ASCII letters fold among themselves except
    /// two: `k`/`K` also match KELVIN SIGN (U+212A) and `s`/`S` LONG S
    /// (U+017F), so an all-ASCII trigram is looked up as every trigram a
    /// match can put in a file (`trigram::fold_variants`).
    Unicode,
}

impl Case {
    /// Whether every match of the literal bytes `t` contains one of the
    /// index's lookups for them: always when case-sensitive; otherwise
    /// only for all-ASCII bytes, since a non-ASCII character can fold to
    /// characters of other byte lengths.
    fn covers(self, t: &[u8]) -> bool {
        match self {
            Case::Sensitive => true,
            Case::Ascii | Case::Unicode => t.is_ascii(),
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

/// Size caps that keep planning linear in the pattern however its
/// repetitions nest (`(?:a.){30000}`, `((ab){1000}){1000}`): a counted
/// repetition expands at most `MAX_REPEAT_COPIES` copies, a literal keeps
/// at most `MAX_LIT_BYTES` bytes, a group at most `MAX_GROUP_PIECES`
/// pieces and `MAX_GROUP_TRIGRAMS` trigrams. Each cut drops required
/// literals (or a literal's tail) and closes fusion where it cut, so the
/// plan only narrows less; it never misses a match.
const MAX_REPEAT_COPIES: usize = 64;
const MAX_LIT_BYTES: usize = 1024;
const MAX_GROUP_PIECES: usize = 128;
const MAX_GROUP_TRIGRAMS: usize = 1024;

fn anything() -> Dnf {
    vec![vec![Piece::Boundary]]
}

fn lit(mut s: Vec<u8>) -> Dnf {
    // A literal's prefix is required wherever the literal is; only its
    // right edge stops being adjacent to what follows it.
    let fuse_right = s.len() <= MAX_LIT_BYTES;
    s.truncate(MAX_LIT_BYTES);
    vec![vec![Piece::Lit {
        fuse_left: true,
        fuse_right,
        s,
    }]]
}

/// Append group `b` to group `a` for concatenation, in place; when `a`'s
/// last literal fuses right and `b`'s first fuses left, their strings
/// concatenate (longer literals produce better trigram narrowing). A
/// literal that outgrows `MAX_LIT_BYTES` keeps its prefix and closes its
/// right edge; a group that reaches `MAX_GROUP_PIECES` ends in a
/// `Boundary` and takes nothing more.
fn append_group(a: &mut Vec<Piece>, b: &[Piece]) {
    if a.len() >= MAX_GROUP_PIECES {
        return;
    }
    match (a.last_mut(), b.first()) {
        (
            Some(Piece::Lit {
                fuse_right: fr0 @ true,
                s: s0,
                ..
            }),
            Some(Piece::Lit {
                fuse_left: true,
                s: s1,
                fuse_right,
            }),
        ) => {
            s0.extend_from_slice(s1);
            // Keep the fused literal's right edge closed when `b`'s was.
            *fr0 = *fuse_right;
            if s0.len() > MAX_LIT_BYTES {
                s0.truncate(MAX_LIT_BYTES);
                *fr0 = false;
            }
            a.extend_from_slice(&b[1..]);
        }
        _ => a.extend_from_slice(b),
    }
    if a.len() >= MAX_GROUP_PIECES {
        a.truncate(MAX_GROUP_PIECES - 1);
        a.push(Piece::Boundary);
    }
}

/// Concatenate `d` onto `acc`: every group of `acc` followed by every
/// alternative of `d`. A single alternative extends the groups in place,
/// so a long concatenation never re-clones what it has built.
fn concat_into(acc: &mut Dnf, d: &Dnf) {
    if acc.len().saturating_mul(d.len()) > MAX_VARIANTS {
        // Product would explode past the cap: degrade THIS element to a
        // boundary. The accumulated groups keep their literals: sound,
        // just less fused.
        concat_into(acc, &anything());
        return;
    }
    if let [e] = d.as_slice() {
        for g in acc.iter_mut() {
            append_group(g, e);
        }
        return;
    }
    let mut next = Vec::with_capacity(acc.len() * d.len());
    for g in acc.iter() {
        for e in d {
            let mut g = g.clone();
            append_group(&mut g, e);
            next.push(g);
        }
    }
    *acc = next;
}

fn concat(dnfs: impl IntoIterator<Item = Dnf>) -> Dnf {
    let mut acc: Dnf = vec![Vec::new()];
    for d in dnfs {
        concat_into(&mut acc, &d);
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
            // A repetition desugars to `min` required copies concatenated
            // (at most MAX_REPEAT_COPIES of them: the first copies are
            // required whatever follows), plus a fusion-closing boundary
            // whenever more may follow them (copies left out, or an
            // optional tail: unbounded or max > min): a further copy may
            // interpose bytes, so `r+`'s right edge must not fuse into
            // its neighbor.
            if r.min == 0 {
                return anything();
            }
            let sub = req(&r.sub);
            let copies = (r.min as usize).min(MAX_REPEAT_COPIES);
            let mut d: Dnf = vec![Vec::new()];
            for _ in 0..copies {
                concat_into(&mut d, &sub);
            }
            if copies < r.min as usize || r.max.is_none_or(|max| max > r.min) {
                concat_into(&mut d, &anything());
            }
            d
        }
        HirKind::Capture(c) => req(&c.sub),
        HirKind::Concat(subs) => concat(subs.iter().map(req)),
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
            if tris.len() >= MAX_GROUP_TRIGRAMS {
                // Any subset of the required trigrams still narrows soundly.
                tris.truncate(MAX_GROUP_TRIGRAMS);
                break;
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

    /// Counted repetitions, nested or not, and long concatenations plan in
    /// time linear in the pattern: a repetition expands at most
    /// MAX_REPEAT_COPIES copies and groups and literals stay bounded.
    /// `(?:a.){30000}` used to spend 10 s in the planner (quadratic in the
    /// count) and deeper nesting grew without bound.
    #[test]
    fn huge_and_nested_repetitions_plan_quickly() {
        let long_concat = "a.".repeat(30_000);
        let long_lit = "xyz".repeat(5_000);
        for pat in [
            "(?:a.){30000}",
            "(?:ab?){30000}",
            "((?:ab){1000}){1000}",
            "(((?:abc){64}){64}){64}",
            "((((?:abc){100}){100}){100}){100}",
            "(?:(?:a|b|c)(?:d|e)){5000}",
            long_concat.as_str(),
            long_lit.as_str(),
        ] {
            let t = std::time::Instant::now();
            let plan = build(pat, false, Case::Sensitive);
            assert!(
                t.elapsed() < std::time::Duration::from_secs(2),
                "{}: planning took {:?}",
                &pat[..pat.len().min(40)],
                t.elapsed()
            );
            if let Plan::Groups(g) = plan {
                assert!(g.len() <= MAX_VARIANTS);
                assert!(g.iter().all(|t| t.len() <= MAX_GROUP_TRIGRAMS));
            }
        }
    }

    /// The caps only weaken what a group requires: the first copies of a
    /// long repetition still narrow, and the literal after it no longer
    /// fuses with the last expanded copy (more copies come between).
    #[test]
    fn capped_repetitions_still_narrow_soundly() {
        match build("x(?:abc){100}y", false, Case::Sensitive) {
            Plan::Groups(g) => {
                assert_eq!(g.len(), 1);
                assert!(g[0].contains(&pack(b"xab")));
                assert!(g[0].contains(&pack(b"cab")));
                assert!(!g[0].contains(&pack(b"bcy")));
            }
            Plan::All => panic!("expected groups"),
        }
        // Below the cap a counted repetition fuses completely.
        match build("x(?:abc){2}y", false, Case::Sensitive) {
            Plan::Groups(g) => assert!(g[0].contains(&pack(b"bcy"))),
            Plan::All => panic!("expected groups"),
        }
        // A literal longer than MAX_LIT_BYTES narrows on its prefix.
        let long: String = (0..2000).map(|i| (b'a' + (i % 26) as u8) as char).collect();
        match build(&long, true, Case::Sensitive) {
            Plan::Groups(g) => {
                assert!(g[0].contains(&pack(b"abc")));
                assert!(g[0].len() <= MAX_GROUP_TRIGRAMS);
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

    /// `k` and `s` trigrams still narrow a Unicode case-insensitive
    /// search: the index looks them up with their KELVIN SIGN and LONG S
    /// spellings too (`trigram::fold_variants`).
    #[test]
    fn k_and_s_trigrams_still_narrow() {
        for case in [Case::Unicode, Case::Ascii, Case::Sensitive] {
            match build("kfree", false, case) {
                Plan::Groups(g) => {
                    assert_eq!(g, vec![vec![pack(b"fre"), pack(b"kfr"), pack(b"ree")]])
                }
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
            // Each partner's UTF-8 has its own trigram windows.
            let t = crate::trigram::pack(&[c, b'x', b'y']);
            let lookups = crate::trigram::fold_variants(t);
            for ch in expect {
                let text = format!("{ch}xy");
                let tris = crate::trigram::extract(text.as_bytes());
                assert!(lookups.iter().any(|v| tris.contains(v)), "{text}");
            }
        }
    }
}
