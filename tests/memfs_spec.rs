//! Regression tests for `docs/memfs-spec.md`.
//!
//! Each test is named for the clause it guards, so a failure names the
//! requirement that broke rather than an implementation detail. The spec is
//! the statement of intent; these are the executable half of it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use herdr_integration::memfs::{
    export_indexed, file_stem, memfs_system_dir, parse_memo, sync, sync_filtered, Direction,
    Filter, Memo,
};

/// A Claude Code memory file: name, description and a declared type.
fn claude_memo(name: &str, description: &str, kind: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\nmetadata:\n  type: {kind}\n---\n\nbody of {name}\n")
}

/// A MemFS memory file: no `metadata`, and in practice no `name` either.
fn memfs_memo(description: &str) -> String {
    format!("---\ndescription: {description}\n---\n\nbody\n")
}

fn write(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

/// A fresh `(root, memfs, claude)` triple. Neither side exists yet.
fn dirs() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let memfs = root.path().join("memfs");
    let claude = root.path().join("claude");
    (root, memfs, claude)
}

// ---------------------------------------------------------------- layout (M)

/// M1: a root that already uses `<root>/*.md` is not reorganised into
/// `system/`.
#[test]
fn m1_flat_layout_is_preserved() {
    let (_root, memfs, _claude) = dirs();
    write(&memfs.join("fact.md"), &memfs_memo("a flat-layout memory"));

    assert_eq!(memfs_system_dir(&memfs), memfs);
}

/// M1: a root using the documented `system/` layout keeps it.
#[test]
fn m1_nested_layout_is_preserved() {
    let (_root, memfs, _claude) = dirs();
    write(
        &memfs.join("system/fact.md"),
        &memfs_memo("a nested-layout memory"),
    );

    assert_eq!(memfs_system_dir(&memfs), memfs.join("system"));
}

/// M1: an empty or absent root falls back to the documented layout.
#[test]
fn m1_an_empty_root_falls_back_to_system() {
    let (_root, memfs, _claude) = dirs();

    assert_eq!(memfs_system_dir(&memfs), memfs.join("system"));
}

/// M2: a root holding only an index is not a memory directory, so detection
/// must not mistake it for the flat layout.
#[test]
fn m2_an_index_alone_is_not_memory() {
    let (_root, memfs, _claude) = dirs();
    write(&memfs.join("MEMORY.md"), "- [A](a.md) — hook\n");
    write(&memfs.join("README.md"), "not a memory\n");

    assert_eq!(memfs_system_dir(&memfs), memfs.join("system"));
}

/// M3: either side may be absent; that is empty, not an error.
#[test]
fn m3_a_missing_directory_is_empty_not_an_error() {
    let (_root, memfs, claude) = dirs();

    let plan = sync(&memfs, &claude, Direction::Import, false).unwrap();

    assert!(plan.is_empty());
}

/// M4: files that are not memories are skipped rather than failing the sync.
#[test]
fn m4_non_memory_files_are_skipped() {
    let (_root, memfs, claude) = dirs();
    write(&memfs.join("system/real.md"), &memfs_memo("a real memory"));
    write(&memfs.join("system/README.md"), "no frontmatter here\n");
    write(
        &memfs.join("system/notes.txt"),
        "---\ndescription: x\n---\n",
    );

    let plan = sync(&memfs, &claude, Direction::Import, false).unwrap();

    let names: Vec<&str> = plan.changes.iter().map(|c| c.name()).collect();
    assert_eq!(names, ["real"], "only the memory file should be copied");
    assert!(!claude.join("README.md").exists());
    assert!(!claude.join("notes.md").exists());
}

// ------------------------------------------------------------------ memo (F)

/// F1: no parseable `description` means it is not a memo.
#[test]
fn f1_description_is_required() {
    assert!(parse_memo("x", "no frontmatter at all\n").is_none());
    assert!(parse_memo("x", "---\nname: x\n---\n\nbody\n").is_none());
    assert!(parse_memo("x", "---\ndescription: d\n---\n\nbody\n").is_some());
}

