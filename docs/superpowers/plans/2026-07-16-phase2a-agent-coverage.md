# glep Phase 2a: Agent Coverage Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development. Steps use checkbox syntax.

**Goal:** Close the agent-facing feature gaps (count mode, separate -A/-B context, multiline -U), polish debuggability, widen the wheel matrix, ship v0.2.0.

**Architecture:** All features ride the existing pipeline (plan -> candidates -> search) and the existing chunked ordered-print model. Every new mode gets a parity-battery case; parity vs real rg is the acceptance test for each feature, not just unit assertions.

**Tech Stack:** existing crates only.

## Global Constraints

- No em-dashes or typographic en-dashes anywhere.
- Every feature task ADDS parity cases to tests/parity.rs and must leave the whole suite green. Never weaken an assertion.
- Parity target invocation stays `rg -n --no-heading --color=never --sort path --no-require-git` plus the feature flag under test.
- The hook (claude/hooks/glep_redirect.py) must keep its never-crash and pass-through-when-inapplicable contract; test_hook.sh must stay green and grow with each mapping change.
- Version stays 0.1.2 until the release task sets 0.2.0.

---

### Task 1: count mode (-c)

**Files:** Modify: `src/search.rs`, `src/cli.rs`, `claude/hooks/glep_redirect.py`, `claude/hooks/test_hook.sh`, `claude/SKILL.md`, `tests/cli.rs`, `tests/parity.rs`

**Semantics:** rg -c: for each file with at least one matching line, print `path:count` (count of matching lines), path-sorted. Exit 0 if any file matched, else 1.

- [ ] **Step 1: search.rs.** Add `pub count: bool` to SearchOpts. Add the sink:

```rust
struct CountSink(u64);

impl grep_searcher::Sink for CountSink {
    type Error = std::io::Error;
    fn matched(
        &mut self,
        _: &grep_searcher::Searcher,
        _: &grep_searcher::SinkMatch<'_>,
    ) -> Result<bool, std::io::Error> {
        self.0 += 1;
        Ok(true)
    }
}
```

In `search_one`, when `opts.count` (checked BEFORE the files_with_matches branch), run the searcher with `CountSink(0)`; if the count is > 0, return `(format!("{}:{}\n", rel.display(), sink.0).into_bytes(), true)`, else `(Vec::new(), false)`. The existing chunked ordered-print pipeline then produces path-sorted `path:count` lines with no further changes. Count output never gets context separators (`separate` already requires context > 0; ensure count mode leaves it false by not touching context).

- [ ] **Step 2: cli.rs.** Add `#[arg(short = 'c', long = "count", conflicts_with_all = ["files_with_matches", "json"])] pub count: bool`, plumb into SearchOpts.

- [ ] **Step 3: hook.** In claude/hooks/glep_redirect.py, REMOVE the count-mode pass-through: `output_mode == "count"` now maps to `cmd.append("-c")` instead of `allow()`. Update the corresponding test_hook.sh case 4: it must now expect a DENY containing `glep -c -e 'x'` instead of pass-through. Add a `| Grep count mode | \`glep -c -e 'X'\` |` row to SKILL.md's table.

- [ ] **Step 4: tests.** tests/cli.rs add:

```rust
#[test]
fn count_mode_prints_path_counts() {
    let dir = corpus();
    glep(dir.path())
        .args(["-c", "hello"])
        .assert()
        .success()
        .stdout("notes.txt:1\nsrc/lib.rs:1\n");
    glep(dir.path()).args(["-c", "zz_absent"]).assert().code(1);
}
```

tests/parity.rs: add `&["-c", "hello"]` and `&["-c", "-i", "HELLO"]` to the battery.

- [ ] **Step 5: verify.** `cargo test --all` green (parity includes new cases, run against real rg); `cargo build && PATH="$PWD/target/debug:$PATH" claude/hooks/test_hook.sh` prints OK.

- [ ] **Step 6: Commit.** `feat: count mode with rg parity, hook maps count queries`

---

### Task 2: separate -A/-B context flags

**Files:** Modify: `src/search.rs`, `src/cli.rs`, `claude/hooks/glep_redirect.py`, `claude/hooks/test_hook.sh`, `tests/parity.rs`

**Semantics:** rg: -A n after-context, -B n before-context, -C n sets both. When both -C and -A/-B appear, the specific flag wins for its direction.

- [ ] **Step 1: search.rs.** Replace `pub context: usize` in SearchOpts with `pub before: usize` and `pub after: usize`. Searcher: `.before_context(opts.before).after_context(opts.after)`. Separator condition becomes `(opts.before > 0 || opts.after > 0)`. Update the SearchOpts construction in src/search.rs tests (context: 0 becomes before: 0, after: 0) and any helper.

- [ ] **Step 2: cli.rs.** Replace `-C` arg with three Options:

```rust
    #[arg(short = 'C', long)]
    pub context: Option<usize>,
    #[arg(short = 'A', long = "after-context")]
    pub after_context: Option<usize>,
    #[arg(short = 'B', long = "before-context")]
    pub before_context: Option<usize>,
```

Effective values: `let before = args.before_context.or(args.context).unwrap_or(0); let after = args.after_context.or(args.context).unwrap_or(0);` plumbed into SearchOpts.

- [ ] **Step 3: hook.** Replace the -A/-B/-C folding loop with direct mapping: `-C` from ti["-C"], `-A` from ti["-A"], `-B` from ti["-B"], each appended with its own flag when present and truthy. Update/extend the test_hook.sh Grep case to cover a payload with `"-A": 2` expecting `-A 2` (not `-C 2`) in the suggested command.

