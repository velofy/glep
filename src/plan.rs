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

pub fn build(pattern: &str, fixed: bool, case_insensitive: bool) -> Plan {
    let literals: Vec<Vec<u8>> = if fixed {
        vec![pattern.as_bytes().to_vec()]
    } else {
        let hir = match regex_syntax::parse(pattern) {
            Ok(h) => h,
            Err(_) => return Plan::All, // engine will surface the real error
        };
        let seq = regex_syntax::hir::literal::Extractor::new().extract(&hir);
        match seq.literals() {
            Some(l) if !l.is_empty() => l.iter().map(|lit| lit.as_bytes().to_vec()).collect(),
            _ => return Plan::All,
        }
    };

    // ASCII case variants cannot cover Unicode case folding; with -i and
    // any non-ASCII literal byte, only a full scan is sound.
    if case_insensitive && literals.iter().any(|l| l.iter().any(|&b| b >= 0x80)) {
        return Plan::All;
    }

    let mut groups = Vec::with_capacity(literals.len());
    for lit in &literals {
        // Inexact prefixes are still REQUIRED substrings, so they narrow
        // soundly as long as they carry at least one trigram.
        match trigrams_of(lit) {
            Some(t) => groups.push(t),
            None => return Plan::All,
        }
    }
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
    }

    #[test]
    fn prefix_of_wildcard_pattern_still_narrows() {
        // "needle.*" requires the literal "needle" prefix
        match build("needle.*", false, false) {
            Plan::Groups(g) => assert!(g[0].contains(&pack(b"nee"))),
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
