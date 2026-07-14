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

pub fn build(pattern: &str, fixed: bool) -> Plan {
    if fixed {
        return match trigrams_of(pattern.as_bytes()) {
            Some(t) => Plan::Groups(vec![t]),
            None => Plan::All,
        };
    }
    let hir = match regex_syntax::parse(pattern) {
        Ok(h) => h,
        Err(_) => return Plan::All, // engine will surface the real error
    };
    let seq = regex_syntax::hir::literal::Extractor::new().extract(&hir);
    let lits = match seq.literals() {
        Some(l) if !l.is_empty() => l,
        _ => return Plan::All,
    };
    let mut groups = Vec::with_capacity(lits.len());
    for lit in lits {
        // Inexact prefixes are still REQUIRED substrings, so they narrow
        // soundly as long as they carry at least one trigram.
        match trigrams_of(lit.as_bytes()) {
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
        match build("hello", true) {
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
        match build("fn main", false) {
            Plan::Groups(g) => assert!(g[0].contains(&pack(b"mai"))),
            Plan::All => panic!("expected groups"),
        }
    }

    #[test]
    fn alternation_yields_or_groups() {
        match build("foobar|bazqux", false) {
            Plan::Groups(g) => {
                assert_eq!(g.len(), 2);
                assert!(g[0].contains(&pack(b"foo")) || g[1].contains(&pack(b"foo")));
            }
            Plan::All => panic!("expected groups"),
        }
    }

    #[test]
    fn unnarrowing_patterns_are_all() {
        assert!(matches!(build(".*", false), Plan::All));
        assert!(matches!(build("a", false), Plan::All));
        assert!(matches!(build("ab", true), Plan::All));
        assert!(matches!(build("[", false), Plan::All)); // unparseable: fall back
    }

    #[test]
    fn prefix_of_wildcard_pattern_still_narrows() {
        // "needle.*" requires the literal "needle" prefix
        match build("needle.*", false) {
            Plan::Groups(g) => assert!(g[0].contains(&pack(b"nee"))),
            Plan::All => panic!("expected groups"),
        }
    }
}
