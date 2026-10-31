//! A bridge between Letta's MemFS and Claude Code's memory directory.
//!
//! Both agents store memory the same way: one fact per Markdown file, with YAML
//! frontmatter carrying a `description` that decides when the fact is relevant.
//! That shared substrate is what makes a bridge possible at all — unlike a
//! memory-native model, whose state never leaves the model.
//!
//! The two layouts differ in three ways, and this module exists to reconcile
//! them:
//!
//! | | Letta MemFS | Claude Code |
//! | --- | --- | --- |
//! | location | `$MEMORY_DIR/system/*.md` | `<memory>/[a-z-]*.md` |
//! | index | none (the directory is the index) | `MEMORY.md`, one line per file |
//! | extra frontmatter | `description` | `name`, `description`, `metadata.type` |
//!
//! Syncing is explicit and never automatic: memory is long-lived context that
//! steers an agent's behaviour, so a surprise write is expensive to undo. Every
//! operation reports what it would change, and `--dry-run` shows the plan
//! without touching either side.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A single memory fact, in whichever layout it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Memo {
    /// Stem of the file name, used as the identity on both sides.
    pub name: String,
    /// The `description` frontmatter field; both sides use it for recall.
    pub description: String,
    /// Claude Code's `metadata.type`. MemFS has no equivalent, so a memo
    /// imported from MemFS gets `reference`.
    pub kind: String,
    /// Everything after the frontmatter.
    pub body: String,
}

impl Memo {
    /// Render in Claude Code's format: name, description, and metadata.type.
    pub fn to_claude(&self) -> String {
        format!(
            "---\nname: {}\ndescription: {}\nmetadata:\n  type: {}\n---\n\n{}",
            self.name,
            self.description,
            self.kind,
            self.body.trim_end()
        ) + "\n"
    }

    /// Render in MemFS's format.
    ///
    /// Both `name` and `description` are required: Letta validates memory on
    /// commit and rejects a file missing either, so a `description`-only file
    /// is written but then refused by the MemFS git hook. `metadata` has no
    /// MemFS equivalent and is left out.
    pub fn to_memfs(&self) -> String {
        format!(
            "---\nname: {}\ndescription: {}\n---\n\n{}",
            self.name,
            self.description,
            self.body.trim_end()
        ) + "\n"
    }

    /// The one-line entry for Claude Code's `MEMORY.md` index.
    pub fn index_line(&self) -> String {
        let hook = self
            .description
            .split(" — ")
            .next()
            .unwrap_or(&self.description);
        let title = title_from_name(&self.name);
        format!("- [{}]({}.md) — {}", title, self.name, hook.trim())
    }
}