/// F1: identity is `name`, falling back to the file stem for MemFS files.
#[test]
fn f1_identity_falls_back_to_the_file_stem() {
    let declared = parse_memo("stem", "---\nname: declared\ndescription: d\n---\n\nb\n").unwrap();
    assert_eq!(declared.name, "declared");

    let bare = parse_memo("stem", "---\ndescription: d\n---\n\nb\n").unwrap();
    assert_eq!(bare.name, "stem");
}

/// F2: an undeclared type reads as `reference` but is not filterable as one.
///
/// This is the clause that stops `--only-types reference` from sweeping up
/// every untyped memory in a project.
#[test]
fn f2_an_undeclared_type_is_reference_but_matches_no_filter() {
    let memo = parse_memo("x", "---\ndescription: d\n---\n\nb\n").unwrap();
    assert_eq!(memo.kind, "reference");
    assert!(!memo.kind_declared);

    let filter = Filter {
        types: vec!["reference".to_owned()],
    };
    assert!(
        !filter.keeps(&memo),
        "an undeclared type must not match an explicit filter"
    );

    let declared = parse_memo(
        "y",
        "---\ndescription: d\nmetadata:\n  type: reference\n---\n\nb\n",
    )
    .unwrap();
    assert!(filter.keeps(&declared));
}

/// F3: MemFS requires both `name` and `description`, and has no `metadata`.
#[test]
fn f3_each_side_is_written_in_its_own_format() {
    let memo = Memo {
        name: "a-fact".to_owned(),
        description: "what it is".to_owned(),
        kind: "feedback".to_owned(),
        kind_declared: true,
        body: "the body".to_owned(),
    };

    let memfs = memo.to_memfs();
    assert!(memfs.contains("name: a-fact"), "{memfs}");
    assert!(memfs.contains("description: what it is"), "{memfs}");
    assert!(
        !memfs.contains("metadata"),
        "metadata has no MemFS equivalent: {memfs}"
    );

    let claude = memo.to_claude();
    assert!(claude.contains("name: a-fact"), "{claude}");
    assert!(claude.contains("metadata:\n  type: feedback"), "{claude}");
}

/// F4: a free-text name becomes a safe stem — the `/` case would otherwise
/// fail the write with ENOENT.
#[test]
fn f4_a_free_text_name_becomes_a_safe_filename() {
    assert_eq!(
        file_stem("Letta MemFS is plain git/Markdown"),
        "letta-memfs-is-plain-git-markdown"
    );
    assert_eq!(
        file_stem("A name — with an em dash"),
        "a-name-with-an-em-dash"
    );
    assert_eq!(
        file_stem("letta 0.34.31"),
        "letta-0.34.31",
        "dots inside a word survive"
    );
    assert_eq!(
        file_stem(".hidden"),
        "hidden",
        "a leading dot never survives"
    );
    assert_eq!(
        file_stem("!!!"),
        "memo",
        "pure punctuation still needs a filename"
    );
}

/// F4: and the sanitised name is what actually reaches disk.
#[test]
fn f4_the_sanitised_name_is_what_lands_on_disk() {
    let (_root, memfs, claude) = dirs();
    write(
        &claude.join("x.md"),
        &claude_memo("Plugin PATH is inherited/not", "d", "reference"),
    );

    sync(&memfs, &claude, Direction::Export, false).unwrap();

    assert!(memfs
        .join("system/plugin-path-is-inherited-not.md")
        .exists());
}

// ------------------------------------------------------------------ sync (S)

/// S1: `--dry-run` reports a plan and writes nothing.
#[test]
fn s1_a_dry_run_writes_nothing() {
    let (_root, memfs, claude) = dirs();
    write(&memfs.join("system/fact.md"), &memfs_memo("a memory"));

    let plan = sync(&memfs, &claude, Direction::Import, true).unwrap();

    assert_eq!(plan.changes.len(), 1, "the plan still reports the work");
    assert!(!claude.exists(), "a dry run must not create the target");
}