- [ ] **Step 4: parity.** Add `&["-A", "1", "hello"]` and `&["-B", "1", "answer"]` to the battery.

- [ ] **Step 5: verify.** Full suite + hook tests green.

- [ ] **Step 6: Commit.** `feat: separate -A/-B context flags with rg parity`

---

### Task 3: multiline (-U)

**Files:** Modify: `src/search.rs`, `src/cli.rs`, `tests/parity.rs`

**Semantics:** rg -U: patterns may span lines (searcher operates multi-line; \n may appear in the pattern). The trigram plan is already sound for this: file content is indexed as raw bytes including newlines, so a literal containing \n narrows correctly.

- [ ] **Step 1: search.rs.** Add `pub multiline: bool` to SearchOpts. SearcherBuilder gains `.multi_line(opts.multiline)`. build_matcher: on RegexMatcherBuilder, when opts.multiline call `.multi_line(true)` if the builder exposes it, and `.line_terminator(None)`-equivalent adjustments only if compilation of \n-containing patterns requires it; verify against grep-regex 0.1 docs and adapt mechanically (rg's own -U maps to searcher multi_line plus a matcher that permits literal \n; if a pattern with an escaped newline fails to build without a builder tweak, that tweak is in scope).

- [ ] **Step 2: cli.rs.** `#[arg(short = 'U', long)] pub multiline: bool`, plumb through.

- [ ] **Step 3: parity.** Add `&["-U", "goodbye\\nfoo"]` to the battery (README.md in the corpus contains "hello and goodbye\nfoo bar baz", so the match spans the line break; rg receives the same argv). Note the Rust string in the array must produce the two-character sequence backslash-n as the ARGUMENT (regex escape), matching what an agent would pass.

- [ ] **Step 4: verify.** Full suite green including the new parity case.

- [ ] **Step 5: Commit.** `feat: multiline search with rg parity`

---

### Task 4: debuggability polish

**Files:** Modify: `src/search.rs`, `src/cli.rs`, `tests/cli.rs`

- [ ] **Step 1:** search.rs run(): the rayon closure's `Err(e)` arm gains a stderr note before mapping to no-match: `eprintln!("glep: {}: {}", rel.display(), e);` (stdout parity is unaffected; the parity harness compares stdout only).
- [ ] **Step 2:** cli.rs status subcommand: call `idx.update(args.max_filesize, 0)?` after open_or_build and before printing, unless `idx.read_only` (read-only status reports persisted state). Counts now reflect the live tree.
- [ ] **Step 3:** tests/cli.rs add:

```rust
#[test]
fn status_reflects_live_tree() {
    let dir = corpus();
    glep(dir.path()).arg("index").assert().success();
    std::fs::write(dir.path().join("third.txt"), "x").unwrap();
    glep(dir.path())
        .arg("status")
        .assert()
        .success()
        .stdout(predicates::str::contains("files: 3"));
}
```

- [ ] **Step 4:** Full suite green. Commit: `fix: search IO errors warn on stderr; status self-heals`

---

### Task 5: wheel matrix expansion

**Files:** Modify: `.github/workflows/release.yml`

- [ ] **Step 1:** Extend the wheels job matrix to:

```yaml
    strategy:
      matrix:
        include:
          - os: ubuntu-latest
          - os: ubuntu-24.04-arm
          - os: macos-14
          - os: macos-13
          - os: ubuntu-latest
            target: x86_64-unknown-linux-musl
            manylinux: musllinux_1_2
```

The maturin-action step gains conditional inputs: `target: ${{ matrix.target || '' }}` and `manylinux: ${{ matrix.manylinux || 'auto' }}`. Artifact names must stay unique: `wheels-${{ matrix.os }}-${{ matrix.target || 'default' }}`.

- [ ] **Step 2:** Validate YAML: `python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/release.yml'))"` (or actionlint if present). CI-side correctness is proven at the release tag; if a leg fails there, fix forward.
- [ ] **Step 3:** Commit: `feat: wheels for mac x86_64, linux aarch64, musl`

---

### Task 6: release v0.2.0

**Files:** Modify: `Cargo.toml`, `README.md`, `site/index.html`, `claude/SKILL.md` (if any mapping rows are still missing)

- [ ] **Step 1:** Bump version to 0.2.0; `cargo build` refreshes the lock.
- [ ] **Step 2:** README Interface section gains -c, -A/-B, -U lines; site's skill/interface mentions stay accurate. Update the README "When to use it" right column: remove "count mode" and "multiline" from the missing-features list (replacements, PCRE2, compressed files remain).
- [ ] **Step 3:** Full suite + hook tests green one final time.
- [ ] **Step 4:** Commit `release: v0.2.0`, merge branch to main via PR, tag v0.2.0, push tag (PyPI workflow), `cargo publish` for crates.io, verify both registries report 0.2.0.

## Self-review notes

- Task 1 before Task 2 because both touch SearchOpts; Task 2's field rename (context -> before/after) would conflict if parallel.
- The -U parity case is the riskiest (grep-regex multiline behavior vs rg's); the task explicitly allows mechanical matcher-builder adaptation but never weakening the parity assertion. If true parity is unreachable with pinned crates, the task must BLOCK and report, not ship approximate output.
- Wheel matrix legs cannot be tested locally; the release tag is the test, and a failed leg is fixed forward without unpublishing good wheels (--skip-existing makes retries safe).
