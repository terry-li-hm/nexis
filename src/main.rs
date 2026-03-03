use clap::{Parser, ValueEnum};
use rayon::prelude::*;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime};
use trama::{
    build_file_index, collect_all_files, collect_markdown_files, extract_wikilinks,
    normalize_target, pluralize, relativize, strip_code_regions,
    strip_html_comments, validate_vault_path, wikilink_regex,
};
use unicase::UniCase;

#[derive(Parser)]
#[command(name = "nexis", version, about = "Obsidian vault link health analyser")]
struct Cli {
    /// Vault root directory
    path: PathBuf,
    #[arg(
        long,
        help = "show asymmetric links (A links to B but B does not link back)"
    )]
    asymmetry: bool,
    #[arg(long)]
    orphans: bool, // show orphans only
    #[arg(long)]
    broken: bool, // show broken links only
    #[arg(long)]
    details: bool, // show full item lists (default is summary counts only)
    #[arg(long)]
    exclude: Vec<String>, // extra dirs to skip
    #[arg(long, help = "only show orphans modified within N days")]
    orphan_days: Option<u64>,
    #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
    format: OutputFormat,
    #[arg(long, help = "Alias for --format json")]
    json: bool,
    #[arg(long, help = "convert broken wikilinks to plain text in place")]
    unlink: bool,
    #[arg(long, help = "with --unlink: preview changes without writing")]
    dry_run: bool,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum OutputFormat {
    Human,
    Json,
}

#[derive(Serialize)]
struct Report {
    vault: String,
    total_files: usize,
    total_assets: usize,
    embed_count: usize,
    missing_backlinks: Vec<MissingBacklink>,
    orphans: Vec<String>,
    broken_links: Vec<BrokenLink>,
}

#[derive(Serialize)]
struct MissingBacklink {
    source: String,
    target: String,
}

#[derive(Serialize)]
struct BrokenLink {
    source: String,
    target: String,
}

struct Analysis {
    missing_backlinks: Vec<(PathBuf, PathBuf)>,
    orphans: Vec<PathBuf>,
    broken_links: Vec<(PathBuf, String)>,
    embed_count: usize,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let format = if cli.json {
        OutputFormat::Json
    } else {
        cli.format
    };

    let vault_root = match validate_vault_path(&cli.path) {
        Ok(path) => path,
        Err(err) => {
            eprintln!("Fatal error: {err}");
            return ExitCode::from(2);
        }
    };

    let files = collect_markdown_files(&vault_root, cli.exclude.as_slice());
    let index = build_file_index(&files);
    let known_assets = collect_all_files(&vault_root, cli.exclude.as_slice());

    let results: Vec<(PathBuf, Vec<String>, Vec<String>)> = files
        .par_iter()
        .filter_map(|path| {
            let content = fs::read_to_string(path).ok()?;
            let (links, embeds) = extract_wikilinks(&content);
            Some((path.clone(), links, embeds))
        })
        .collect();

    let analysis = analyze_graph(&files, &results, &index, &known_assets);
    let has_findings = !(analysis.orphans.is_empty() && analysis.broken_links.is_empty());

    if cli.unlink {
        let (unlinked, changed) = unlink_broken_links(&analysis.broken_links, cli.dry_run);
        if cli.dry_run {
            eprintln!("Dry run: would unlink {unlinked} broken links across {changed} files");
        } else {
            eprintln!("Unlinked {unlinked} broken links across {changed} files");
        }
    }

    match format {
        OutputFormat::Human => {
            print_human_report(&vault_root, files.len(), known_assets.len(), &analysis, &cli);
            if has_findings {
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            }
        }
        OutputFormat::Json => {
            let report = build_json_report(&vault_root, files.len(), known_assets.len(), &analysis);
            match serde_json::to_string_pretty(&report) {
                Ok(json) => {
                    println!("{json}");
                    if has_findings {
                        ExitCode::from(1)
                    } else {
                        ExitCode::SUCCESS
                    }
                }
                Err(err) => {
                    eprintln!("Fatal error: failed to serialize JSON output: {err}");
                    ExitCode::from(2)
                }
            }
        }
    }
}


