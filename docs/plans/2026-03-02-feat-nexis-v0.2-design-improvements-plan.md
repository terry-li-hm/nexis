---
title: "feat: nexis v0.2 — design improvements from consilium review"
type: feat
status: active
date: 2026-03-02
---

# feat: nexis v0.2 — design improvements from consilium review

Seven targeted changes to `src/main.rs` based on 6-model consilium design review. No new dependencies. Single-file rewrite + version bump.

## Acceptance Criteria

- [ ] `--backlinks` flag renamed to `--asymmetry`; removed from default report
- [ ] Default output: summary counts only (orphans, broken links, embeds)
- [ ] `--details` flag: shows full item lists for all sections
- [ ] Filter flags (`--orphans`, `--broken`, `--asymmetry`): show full list for that section only
- [ ] `.obsidian`, `.git`, `.trash` path components always excluded from traversal
- [ ] `--exclude <name>` flag (repeatable): skip additional directory names
- [ ] All files indexed in `known_assets` — `[[photo.png]]` not reported as broken
- [ ] `![[embed]]` parsed separately from `[[link]]`; embeds count toward connectivity
- [ ] Exit codes: 0 = clean, 1 = issues found, 2 = fatal error
- [ ] `Cargo.toml` version bumped to `0.2.0`
- [ ] All existing tests pass; new tests for embed detection and exclusion logic

## Touch Points

All changes are in `src/main.rs`. Line numbers from v0.1 (529 lines):

| Change | Location |
|--------|----------|
| Rename `backlinks` → `asymmetry` in `Cli` | line 20 |
| Add `details: bool`, `exclude: Vec<String>` to `Cli` | after line 24 |
| Update `should_show_all` for new flags | line 330 |
| Add `collect_all_files()` for `known_assets` | new fn after `collect_markdown_files` (line 141) |
| Add exclusion logic to both collect fns | lines 141–160 |
| Update `wikilink_regex` to `(!?)\[\[...\]\]` | line 187 |
| `extract_wikilinks` → returns `(Vec<String>, Vec<String>)` (links, embeds) | line 246 |
| `Analysis` struct: add `embeds: Vec<(PathBuf, String)>`, `embed_count: usize` | line 58 |
| `analyze_graph`: use `known_assets` for broken check; track embeds | line 256 |
| `print_human_report`: summary block first; `--details` / filter flag guard | line 349 |
| `Report` struct: add `embeds_count: usize` | line 37 |
| `main`: exit code logic after analyze | line 94 |
| `Cargo.toml`: version `0.2.0` | Cargo.toml line 3 |

## Context

### Regex change (no lookahead — Rust regex crate constraint)

Change from:
```
\[\[([^\]|][^\]]*?)(?:\|([^\]]*))?\]\]
```
To:
```
(!?)\[\[([^\]|][^\]]*?)(?:\|([^\]]*))?\]\]
```
Group 1 = `!` or `""` (embed marker). Group 2 = target. Group 3 = alias. No lookahead needed.

### known_assets logic

```rust
fn collect_all_files(vault_root: &Path, excludes: &[String]) -> HashSet<UniCase<String>> {
    // WalkDir all files; same exclusion logic as collect_markdown_files
    // For each file insert: UniCase(filename_with_ext) AND UniCase(stem_only)
    // So [[photo.png]] and [[photo]] both resolve
}
```

Broken link check: `!index.contains(target) && !known_assets.contains(target)`

### Exclusion logic

Skip any `WalkDir` entry where any path component (as `OsStr`) matches:
- `.obsidian`, `.git`, `.trash` (hardcoded)
- Any string in `cli.exclude`

### Exit codes

```rust
// After analyze_graph:
if analysis.orphans.is_empty() && analysis.broken_links.is_empty() {
    ExitCode::SUCCESS      // 0
} else {
    ExitCode::from(1)      // 1 — issues found
}
// Fatal errors (validate_vault_path failure, JSON serialize failure): ExitCode::from(2)
```

### Summary output format (default, no flags)

```
Vault: /path/to/vault  (11,738 notes)
  Orphans        8,113   (--orphans to list)
  Broken links   3,037   (--broken to list)
  Embeds           423
```
`--details` or a filter flag shows the full item list for relevant sections.

## Delegation

**Tool:** Codex (single-file Rust rewrite, complex logic)
**Constraint:** Codex sandbox has no network — write correct source, DO NOT run `cargo build`. Build locally after.
**Verify after delegation:**
```bash
cd ~/code/nexis
cargo build --release 2>&1 | tail -20
cargo nextest run
nexis ~/notes --help
nexis ~/notes
```

## Sources

- Consilium design review: output lost (session ended before save — see consilium skill update)
- Existing implementation: `/Users/terry/code/nexis/src/main.rs`
- Rust regex no-lookahead constraint: `~/docs/solutions/memory-overflow.md`
- Codex sandbox no-network constraint: `~/docs/solutions/memory-overflow.md`