/// S2: a type difference alone is not a change, or every MemFS round trip
/// would report every file as stale.
#[test]
fn s2_type_alone_does_not_make_a_memo_stale() {
    let (_root, memfs, claude) = dirs();
    // Same description and body; only the type differs, and MemFS cannot
    // carry a type at all.
    write(
        &claude.join("fact.md"),
        &claude_memo("fact", "same", "feedback"),
    );
    write(
        &memfs.join("system/fact.md"),
        "---\nname: fact\ndescription: same\n---\n\nbody of fact\n",
    );

    let plan = sync(&memfs, &claude, Direction::Export, true).unwrap();

    assert!(
        plan.is_empty(),
        "expected no change, got {:?}",
        plan.changes
    );
}

/// S2: a description change is a change.
#[test]
fn s2_a_changed_description_is_stale() {
    let (_root, memfs, claude) = dirs();
    write(
        &claude.join("fact.md"),
        &claude_memo("fact", "new", "feedback"),
    );
    write(
        &memfs.join("system/fact.md"),
        "---\nname: fact\ndescription: old\n---\n\nbody of fact\n",
    );

    let plan = sync(&memfs, &claude, Direction::Export, true).unwrap();

    assert_eq!(plan.changes.len(), 1, "expected an update");
}

/// S3: trailing whitespace is not a difference, so a hand-edited file does
/// not make a round trip oscillate forever.
#[test]
fn s3_trailing_whitespace_is_not_a_change() {
    let (_root, memfs, claude) = dirs();
    write(
        &claude.join("fact.md"),
        &claude_memo("fact", "d", "feedback"),
    );
    // The same memory as MemFS would hold it, but with the body padded the way
    // a hand edit or an editor's final-newline setting leaves it.
    write(
        &memfs.join("system/fact.md"),
        "---\nname: fact\ndescription: d\n---\n\nbody of fact\n\n\n",
    );

    let plan = sync(&memfs, &claude, Direction::Export, true).unwrap();

    assert!(
        plan.is_empty(),
        "padding alone must not count as a change: {:?}",
        plan.changes
    );
}

/// S3: import then export settles, and repeating either is a no-op.
#[test]
fn s3_round_trips_settle() {
    let (_root, memfs, claude) = dirs();
    write(&memfs.join("system/one.md"), &memfs_memo("first memory"));
    write(&memfs.join("system/two.md"), &memfs_memo("second memory"));

    let imported = sync(&memfs, &claude, Direction::Import, false).unwrap();
    assert_eq!(imported.changes.len(), 2);

    // Both directions now report nothing to do, in either order, repeatedly.
    for _ in 0..2 {
        assert!(
            sync(&memfs, &claude, Direction::Import, false)
                .unwrap()
                .is_empty(),
            "import did not settle"
        );
        assert!(
            sync(&memfs, &claude, Direction::Export, false)
                .unwrap()
                .is_empty(),
            "export did not settle"
        );
    }
}

/// S4: the Claude Code index is rebuilt from what exists, so a stale entry
/// cannot survive a sync.
#[test]
fn s4_a_stale_index_entry_does_not_survive() {
    let (_root, memfs, claude) = dirs();
    write(&memfs.join("system/kept.md"), &memfs_memo("a kept memory"));
    write(
        &claude.join("MEMORY.md"),
        "- [Deleted](deleted.md) — points at a file that is gone\n",
    );

    sync(&memfs, &claude, Direction::Import, false).unwrap();

    let index = std::fs::read_to_string(claude.join("MEMORY.md")).unwrap();
    assert!(index.contains("kept.md"), "{index}");
    assert!(
        !index.contains("deleted.md"),
        "the index is derived, not merged: {index}"
    );
}