/// Convert broken wikilinks to plain text in source files.
/// Returns (links_unlinked, files_changed).
fn unlink_broken_links(broken_links: &[(PathBuf, String)], dry_run: bool) -> (usize, usize) {
    // Group broken targets by source file for a single read/write pass per file.
    let mut by_source: HashMap<&PathBuf, HashSet<&str>> = HashMap::new();
    for (source, target) in broken_links {
        by_source.entry(source).or_default().insert(target.as_str());
    }

    let mut total_unlinked = 0usize;
    let mut files_changed = 0usize;

    for (source, targets) in &by_source {
        let content = match fs::read_to_string(source) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("warning: could not read {:?}: {}", source, e);
                continue;
            }
        };

        // Strip code regions + HTML comments while preserving byte offsets.
        let stripped = strip_code_regions(&content);
        let stripped = strip_html_comments(&stripped);

        // (byte_start, byte_end, replacement_text)
        let mut replacements: Vec<(usize, usize, String)> = Vec::new();

        for caps in wikilink_regex().captures_iter(&stripped) {
            let marker = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            if marker == "!" {
                continue;
            }

            let full_match = caps.get(0).expect("wikilink regex provides full match");
            let raw_target = caps.get(2).map(|m| m.as_str()).unwrap_or("");
            let alias = caps.get(3).map(|m| m.as_str());

            let normalized = match normalize_target(raw_target) {
                Some(n) => n,
                None => continue,
            };

            if !targets.contains(normalized.as_str()) {
                continue;
            }

            let replacement = alias.unwrap_or(&normalized).to_string();
            replacements.push((full_match.start(), full_match.end(), replacement));
        }

        if replacements.is_empty() {
            continue;
        }

        replacements.sort_by(|a, b| b.0.cmp(&a.0));

        let mut new_content = content.clone();
        for (start, end, replacement) in &replacements {
            new_content.replace_range(*start..*end, replacement);
        }

        total_unlinked += replacements.len();
        files_changed += 1;

        if !dry_run {
            if let Err(e) = fs::write(source, &new_content) {
                eprintln!("warning: could not write {:?}: {}", source, e);
            }
        }
    }

    (total_unlinked, files_changed)
}

