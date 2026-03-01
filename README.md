# nexis

Fast wikilink and backlink analysis for Obsidian markdown vaults.

```
nexis <vault-path>               # full report
nexis <vault-path> --backlinks   # missing backlinks only
nexis <vault-path> --orphans     # orphan notes only
nexis <vault-path> --broken      # broken links only
nexis <vault-path> --format json # machine-readable output
```

## What it reports

- **Missing backlinks** — A links to B, but B does not link back to A
- **Orphan notes** — notes with no incoming or outgoing links
- **Broken links** — `[[Target]]` where no `Target.md` exists in the vault

## Install

```bash
cargo install nexis
```

## Performance

~1s for 10,000+ notes on a warm SSD (rayon parallel parsing).

## Notes

- Case-insensitive filename matching (APFS-safe)
- Wikilinks inside code blocks are ignored
- `[[alias|target]]` syntax supported
- JSON output is agent-friendly; human output auto-detects TTY for colour

## License

MIT