/// `some-slug-here` -> `Some slug here`, for a readable index entry.
fn title_from_name(name: &str) -> String {
    let spaced = name.replace(['-', '_'], " ");
    let mut chars = spaced.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Split a Markdown file into its YAML frontmatter and body.
///
/// Returns `None` when the file has no frontmatter: MemFS requires it, and a
/// file without it is not a memory fact (a stray README, say).
fn split_frontmatter(contents: &str) -> Option<(&str, &str)> {
    let rest = contents.strip_prefix("---\n")?;
    let end = rest
        .find("\n---\n")
        .or_else(|| rest.strip_suffix("\n---").map(|trimmed| trimmed.len()))?;
    let frontmatter = &rest[..end];
    let body = rest[end..].strip_prefix("\n---\n").unwrap_or("");
    Some((frontmatter, body))
}

/// Read one scalar field out of YAML frontmatter.
///
/// Deliberately not a YAML parser: these files have a flat, known shape, and
/// adding a YAML dependency to read two fields is not a good trade. Nested
/// keys are addressed as `metadata.type`.
fn frontmatter_field(frontmatter: &str, key: &str) -> Option<String> {
    let (parent, leaf) = match key.split_once('.') {
        Some((parent, leaf)) => (Some(parent), leaf),
        None => (None, key),
    };

    let mut in_parent = parent.is_none();
    for line in frontmatter.lines() {
        let indented = line.starts_with(' ') || line.starts_with('\t');
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if let Some(parent) = parent {
            // A non-indented line either opens our parent or leaves it.
            if !indented {
                in_parent = trimmed
                    .strip_suffix(':')
                    .map(|name| name.trim() == parent)
                    .unwrap_or(false);
                continue;
            }
            if !in_parent {
                continue;
            }
        } else if indented {
            // Skip the children of some other key.
            continue;
        }

        if let Some((name, value)) = trimmed.split_once(':') {
            if name.trim() == leaf {
                let value = value.trim();
                let value = value
                    .strip_prefix('"')
                    .and_then(|rest| rest.strip_suffix('"'))
                    .unwrap_or(value);
                if !value.is_empty() {
                    return Some(value.to_owned());
                }
            }
        }
    }
    None
}

/// Parse a memory file. `stem` is the fallback identity when the file carries
/// no `name` (MemFS files do not).
pub fn parse_memo(stem: &str, contents: &str) -> Option<Memo> {
    let (frontmatter, body) = split_frontmatter(contents)?;
    let description = frontmatter_field(frontmatter, "description")?;
    Some(Memo {
        name: frontmatter_field(frontmatter, "name").unwrap_or_else(|| stem.to_owned()),
        description,
        kind: frontmatter_field(frontmatter, "metadata.type")
            .unwrap_or_else(|| "reference".to_owned()),
        body: body.trim_start_matches('\n').to_owned(),
    })
}

/// Read every memory file in a directory, keyed by name.
///
/// Files without frontmatter are skipped rather than treated as an error, so an
/// index or a README in the same directory does no harm.
fn read_dir(dir: &Path) -> Result<BTreeMap<String, Memo>, String> {
    let mut memos = BTreeMap::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        // A missing directory is an empty one: the other side may not have been
        // created yet.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(memos),
        Err(error) => return Err(format!("cannot read {}: {error}", dir.display())),
    };

    for entry in entries {
        let entry = entry.map_err(|error| format!("cannot read {}: {error}", dir.display()))?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        // Claude Code's index is not itself a memory.
        if stem == "MEMORY" {
            continue;
        }
        let contents = std::fs::read_to_string(&path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        if let Some(memo) = parse_memo(stem, &contents) {
            memos.insert(memo.name.clone(), memo);
        }
    }
    Ok(memos)
}

/// Which way memory is being copied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// MemFS -> Claude Code.
    Import,
    /// Claude Code -> MemFS.
    Export,
}

/// What a sync would do to one memo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Create(String),
    Update(String),
}

impl Change {
    pub fn name(&self) -> &str {
        match self {
            Self::Create(name) | Self::Update(name) => name,
        }
    }
}

/// The plan for one sync: what to write, and what was already in step.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub changes: Vec<Change>,
    pub unchanged: Vec<String>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    pub fn render(&self, direction: Direction) -> String {
        let arrow = match direction {
            Direction::Import => "MemFS -> Claude Code",
            Direction::Export => "Claude Code -> MemFS",
        };
        if self.changes.is_empty() {
            return format!(
                "{arrow}: nothing to do ({} already in sync)\n",
                self.unchanged.len()
            );
        }
        let mut out = format!("{arrow}:\n");
        for change in &self.changes {
            let verb = match change {
                Change::Create(_) => "create",
                Change::Update(_) => "update",
            };
            out.push_str(&format!("  {verb} {}\n", change.name()));
        }
        if !self.unchanged.is_empty() {
            out.push_str(&format!("  ({} already in sync)\n", self.unchanged.len()));
        }
        out
    }
}

/// Compare the two sides and decide what needs writing.
///
/// Only `description` and `body` are compared. A memo's `kind` is Claude
/// Code-only, so a MemFS round trip would otherwise report a spurious change
/// on every file.
pub fn plan(source: &BTreeMap<String, Memo>, target: &BTreeMap<String, Memo>) -> Plan {
    let mut plan = Plan::default();
    for (name, memo) in source {
        match target.get(name) {
            None => plan.changes.push(Change::Create(name.clone())),
            Some(existing)
                if existing.description != memo.description
                    || existing.body.trim() != memo.body.trim() =>
            {
                plan.changes.push(Change::Update(name.clone()))
            }
            Some(_) => plan.unchanged.push(name.clone()),
        }
    }
    plan
}