fn analyze_graph(
    files: &[PathBuf],
    parsed_links: &[(PathBuf, Vec<String>, Vec<String>)],
    index: &HashMap<UniCase<String>, PathBuf>,
    known_assets: &HashSet<UniCase<String>>,
) -> Analysis {
    let mut outgoing: HashMap<PathBuf, Vec<PathBuf>> = files
        .iter()
        .cloned()
        .map(|path| (path, Vec::new()))
        .collect();
    let mut incoming: HashMap<PathBuf, Vec<PathBuf>> = files
        .iter()
        .cloned()
        .map(|path| (path, Vec::new()))
        .collect();
    let mut outgoing_embeds: HashMap<PathBuf, Vec<PathBuf>> = files
        .iter()
        .cloned()
        .map(|path| (path, Vec::new()))
        .collect();
    let mut incoming_embeds: HashMap<PathBuf, Vec<PathBuf>> = files
        .iter()
        .cloned()
        .map(|path| (path, Vec::new()))
        .collect();

    let mut broken_set: HashSet<(PathBuf, String)> = HashSet::new();
    let mut embed_count: usize = 0;

    for (source, links, embeds) in parsed_links {
        for target_name in links {
            if let Some(target_path) = index.get(&UniCase::new(target_name.clone())) {
                let target = target_path.clone();
                outgoing
                    .entry(source.clone())
                    .or_default()
                    .push(target.clone());
                incoming.entry(target).or_default().push(source.clone());
            } else if !known_assets.contains(&UniCase::new(target_name.clone())) {
                broken_set.insert((source.clone(), target_name.clone()));
            }
        }

        for target_name in embeds {
            embed_count += 1;

            if let Some(target_path) = index.get(&UniCase::new(target_name.clone())) {
                let target = target_path.clone();
                outgoing_embeds
                    .entry(source.clone())
                    .or_default()
                    .push(target.clone());
                incoming_embeds
                    .entry(target)
                    .or_default()
                    .push(source.clone());
            } else if !known_assets.contains(&UniCase::new(target_name.clone())) {
                broken_set.insert((source.clone(), target_name.clone()));
            }
        }
    }

    let mut missing_backlinks_set: HashSet<(PathBuf, PathBuf)> = HashSet::new();

    for (source, targets) in &outgoing {
        let mut dedup_targets: HashSet<PathBuf> = HashSet::new();

        for target in targets {
            if !dedup_targets.insert(target.clone()) {
                continue;
            }

            let target_outgoing = outgoing.get(target).map(Vec::as_slice).unwrap_or(&[]);
            if !target_outgoing.iter().any(|path| path == source) {
                missing_backlinks_set.insert((source.clone(), target.clone()));
            }
        }
    }

    let mut missing_backlinks: Vec<(PathBuf, PathBuf)> =
        missing_backlinks_set.into_iter().collect();
    missing_backlinks.sort_by(|(s1, t1), (s2, t2)| s1.cmp(s2).then_with(|| t1.cmp(t2)));

    let mut orphans: Vec<PathBuf> = files
        .iter()
        .filter(|file| {
            outgoing.get(*file).is_some_and(Vec::is_empty)
                && incoming.get(*file).is_some_and(Vec::is_empty)
                && outgoing_embeds.get(*file).is_some_and(Vec::is_empty)
                && incoming_embeds.get(*file).is_some_and(Vec::is_empty)
        })
        .cloned()
        .collect();
    orphans.sort();

    let mut broken_links: Vec<(PathBuf, String)> = broken_set.into_iter().collect();
    broken_links.sort_by(|(s1, t1), (s2, t2)| s1.cmp(s2).then_with(|| t1.cmp(t2)));

    Analysis {
        missing_backlinks,
        orphans,
        broken_links,
        embed_count,
    }
}

fn should_show_all(cli: &Cli) -> bool {
    !cli.asymmetry && !cli.orphans && !cli.broken
}

fn filter_orphans_for_report<'a>(orphans: &'a [PathBuf], orphan_days: Option<u64>) -> Vec<&'a PathBuf> {
    let Some(days) = orphan_days else {
        return orphans.iter().collect();
    };

    let cutoff = SystemTime::now().checked_sub(Duration::from_secs(days.saturating_mul(86_400)));

    orphans
        .iter()
        .filter(|orphan| {
            std::fs::metadata(orphan)
                .ok()
                .and_then(|m| m.modified().ok())
                .zip(cutoff)
                .is_some_and(|(modified, threshold)| modified >= threshold)
        })
        .collect()
}