/// S4: export writes no index, because MemFS treats the directory as the
/// index. Writing one would add a file Letta does not expect.
#[test]
fn s4_export_writes_no_memfs_index() {
    let (_root, memfs, claude) = dirs();
    write(
        &claude.join("fact.md"),
        &claude_memo("fact", "d", "feedback"),
    );

    sync(&memfs, &claude, Direction::Export, false).unwrap();

    assert!(memfs.join("system/fact.md").exists());
    assert!(
        !memfs.join("system/MEMORY.md").exists(),
        "a plain export must not write a MemFS index"
    );
}

// --------------------------------------------------------------- seeding (I)

/// I1: seeded facts go to the child directory, never the root, because the
/// root is the agent's permanent prompt.
#[test]
fn i1_seeding_keeps_the_root_lean() {
    let (_root, memfs, claude) = dirs();
    for name in ["one", "two", "three"] {
        write(
            &claude.join(format!("{name}.md")),
            &claude_memo(name, "d", "feedback"),
        );
    }

    let plan = export_indexed(&memfs, &claude, "project", &Filter::default(), false).unwrap();

    assert_eq!(plan.changes.len(), 3);
    for name in ["one", "two", "three"] {
        assert!(memfs.join(format!("project/{name}.md")).exists());
        assert!(
            !memfs.join(format!("{name}.md")).exists(),
            "{name} must not reach the root"
        );
    }
}

/// I2: a child directory counts as memory only with its own frontmatter-free
/// index.
#[test]
fn i2_a_child_directory_gets_a_frontmatter_free_index() {
    let (_root, memfs, claude) = dirs();
    write(
        &claude.join("one.md"),
        &claude_memo("one", "the first hook — detail", "feedback"),
    );

    export_indexed(&memfs, &claude, "project", &Filter::default(), false).unwrap();

    let index = std::fs::read_to_string(memfs.join("project/MEMORY.md")).unwrap();
    assert!(index.starts_with("# project\n"), "{index}");
    assert!(
        !index.contains("---"),
        "a child index must carry no frontmatter: {index}"
    );
    assert!(index.contains("one.md"), "{index}");
    assert!(
        index.contains("the first hook"),
        "the index entry carries the description's hook: {index}"
    );
}

/// I2: the index describes every file in the directory, not just the ones the
/// current run wrote.
///
/// The earlier memory is removed from the Claude side before the second run,
/// so it is absent from that run's source entirely. It is still a file in the
/// child directory, so the index must still list it — otherwise a second
/// seeding run makes previously seeded facts undiscoverable.
#[test]
fn i2_the_child_index_covers_files_from_earlier_runs() {
    let (_root, memfs, claude) = dirs();
    write(
        &claude.join("first.md"),
        &claude_memo("first", "d", "feedback"),
    );
    export_indexed(&memfs, &claude, "project", &Filter::default(), false).unwrap();
    std::fs::remove_file(claude.join("first.md")).unwrap();

    write(
        &claude.join("second.md"),
        &claude_memo("second", "d", "feedback"),
    );
    export_indexed(&memfs, &claude, "project", &Filter::default(), false).unwrap();

    let index = std::fs::read_to_string(memfs.join("project/MEMORY.md")).unwrap();
    assert!(
        index.contains("first.md"),
        "an earlier run's file must stay indexed: {index}"
    );
    assert!(index.contains("second.md"), "{index}");
}

/// I3: a subdirectory name can neither escape the memory root nor hide from
/// the index.
#[test]
fn i3_a_subdirectory_name_cannot_escape_or_hide() {
    let (_root, memfs, claude) = dirs();
    write(
        &claude.join("fact.md"),
        &claude_memo("fact", "d", "feedback"),
    );

    for bad in ["", "../escape", "nested/dir", ".hidden", "."] {
        let error = export_indexed(&memfs, &claude, bad, &Filter::default(), false)
            .expect_err("{bad} must be rejected");
        assert!(
            error.contains("invalid memory subdirectory"),
            "{bad:?} gave the wrong error: {error}"
        );
    }
}