/// Where the memory files live inside a MemFS root.
///
/// MemFS uses two layouts in practice, so the directory is detected rather
/// than assumed:
///
/// - `<root>/system/*.md`, which the bundled authoring instructions describe;
/// - `<root>/*.md`, which is what Letta's local backend actually creates
///   (`~/.letta/lc-local-backend/memfs/<agent>/memory/`).
///
/// Detection prefers whichever layout already holds memory files, so a sync
/// writes back into the layout the directory is already using instead of
/// imposing one. An empty or absent root falls back to `system/`, matching the
/// documented layout for a directory we are creating from scratch.
pub fn memfs_system_dir(memory_dir: &Path) -> PathBuf {
    let nested = memory_dir.join("system");
    if has_memos(&nested) {
        return nested;
    }
    if has_memos(memory_dir) {
        return memory_dir.to_path_buf();
    }
    nested
}

/// True when a directory holds at least one parseable memory file.
///
/// Presence of `.md` files is not enough: a root may hold only `MEMORY.md`, or
/// a stray README, neither of which is a memory fact.
fn has_memos(dir: &Path) -> bool {
    read_dir(dir)
        .map(|memos| !memos.is_empty())
        .unwrap_or(false)
}

/// Rebuild Claude Code's `MEMORY.md` index from the files present.
///
/// The index is derived, never merged: it must list exactly the memory files
/// that exist, so a stale entry pointing at a deleted file cannot survive.
fn write_index(dir: &Path, memos: &BTreeMap<String, Memo>) -> Result<(), String> {
    let mut body = String::new();
    for memo in memos.values() {
        body.push_str(&memo.index_line());
        body.push('\n');
    }
    let path = dir.join("MEMORY.md");
    std::fs::write(&path, body).map_err(|error| format!("cannot write {}: {error}", path.display()))
}

/// Copy memory from one side to the other.
///
/// Returns the plan that was applied (or, with `dry_run`, would have been).
pub fn sync(
    memfs_dir: &Path,
    claude_dir: &Path,
    direction: Direction,
    dry_run: bool,
) -> Result<Plan, String> {
    let system = memfs_system_dir(memfs_dir);
    let memfs = read_dir(&system)?;
    let claude = read_dir(claude_dir)?;

    let (source, target, target_dir) = match direction {
        Direction::Import => (&memfs, &claude, claude_dir.to_path_buf()),
        Direction::Export => (&claude, &memfs, system.clone()),
    };

    let plan = plan(source, target);
    if dry_run || plan.is_empty() {
        return Ok(plan);
    }

    std::fs::create_dir_all(&target_dir)
        .map_err(|error| format!("cannot create {}: {error}", target_dir.display()))?;

    for change in &plan.changes {
        let memo = source
            .get(change.name())
            .ok_or_else(|| format!("internal error: {} vanished", change.name()))?;
        let path = target_dir.join(format!("{}.md", memo.name));
        let rendered = match direction {
            Direction::Import => memo.to_claude(),
            Direction::Export => memo.to_memfs(),
        };
        std::fs::write(&path, rendered)
            .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    }

    // Claude Code reads MEMORY.md as its index, so it has to describe whatever
    // we just wrote.
    if direction == Direction::Import {
        let merged = read_dir(claude_dir)?;
        write_index(claude_dir, &merged)?;
    }

    Ok(plan)
}

/// Resolve the MemFS root: an explicit path, else `$MEMORY_DIR`.
pub fn resolve_memfs_dir(explicit: Option<PathBuf>) -> Result<PathBuf, String> {
    if let Some(dir) = explicit {
        return Ok(dir);
    }
    std::env::var_os("MEMORY_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| {
            "no MemFS directory: pass --memory-dir or set MEMORY_DIR (Letta sets it \
             for an agent session)"
                .to_owned()
        })
}