fn print_human_report(
    vault_root: &Path,
    total_files: usize,
    total_assets: usize,
    analysis: &Analysis,
    cli: &Cli,
) {
    let use_color = io::stdout().is_terminal();
    let _show_all = should_show_all(cli);
    let visible_orphans = filter_orphans_for_report(&analysis.orphans, cli.orphan_days);
    let visible_orphan_count = visible_orphans.len();

    let heading = |title: &str, count: usize, noun_singular: &str, noun_plural: &str| -> String {
        let noun = pluralize(count, noun_singular, noun_plural);
        let text = format!("=== {title} ({count} {noun}) ===");
        if use_color {
            format!("\x1b[1;36m{text}\x1b[0m")
        } else {
            text
        }
    };

    let orphans_hint = if cli.details || cli.orphans {
        ""
    } else if cli.orphan_days.is_some() {
        "   (--orphans to list)"
    } else {
        "   (--orphan-days N to filter by recency)"
    };
    let broken_hint = if cli.details || cli.broken {
        ""
    } else {
        "   (--broken to list)"
    };

    println!(
        "Vault: {}  ({} notes, {} assets)",
        vault_root.display(),
        total_files,
        total_assets
    );
    let orphan_total_note = if cli.orphan_days.is_some() && visible_orphan_count < analysis.orphans.len() {
        format!(" (of {} total)", analysis.orphans.len())
    } else {
        String::new()
    };
    println!("  Orphans        {}{}{}", visible_orphan_count, orphan_total_note, orphans_hint);
    println!("  Broken links   {}{}", analysis.broken_links.len(), broken_hint);
    println!("  Embeds         {}", analysis.embed_count);
    if cli.asymmetry {
        let asymmetry_hint = if cli.details || cli.asymmetry {
            ""
        } else {
            "   (--asymmetry to list)"
        };
        println!(
            "  Asymmetric     {}{}",
            analysis.missing_backlinks.len(),
            asymmetry_hint
        );
    }

    let show_orphans_details = cli.details || cli.orphans;
    let show_broken_details = cli.details || cli.broken;
    let show_asymmetry_details = cli.details || cli.asymmetry;

    if show_asymmetry_details {
        println!();
        println!(
            "{}",
            heading(
                "Asymmetric Links",
                analysis.missing_backlinks.len(),
                "pair",
                "pairs"
            )
        );
        for (source, target) in &analysis.missing_backlinks {
            let source_rel = relativize(vault_root, source);
            let target_rel = relativize(vault_root, target);
            let target_name = target
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("target");
            println!("  {source_rel} -> {target_rel} ({target_name} does not link back)");
        }
    }

    if show_orphans_details {
        println!();
        let orphan_heading_title = if let Some(days) = cli.orphan_days {
            format!("Orphans (\u{2264} {days}d)")
        } else {
            "Orphans".to_string()
        };
        println!(
            "{}",
            heading(&orphan_heading_title, visible_orphan_count, "note", "notes")
        );
        for orphan in &visible_orphans {
            println!("  {}", relativize(vault_root, orphan));
        }
    }

    if show_broken_details {
        println!();
        println!(
            "{}",
            heading("Broken Links", analysis.broken_links.len(), "link", "links")
        );
        for (source, target) in &analysis.broken_links {
            println!("  {}: [[{}]]", relativize(vault_root, source), target);
        }
    }

}

