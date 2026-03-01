use clap::{Parser, ValueEnum};
use rayon::prelude::*;
use regex::Regex;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, IsTerminal};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};
use unicase::UniCase;
use walkdir::WalkDir;

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

fn validate_vault_path(path: &Path) -> Result<PathBuf, String> {
    if !path.exists() {
        return Err(format!("vault path does not exist: {}", path.display()));
    }
    if !path.is_dir() {
        return Err(format!("vault path is not a directory: {}", path.display()));
    }

    fs::read_dir(path)
        .map_err(|err| format!("cannot read vault path {}: {err}", path.display()))?;

    Ok(absolute_path(path))
}

fn absolute_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(path)
        }
    })
}

fn should_skip_entry(path: &Path, excludes: &[String]) -> bool {
    path.components().any(|component| {
        let os = component.as_os_str();
        os == OsStr::new(".obsidian")
            || os == OsStr::new(".git")
            || os == OsStr::new(".trash")
            || excludes.iter().any(|exclude| os == OsStr::new(exclude))
    })
}

fn collect_markdown_files(vault_root: &Path, excludes: &[String]) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = WalkDir::new(vault_root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| !should_skip_entry(entry.path(), excludes))
        .filter_map(|e| e.ok())
        .filter(|entry| entry.file_type().is_file())
        .filter(|entry| {
            entry
                .path()
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext.eq_ignore_ascii_case("md"))
                .unwrap_or(false)
        })
        .map(|entry| entry.into_path())
        .collect();

    files.sort();
    files
}

fn collect_all_files(vault_root: &Path, excludes: &[String]) -> HashSet<UniCase<String>> {
    let mut known_assets: HashSet<UniCase<String>> = HashSet::new();

    for entry in WalkDir::new(vault_root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| !should_skip_entry(entry.path(), excludes))
        .filter_map(|e| e.ok())
        .filter(|entry| entry.file_type().is_file())
    {
        let path = entry.path();
        if let Some(file_name) = path.file_name().and_then(|n| n.to_str()) {
            known_assets.insert(UniCase::new(file_name.to_owned()));
        }
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            known_assets.insert(UniCase::new(stem.to_owned()));
        }
    }

    known_assets
}

fn build_file_index(files: &[PathBuf]) -> HashMap<UniCase<String>, PathBuf> {
    let mut index: HashMap<UniCase<String>, PathBuf> = HashMap::new();

    for path in files {
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            let key = UniCase::new(stem.to_owned());
            if let Some(existing) = index.get(&key) {
                eprintln!(
                    "warning: duplicate stem {:?} — {:?} shadows {:?}; links to [[{}]] will resolve to the first path only",
                    stem, existing, path, stem
                );
            } else {
                index.insert(key, path.clone());
            }
        }
    }

    index
}

fn fenced_code_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)```.*?```").expect("valid fenced-code regex"))
}

fn inline_code_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"`[^`\n]*`").expect("valid inline-code regex"))
}

fn html_comment_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)<!--.*?-->").expect("valid html comment regex"))
}

fn wikilink_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(!?)\[\[([^\]|][^\]]*?)(?:\|([^\]]*))?\]\]").expect("valid wikilink regex")
    })
}

fn replace_range_with_spaces(buf: &mut [u8], start: usize, end: usize) {
    for byte in buf.iter_mut().take(end).skip(start) {
        *byte = b' ';
    }
}

fn strip_code_regions(text: &str) -> String {
    let mut bytes = text.as_bytes().to_vec();

    for mat in fenced_code_regex().find_iter(text) {
        replace_range_with_spaces(&mut bytes, mat.start(), mat.end());
    }

    let after_fenced = String::from_utf8(bytes).expect("valid UTF-8 after fenced replacement");
    let mut bytes = after_fenced.as_bytes().to_vec();
    for mat in inline_code_regex().find_iter(&after_fenced) {
        replace_range_with_spaces(&mut bytes, mat.start(), mat.end());
    }

    String::from_utf8(bytes).expect("valid UTF-8 after inline replacement")
}

fn strip_html_comments(text: &str) -> String {
    let mut bytes = text.as_bytes().to_vec();

    for mat in html_comment_regex().find_iter(text) {
        replace_range_with_spaces(&mut bytes, mat.start(), mat.end());
    }

    String::from_utf8(bytes).expect("valid UTF-8 after html comment replacement")
}

fn normalize_target(raw_target: &str) -> Option<String> {
    let trimmed = raw_target.trim();
    if trimmed.is_empty() {
        return None;
    }

    let after_hash = trimmed
        .split_once('#')
        .map(|(head, _)| head)
        .unwrap_or(trimmed);

    let no_anchor = after_hash
        .split_once('^')
        .map(|(head, _)| head)
        .unwrap_or(after_hash)
        .trim();

    if no_anchor.is_empty() {
        return None;
    }

    let last_component = no_anchor
        .rsplit('/')
        .next()
        .unwrap_or(no_anchor)
        .trim()
        .to_string();

    if last_component.is_empty() {
        None
    } else {
        Some(last_component)
    }
}

fn extract_wikilinks(content: &str) -> (Vec<String>, Vec<String>) {
    let stripped = strip_code_regions(content);
    let stripped = strip_html_comments(&stripped);
    let mut links = Vec::new();
    let mut embeds = Vec::new();

    for caps in wikilink_regex().captures_iter(&stripped) {
        let marker = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let target = caps
            .get(2)
            .map(|m| m.as_str())
            .and_then(normalize_target);

        if let Some(target) = target {
            if marker == "!" {
                embeds.push(target);
            } else {
                links.push(target);
            }
        }
    }

    (links, embeds)
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

fn relativize(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

fn pluralize<'a>(count: usize, singular: &'a str, plural: &'a str) -> &'a str {
    if count == 1 {
        singular
    } else {
        plural
    }
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
}