/// Resolve Claude Code's memory directory for a project.
///
/// Claude Code derives the directory from the project path by replacing path
/// separators with dashes.
pub fn resolve_claude_dir(explicit: Option<PathBuf>, project: &Path) -> Result<PathBuf, String> {
    if let Some(dir) = explicit {
        return Ok(dir);
    }
    let home = crate::home_dir();
    let slug: String = project
        .to_string_lossy()
        .chars()
        .map(|ch| if ch == '/' { '-' } else { ch })
        .collect();
    Ok(home
        .join(".claude/current/projects")
        .join(slug)
        .join("memory"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MEMFS_FILE: &str = "\
---
description: The plugin PATH gotcha.
---

Herdr runs plugin commands with the server's PATH.
";

    const CLAUDE_FILE: &str = "\
---
name: plugin-path
description: The plugin PATH gotcha.
metadata:
  type: reference
---

Herdr runs plugin commands with the server's PATH.
";

    #[test]
    fn parses_a_memfs_file_and_defaults_the_metadata() {
        let memo = parse_memo("plugin-path", MEMFS_FILE).unwrap();
        assert_eq!(memo.name, "plugin-path");
        assert_eq!(memo.description, "The plugin PATH gotcha.");
        // MemFS has no type; reference is the safe default.
        assert_eq!(memo.kind, "reference");
        assert!(memo.body.contains("server's PATH"));
    }

    #[test]
    fn parses_a_claude_file_including_nested_metadata() {
        let memo = parse_memo("ignored-stem", CLAUDE_FILE).unwrap();
        // The frontmatter name wins over the file stem.
        assert_eq!(memo.name, "plugin-path");
        assert_eq!(memo.kind, "reference");
    }

    #[test]
    fn nested_lookup_does_not_confuse_sibling_keys() {
        let frontmatter = "name: a\nother:\n  type: wrong\nmetadata:\n  type: feedback";
        assert_eq!(
            frontmatter_field(frontmatter, "metadata.type"),
            Some("feedback".to_owned())
        );
        // A top-level lookup must not pick up an indented child.
        assert_eq!(frontmatter_field(frontmatter, "type"), None);
        assert_eq!(frontmatter_field(frontmatter, "name"), Some("a".to_owned()));
    }

    #[test]
    fn a_file_without_frontmatter_is_not_a_memo() {
        assert!(parse_memo("readme", "# Just a heading\n").is_none());
        assert!(parse_memo("empty", "").is_none());
        // Frontmatter without a description cannot be recalled, so it is not a
        // memory fact.
        assert!(parse_memo("x", "---\nname: x\n---\n\nbody\n").is_none());
    }

    #[test]
    fn round_trips_between_the_two_formats() {
        let memo = parse_memo("plugin-path", MEMFS_FILE).unwrap();
        let claude = memo.to_claude();
        assert!(claude.contains("name: plugin-path"));
        assert!(claude.contains("metadata:\n  type: reference"));

        let back = parse_memo("plugin-path", &claude).unwrap();
        assert_eq!(back.description, memo.description);
        assert_eq!(back.body.trim(), memo.body.trim());

        let memfs = back.to_memfs();
        // Letta validates on commit and requires both fields.
        assert!(memfs.contains("name: plugin-path"), "{memfs}");
        assert!(memfs.contains("description:"), "{memfs}");
        // `metadata` is Claude Code-only and must not leak in.
        assert!(!memfs.contains("metadata"));
    }

    #[test]
    fn index_lines_are_readable_and_link_correctly() {
        let memo = Memo {
            name: "letta-memfs-is-plain-git-markdown".to_owned(),
            description: "Short hook — and a longer tail that should be dropped.".to_owned(),
            kind: "reference".to_owned(),
            body: "body".to_owned(),
        };
        let line = memo.index_line();
        assert_eq!(
            line,
            "- [Letta memfs is plain git markdown](letta-memfs-is-plain-git-markdown.md) — Short hook"
        );
    }

    #[test]
    fn plan_distinguishes_create_update_and_unchanged() {
        let mut source = BTreeMap::new();
        source.insert(
            "same".to_owned(),
            Memo {
                name: "same".to_owned(),
                description: "d".to_owned(),
                kind: "reference".to_owned(),
                body: "b".to_owned(),
            },
        );
        source.insert(
            "changed".to_owned(),
            Memo {
                name: "changed".to_owned(),
                description: "new".to_owned(),
                kind: "reference".to_owned(),
                body: "b".to_owned(),
            },
        );
        source.insert(
            "fresh".to_owned(),
            Memo {
                name: "fresh".to_owned(),
                description: "d".to_owned(),
                kind: "reference".to_owned(),
                body: "b".to_owned(),
            },
        );

        let mut target = BTreeMap::new();
        target.insert(
            "same".to_owned(),
            Memo {
                name: "same".to_owned(),
                description: "d".to_owned(),
                // A differing kind alone must not count as a change: MemFS
                // cannot represent it.
                kind: "project".to_owned(),
                body: "b".to_owned(),
            },
        );
        target.insert(
            "changed".to_owned(),
            Memo {
                name: "changed".to_owned(),
                description: "old".to_owned(),
                kind: "reference".to_owned(),
                body: "b".to_owned(),
            },
        );

        let plan = plan(&source, &target);
        assert_eq!(plan.unchanged, ["same"]);
        let mut names: Vec<&str> = plan.changes.iter().map(Change::name).collect();
        names.sort();
        assert_eq!(names, ["changed", "fresh"]);
        assert!(plan.changes.contains(&Change::Update("changed".to_owned())));
        assert!(plan.changes.contains(&Change::Create("fresh".to_owned())));
    }

    #[test]
    fn import_writes_claude_files_and_rebuilds_the_index() {
        let root = tempfile::tempdir().unwrap();
        let memfs = root.path().join("memfs");
        let claude = root.path().join("claude");
        std::fs::create_dir_all(memfs.join("system")).unwrap();
        std::fs::write(memfs.join("system/plugin-path.md"), MEMFS_FILE).unwrap();
        // A file without frontmatter must be ignored, not imported.
        std::fs::write(memfs.join("system/README.md"), "# not a memo\n").unwrap();

        let plan = sync(&memfs, &claude, Direction::Import, false).unwrap();
        assert_eq!(plan.changes, [Change::Create("plugin-path".to_owned())]);

        let written = std::fs::read_to_string(claude.join("plugin-path.md")).unwrap();
        assert!(written.contains("name: plugin-path"));
        assert!(written.contains("metadata:"));

        let index = std::fs::read_to_string(claude.join("MEMORY.md")).unwrap();
        assert!(index.contains("(plugin-path.md)"));
        assert!(!index.contains("README"));
    }

    #[test]
    fn export_writes_memfs_files_under_system() {
        let root = tempfile::tempdir().unwrap();
        let memfs = root.path().join("memfs");
        let claude = root.path().join("claude");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::write(claude.join("plugin-path.md"), CLAUDE_FILE).unwrap();
        // The index is not a memory and must not be exported.
        std::fs::write(claude.join("MEMORY.md"), "- [x](x.md) — y\n").unwrap();

        let plan = sync(&memfs, &claude, Direction::Export, false).unwrap();
        assert_eq!(plan.changes, [Change::Create("plugin-path".to_owned())]);

        let written = std::fs::read_to_string(memfs.join("system/plugin-path.md")).unwrap();
        // Letta requires both name and description; metadata is ours alone.
        assert!(written.contains("name: plugin-path"), "{written}");
        assert!(written.contains("description:"), "{written}");
        assert!(!written.contains("metadata"));
        assert!(!memfs.join("system/MEMORY.md").exists());
    }

    #[test]
    fn a_second_sync_is_a_no_op() {
        let root = tempfile::tempdir().unwrap();
        let memfs = root.path().join("memfs");
        let claude = root.path().join("claude");
        std::fs::create_dir_all(memfs.join("system")).unwrap();
        std::fs::write(memfs.join("system/plugin-path.md"), MEMFS_FILE).unwrap();

        sync(&memfs, &claude, Direction::Import, false).unwrap();
        let again = sync(&memfs, &claude, Direction::Import, false).unwrap();
        assert!(again.is_empty(), "{:?}", again.changes);
        assert_eq!(again.unchanged, ["plugin-path"]);

        // And back the other way: the round trip must also settle.
        let back = sync(&memfs, &claude, Direction::Export, false).unwrap();
        assert!(back.is_empty(), "{:?}", back.changes);
    }

    #[test]
    fn dry_run_changes_nothing_on_disk() {
        let root = tempfile::tempdir().unwrap();
        let memfs = root.path().join("memfs");
        let claude = root.path().join("claude");
        std::fs::create_dir_all(memfs.join("system")).unwrap();
        std::fs::write(memfs.join("system/plugin-path.md"), MEMFS_FILE).unwrap();

        let plan = sync(&memfs, &claude, Direction::Import, true).unwrap();
        assert_eq!(plan.changes.len(), 1);
        assert!(!claude.exists(), "dry run created the target directory");
    }

    /// The shape Letta's local backend actually writes: files at the memory
    /// root, with `name` and `description` but no `metadata`.
    const LIVE_FILE: &str = "\
---
name: Human
description: What I've learned about the person I'm working with.
---
Learn sideways, through the work.
Not a questionnaire.
";

    #[test]
    fn detects_the_live_root_layout() {
        let root = tempfile::tempdir().unwrap();
        let memory = root.path().join("memory");
        std::fs::create_dir_all(&memory).unwrap();
        std::fs::write(memory.join("human.md"), LIVE_FILE).unwrap();
        // A bare index at the root must not be mistaken for a memory file.
        std::fs::write(memory.join("MEMORY.md"), "# Memory\n").unwrap();

        assert_eq!(memfs_system_dir(&memory), memory);
    }

    #[test]
    fn prefers_the_system_layout_when_it_holds_the_memos() {
        let root = tempfile::tempdir().unwrap();
        let memory = root.path().join("memory");
        std::fs::create_dir_all(memory.join("system")).unwrap();
        std::fs::write(memory.join("system/a.md"), MEMFS_FILE).unwrap();
        // A root-level index must not pull detection away from system/.
        std::fs::write(memory.join("MEMORY.md"), "# Memory\n").unwrap();

        assert_eq!(memfs_system_dir(&memory), memory.join("system"));
    }

    #[test]
    fn an_empty_root_defaults_to_the_documented_layout() {
        let root = tempfile::tempdir().unwrap();
        let memory = root.path().join("memory");
        std::fs::create_dir_all(&memory).unwrap();
        assert_eq!(memfs_system_dir(&memory), memory.join("system"));
        // And a root that does not exist at all.
        assert_eq!(
            memfs_system_dir(&root.path().join("absent")),
            root.path().join("absent/system")
        );
    }

    #[test]
    fn imports_from_a_live_root_layout_directory() {
        let root = tempfile::tempdir().unwrap();
        let memory = root.path().join("memory");
        let claude = root.path().join("claude");
        std::fs::create_dir_all(&memory).unwrap();
        std::fs::write(memory.join("human.md"), LIVE_FILE).unwrap();
        std::fs::write(memory.join("MEMORY.md"), "# Memory\n").unwrap();

        let plan = sync(&memory, &claude, Direction::Import, false).unwrap();
        // The frontmatter `name` is the identity, not the file stem.
        assert_eq!(plan.changes, [Change::Create("Human".to_owned())]);

        let written = std::fs::read_to_string(claude.join("Human.md")).unwrap();
        assert!(written.contains("name: Human"));
        // MemFS carries no type, so the import defaults it.
        assert!(written.contains("type: reference"));

        // Writing back must land in the same layout, not create system/.
        let back = sync(&memory, &claude, Direction::Export, false).unwrap();
        assert!(back.is_empty(), "{:?}", back.changes);
        assert!(!memory.join("system").exists());
    }

    #[test]
    fn a_missing_source_directory_is_empty_not_an_error() {
        let root = tempfile::tempdir().unwrap();
        let plan = sync(
            &root.path().join("absent"),
            &root.path().join("also-absent"),
            Direction::Import,
            false,
        )
        .unwrap();
        assert!(plan.is_empty());
    }

    #[test]
    fn plans_render_for_a_human() {
        let plan = Plan {
            changes: vec![
                Change::Create("a".to_owned()),
                Change::Update("b".to_owned()),
            ],
            unchanged: vec!["c".to_owned()],
        };
        let text = plan.render(Direction::Import);
        assert!(text.contains("MemFS -> Claude Code"));
        assert!(text.contains("create a"));
        assert!(text.contains("update b"));
        assert!(text.contains("1 already in sync"));

        let empty = Plan {
            changes: vec![],
            unchanged: vec!["c".to_owned()],
        };
        assert!(empty.render(Direction::Export).contains("nothing to do"));
    }

    #[test]
    fn claude_dir_is_derived_from_the_project_path() {
        let dir = resolve_claude_dir(None, Path::new("/home/u/herdr-infinity")).unwrap();
        let shown = dir.to_string_lossy();
        assert!(shown.contains("-home-u-herdr-infinity"), "{shown}");
        assert!(shown.ends_with("memory"), "{shown}");

        let explicit = resolve_claude_dir(Some(PathBuf::from("/tmp/x")), Path::new("/ignored"));
        assert_eq!(explicit.unwrap(), PathBuf::from("/tmp/x"));
    }
}