fn build_json_report(vault_root: &Path, total_files: usize, total_assets: usize, analysis: &Analysis) -> Report {
    Report {
        vault: vault_root.to_string_lossy().into_owned(),
        total_files,
        total_assets,
        embed_count: analysis.embed_count,
        missing_backlinks: analysis
            .missing_backlinks
            .iter()
            .map(|(source, target)| MissingBacklink {
                source: relativize(vault_root, source),
                target: relativize(vault_root, target),
            })
            .collect(),
        orphans: analysis
            .orphans
            .iter()
            .map(|path| relativize(vault_root, path))
            .collect(),
        broken_links: analysis
            .broken_links
            .iter()
            .map(|(source, target)| BrokenLink {
                source: relativize(vault_root, source),
                target: target.clone(),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trama::should_skip_entry;

    fn pb(path: &str) -> PathBuf {
        PathBuf::from(path)
    }

    #[test]
    fn extract_wikilinks_basic_and_alias() {
        let text = "See [[target]] and [[another target|Alias Here]]";
        let (links, embeds) = extract_wikilinks(text);
        assert_eq!(links, vec!["target", "another target"]);
        assert!(embeds.is_empty());
    }

    #[test]
    fn extract_wikilinks_ignores_code_regions() {
        let text = r#"
Outside [[Real]]

```md
[[InFence]]
```

`[[InInline]]`
"#;

        let (links, embeds) = extract_wikilinks(text);
        assert_eq!(links, vec!["Real"]);
        assert!(embeds.is_empty());
    }

    #[test]
    fn extract_wikilinks_empty_text() {
        let (links, embeds) = extract_wikilinks("");
        assert!(links.is_empty());
        assert!(embeds.is_empty());
    }

    #[test]
    fn extract_wikilinks_distinguishes_embeds() {
        let text = "See [[link]] and ![[embed]] here";
        let (links, embeds) = extract_wikilinks(text);
        assert_eq!(links, vec!["link"]);
        assert_eq!(embeds, vec!["embed"]);
    }

    #[test]
    fn extract_wikilinks_ignores_html_comments() {
        let text = r#"
Outside [[Real]]
<!-- [[Hidden]] -->
<!--
![[HiddenEmbed]]
[[HiddenToo]]
-->
"#;

        let (links, embeds) = extract_wikilinks(text);
        assert_eq!(links, vec!["Real"]);
        assert!(embeds.is_empty());
    }

    #[test]
    fn exclusions_skip_obsidian_components() {
        let path = Path::new("/vault/.obsidian/plugins/a.md");
        assert!(should_skip_entry(path, &[]));
    }

    #[test]
    fn graph_construction_reports_all_categories() {
        let files = vec![
            pb("/vault/A.md"),
            pb("/vault/B.md"),
            pb("/vault/C.md"),
            pb("/vault/D.md"),
            pb("/vault/E.md"),
        ];

        let index = build_file_index(&files);

        let parsed_links = vec![
            (
                pb("/vault/A.md"),
                vec!["B".to_string(), "Missing".to_string()],
                vec![],
            ),
            (pb("/vault/B.md"), vec![], vec![]),
            (pb("/vault/C.md"), vec!["A".to_string()], vec![]),
            (pb("/vault/D.md"), vec![], vec![]),
            (pb("/vault/E.md"), vec![], vec![]),
        ];

        let known_assets = HashSet::new();
        let analysis = analyze_graph(&files, &parsed_links, &index, &known_assets);

        assert_eq!(analysis.missing_backlinks.len(), 2);
        assert!(analysis
            .missing_backlinks
            .contains(&(pb("/vault/A.md"), pb("/vault/B.md"))));
        assert!(analysis
            .missing_backlinks
            .contains(&(pb("/vault/C.md"), pb("/vault/A.md"))));

        assert_eq!(analysis.orphans, vec![pb("/vault/D.md"), pb("/vault/E.md")]);

        assert_eq!(analysis.broken_links.len(), 1);
        assert_eq!(
            analysis.broken_links[0],
            (pb("/vault/A.md"), "Missing".to_string())
        );
        assert_eq!(analysis.embed_count, 0);
    }

    #[test]
    fn case_insensitive_matching_resolves_targets() {
        let files = vec![pb("/vault/capco.md"), pb("/vault/source.md")];
        let index = build_file_index(&files);

        let parsed_links = vec![(pb("/vault/source.md"), vec!["Capco".to_string()], vec![])];
        let known_assets = HashSet::new();
        let analysis = analyze_graph(&files, &parsed_links, &index, &known_assets);

        assert!(analysis.broken_links.is_empty());
        assert!(analysis
            .missing_backlinks
            .contains(&(pb("/vault/source.md"), pb("/vault/capco.md"))));
    }

    #[test]
    fn build_file_index_warns_on_duplicate_stem() {
        let files = vec![pb("/vault/Capco/Bertie Haskins Profile.md"), pb("/vault/People/Bertie Haskins Profile.md")];
        let index = build_file_index(&files);

        assert_eq!(index.len(), 1);
        assert_eq!(
            index.get(&UniCase::new("Bertie Haskins Profile".to_string())),
            Some(&pb("/vault/Capco/Bertie Haskins Profile.md"))
        );
    }

    #[test]
    fn unlink_broken_links_basic() {
        let path = PathBuf::from("/tmp/nexis-test-unlink.md");
        let content = "See [[Dead Note]] and [[Dead Note|Alias]] and [[Live Note]].\n";
        fs::write(&path, content).unwrap();

        let broken = vec![(path.clone(), "Dead Note".to_string())];
        let (unlinked, changed) = unlink_broken_links(&broken, false);

        assert_eq!(unlinked, 2);
        assert_eq!(changed, 1);

        let result = fs::read_to_string(&path).unwrap();
        assert!(result.contains("Dead Note") && !result.contains("[[Dead Note]]"));
        assert!(result.contains("Alias") && !result.contains("[[Dead Note|Alias]]"));
        assert!(result.contains("[[Live Note]]"));

        let _ = fs::remove_file(&path);
    }
}