/// I5: only declared types in the list are seeded.
#[test]
fn i5_only_types_seeds_selectively() {
    let (_root, memfs, claude) = dirs();
    for (name, kind) in [
        ("guidance", "feedback"),
        ("pointer", "reference"),
        ("status", "project"),
    ] {
        write(
            &claude.join(format!("{name}.md")),
            &claude_memo(name, "d", kind),
        );
    }

    let filter = Filter {
        types: vec!["feedback".to_owned(), "reference".to_owned()],
    };
    let plan = sync_filtered(&memfs, &claude, Direction::Export, false, &filter).unwrap();

    let mut names: Vec<&str> = plan.changes.iter().map(|c| c.name()).collect();
    names.sort_unstable();
    assert_eq!(names, ["guidance", "pointer"]);
    assert!(
        !memfs.join("system/status.md").exists(),
        "an excluded type must not reach MemFS at all"
    );
}

/// I5: an empty filter keeps everything, so plain export is unfiltered.
#[test]
fn i5_an_empty_filter_keeps_everything() {
    let filter = Filter::default();
    let typed = parse_memo(
        "a",
        "---\ndescription: d\nmetadata:\n  type: project\n---\n\nb\n",
    )
    .unwrap();
    let untyped = parse_memo("b", "---\ndescription: d\n---\n\nb\n").unwrap();

    assert!(filter.keeps(&typed));
    assert!(filter.keeps(&untyped));
}

/// I5 with I1: filtering and indexed seeding compose, which is the shape an
/// actual seeding run uses.
#[test]
fn i5_filtering_composes_with_indexed_seeding() {
    let (_root, memfs, claude) = dirs();
    write(
        &claude.join("keep.md"),
        &claude_memo("keep", "d", "feedback"),
    );
    write(
        &claude.join("drop.md"),
        &claude_memo("drop", "d", "project"),
    );

    let filter = Filter {
        types: vec!["feedback".to_owned()],
    };
    export_indexed(&memfs, &claude, "project", &filter, false).unwrap();

    assert!(memfs.join("project/keep.md").exists());
    assert!(!memfs.join("project/drop.md").exists());

    let index = std::fs::read_to_string(memfs.join("project/MEMORY.md")).unwrap();
    assert!(index.contains("keep.md"), "{index}");
    assert!(
        !index.contains("drop.md"),
        "the index must not advertise a file that was filtered out: {index}"
    );
}

// ------------------------------------------------------------ resolution (R)

/// R2: Claude Code's memory directory is the project path with separators
/// replaced by dashes.
#[test]
fn r2_the_claude_directory_is_derived_from_the_project_path() {
    let derived =
        herdr_integration::memfs::resolve_claude_dir(None, Path::new("/home/u/herdr-infinity"))
            .unwrap();
    let shown = derived.to_string_lossy();

    assert!(shown.contains("-home-u-herdr-infinity"), "{shown}");
    assert!(shown.ends_with("memory"), "{shown}");
}

/// R1/R2: an explicit directory always wins over the derived or environment
/// default.
#[test]
fn r1_an_explicit_directory_wins() {
    let explicit = PathBuf::from("/tmp/explicit-memory");

    let memfs = herdr_integration::memfs::resolve_memfs_dir(Some(explicit.clone())).unwrap();
    assert_eq!(memfs, explicit);

    let claude =
        herdr_integration::memfs::resolve_claude_dir(Some(explicit.clone()), Path::new("/ignored"))
            .unwrap();
    assert_eq!(claude, explicit);
}

/// A guard on the helpers above: `claude_memo` and `memfs_memo` must produce
/// what the parser accepts, or every test here would pass vacuously.
#[test]
fn the_fixtures_parse_as_memories() {
    let claude = parse_memo("x", &claude_memo("a-name", "a description", "feedback")).unwrap();
    assert_eq!(claude.name, "a-name");
    assert_eq!(claude.kind, "feedback");
    assert!(claude.kind_declared);

    let memfs = parse_memo("stem", &memfs_memo("a description")).unwrap();
    assert_eq!(memfs.name, "stem");
    assert_eq!(memfs.description, "a description");

    let _: BTreeMap<String, Memo> = BTreeMap::new();
}
