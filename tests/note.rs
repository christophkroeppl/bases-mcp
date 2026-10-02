//! Note segmentation.
//!
//! Two things are pinned here. The first is that a ` ```base-rendered ` fence
//! is a Base REGION and not prose: it has to be matchable back to the region it
//! replaced, on the Base path in its info string, because that fence is what an
//! agent gets back from a Projection and must never reach disk.
//!
//! The second is the invariant the whole write path rests on: a note's segments
//! concatenate back to the note itself, byte for byte, and every segment's span
//! indexes its own text. That is asserted over the real testing vault rather
//! than over examples, because an offset bug only shows up on notes with
//! umlauts, CRLF line endings and embeds at the end of the file.

use std::fs;
use std::path::{Path, PathBuf};

#[allow(unused_imports)]
use bases_mcp::note::is_base_embed;
use bases_mcp::note::{
    extract_inline_tags, extract_links, extract_tasks, fence_attrs, is_base_region, parse_note,
    parse_note_with_embeds, serialise, BaseFence, ParsedNote, Segment, SegmentKind, TaskItem,
    RENDER_FENCE_LANG,
};
use bases_mcp::value::BasesValue;

/// A host note with one embedded base, as `test/vault` has.
const HOST: &str = "---\nstatus: active\n---\n\n## Tickets\n\n![[Tickets.base]]\n";

/// What `get_note` returns for `HOST`: the embed rendered into a fence, with the
/// provenance that ties it back to the region it replaced.
const PROJECTION: &str = concat!(
    "---\n",
    "status: active\n",
    "---\n",
    "\n",
    "## Tickets\n",
    "\n",
    "```base-rendered path=\"Tickets.base\"\n",
    "| file name |\n",
    "| --- |\n",
    "| Fix login redirect |\n",
    "```\n",
    "\n",
);

/// The Base regions of a note, in document order.
fn base_regions(note: &ParsedNote) -> Vec<&Segment> {
    note.segments.iter().filter(|s| is_base_region(s)).collect()
}

fn kinds(note: &ParsedNote) -> Vec<SegmentKind> {
    note.segments.iter().map(Segment::kind).collect()
}

#[test]
fn a_rendered_fence_is_a_base_region_not_prose() {
    let note = parse_note_with_embeds("Host.md", PROJECTION);
    assert_eq!(
        kinds(&note),
        [
            SegmentKind::Frontmatter,
            SegmentKind::Prose,
            SegmentKind::BaseFence,
            SegmentKind::Prose
        ]
    );
    let rendered = serialise(
        &note
            .segments
            .iter()
            .filter(|s| s.kind() == SegmentKind::Prose)
            .cloned()
            .collect::<Vec<_>>(),
    );
    assert!(
        !rendered.contains("base-rendered"),
        "the fence leaked into prose: {rendered}"
    );
}

#[test]
fn the_fence_keeps_the_base_path_and_view_from_its_info_string() {
    let note = parse_note_with_embeds("Host.md", PROJECTION);
    let fence = base_regions(&note)
        .pop()
        .expect("the projection has one region")
        .clone();

    assert_eq!(fence.base_path(), Some("Tickets.base"));
    assert_eq!(fence.view_name(), None, "no view was pinned");
    assert!(fence.is_rendered());
    assert!(fence.end() > fence.start());
}

#[test]
fn a_rendered_fence_carries_no_yaml_so_it_is_never_mistaken_for_live() {
    let note = parse_note_with_embeds("Host.md", PROJECTION);
    let fence = note
        .segments
        .iter()
        .find_map(Segment::base_fence)
        .expect("a fence");

    assert_eq!(fence.yaml(), None);
    assert!(matches!(fence, BaseFence::Rendered { .. }));
}

#[test]
fn every_provenance_attribute_survives_the_round_trip_through_the_info_string() {
    let text = concat!(
        "```base-rendered path=\"Tickets.base\" view=\"All\" context=\"Host.md\" rows=\"3\"\n",
        "| x |\n",
        "```\n",
    );
    let note = parse_note_with_embeds("Host.md", text);
    let fence = note
        .segments
        .iter()
        .find_map(Segment::base_fence)
        .expect("a fence");

    assert_eq!(fence.base_path(), Some("Tickets.base"));
    assert_eq!(fence.view_name(), Some("All"));
    assert_eq!(
        serialise(&note.segments),
        text,
        "the region must survive verbatim"
    );
}

#[test]
fn the_language_is_the_first_token_of_the_info_string() {
    // `path="Tickets.base"` is an attribute, not part of the language. Taking the
    // whole info string would make this fence match neither `base` nor
    // `base-rendered`, and the region would degrade to prose.
    let live = parse_note("A.md", "```base extra=\"x\"\nviews: []\n```\n");
    let fence = live
        .segments
        .iter()
        .find_map(Segment::base_fence)
        .expect("a live fence");
    assert_eq!(fence.yaml(), Some("views: []\n"));
    assert!(!fence.is_rendered());
    assert_eq!(fence.base_path(), None);

    // A language that merely starts with `base` is not a base fence.
    let lookalike = parse_note("A.md", "```base-renderedish\nrow\n```\n");
    assert!(
        base_regions(&lookalike).is_empty(),
        "`base-renderedish` is not a region"
    );
    assert_eq!(
        serialise(&lookalike.segments),
        "```base-renderedish\nrow\n```\n"
    );

    assert_eq!(
        fence_attrs(&format!(
            "{} path=\"T.base\" view=\"All\"",
            RENDER_FENCE_LANG
        )),
        [
            ("path".to_string(), "T.base".to_string()),
            ("view".to_string(), "All".to_string())
        ]
        .into_iter()
        .collect()
    );
}

#[test]
fn an_ordinary_code_fence_is_still_prose() {
    let text = "---\n---\n\n```ts\nconst x = 1;\n```\n";
    let note = parse_note_with_embeds("Host.md", text);

    assert!(
        base_regions(&note).is_empty(),
        "a `ts` fence is not a Base region"
    );
    assert!(!note
        .segments
        .iter()
        .any(|s| s.kind() == SegmentKind::BaseFence));
    assert_eq!(serialise(&note.segments), text);
}

#[test]
fn a_host_note_carries_one_embed_region() {
    let note = parse_note_with_embeds("Host.md", HOST);
    let regions = base_regions(&note);
    assert_eq!(regions.len(), 1);

    let embed = regions[0].base_embed().expect("an embed, not a fence");
    assert_eq!(embed.base_path, "Tickets.base");
    assert_eq!(embed.view_name, None);
    assert_eq!(regions[0].raw(), "![[Tickets.base]]");
    // The embed is its own region, so the prose around it is untouched.
    assert!(!serialise(&note.segments).contains("Tickets.base#"));

    // An embed is a link, so `file.embeds` finds it whichever way the note was
    // parsed.
    for parsed in [&note, &parse_note("Host.md", HOST)] {
        assert_eq!(parsed.embeds.len(), 1);
        assert_eq!(parsed.embeds[0].target, "Tickets.base");
        assert!(parsed.embeds[0].embedded);
    }
}

#[test]
fn an_embed_pins_a_view_and_a_display() {
    let note = parse_note_with_embeds("Host.md", "  ![[Tickets.base#All|the tickets]]\n");
    let embed = base_regions(&note)
        .pop()
        .expect("an embed")
        .base_embed()
        .expect("an embed")
        .clone();

    assert_eq!(embed.base_path, "Tickets.base");
    assert_eq!(embed.view_name.as_deref(), Some("All"));
    assert_eq!(embed.base_path.as_str(), "Tickets.base");
}

#[test]
fn an_embed_in_the_middle_of_a_sentence_is_prose() {
    let note = parse_note_with_embeds("Host.md", "see ![[Tickets.base]] here\n");
    assert_eq!(
        kinds(&note),
        [SegmentKind::Prose],
        "only a line of its own is a region"
    );
}

#[test]
fn frontmatter_becomes_properties_the_evaluator_can_read() {
    let note = parse_note(
        "A.md",
        "---\ntags: [a, b]\nproject:\n  owner: me\ndone: false\n---\n\nbody\n",
    );

    assert!(!note.malformed_frontmatter);
    assert_eq!(
        note.frontmatter.get("tags"),
        Some(&BasesValue::List(vec![
            BasesValue::String("a".into()),
            BasesValue::String("b".into())
        ]))
    );
    assert_eq!(note.frontmatter.get("done"), Some(&BasesValue::Bool(false)));
    assert_eq!(
        note.frontmatter.get("project"),
        Some(&BasesValue::Namespace(std::rc::Rc::new(
            [("owner".to_string(), BasesValue::String("me".into()))]
                .into_iter()
                .collect()
        )))
    );
    // The frontmatter segment shares one map with the note rather than copying it.
    let segment = note.segments.first().expect("a frontmatter segment");
    let shared = &segment.frontmatter().expect("frontmatter").data;
    assert!(std::rc::Rc::ptr_eq(shared, &note.frontmatter));
    assert_eq!(
        serialise(&note.segments),
        "---\ntags: [a, b]\nproject:\n  owner: me\ndone: false\n---\n\nbody\n"
    );
}

#[test]
fn malformed_frontmatter_degrades_to_no_properties_rather_than_failing_the_note() {
    let note = parse_note("A.md", "---\ntags: [unclosed\n---\n\n# Scratch\n");

    assert!(note.malformed_frontmatter);
    assert!(note.frontmatter.is_empty());
    assert_eq!(kinds(&note), [SegmentKind::Frontmatter, SegmentKind::Prose]);
    assert_eq!(
        serialise(&note.segments),
        "---\ntags: [unclosed\n---\n\n# Scratch\n"
    );
}

#[test]
fn frontmatter_must_be_the_first_thing_in_the_file() {
    // A `---` further down is a thematic break, not a property block.
    let note = parse_note("A.md", "intro\n\n---\na: 1\n---\n");
    assert_eq!(kinds(&note), [SegmentKind::Prose]);
    assert!(!note.malformed_frontmatter);
}

#[test]
fn an_unclosed_fence_runs_to_end_of_file_as_obsidian_does() {
    let note = parse_note("A.md", "---\na: 1\n---\n\n```base\nviews: []\n");
    let fence = note.segments.last().expect("a segment");
    assert_eq!(
        fence.end(),
        note.segments
            .iter()
            .map(Segment::end)
            .max()
            .expect("an end")
    );
    assert_eq!(fence.yaml(), Some("views: []\n"));
    assert_eq!(
        serialise(&note.segments),
        "---\na: 1\n---\n\n```base\nviews: []\n"
    );
}

#[test]
fn a_fence_that_is_not_at_a_line_start_is_prose() {
    let text = "text ```ts\nx\n```\n";
    let note = parse_note("A.md", text);

    assert!(
        base_regions(&note).is_empty(),
        "a fence only opens at a line start"
    );
    assert_eq!(serialise(&note.segments), text);
}

#[test]
fn a_closing_fence_must_repeat_the_opener_exactly() {
    let closing = |text: &str| parse_note("A.md", text).segments[0].end();
    // A blockquoted line closes the fence: Obsidian allows `> ` before the run.
    assert_eq!(closing("```ts\n> ```\nafter\n"), "```ts\n> ```".len());
    // Trailing spacing is part of the closing fence, a different number of
    // backticks is not a closing fence at all, so that fence runs to the end.
    assert_eq!(
        closing("```ts\nx\n```   \nafter\n"),
        "```ts\nx\n```   ".len()
    );
    assert_eq!(
        closing("```ts\nx\n````\nafter\n"),
        "```ts\nx\n````\nafter\n".len()
    );
    assert_eq!(
        closing("````ts\nx\n```\nafter\n"),
        "````ts\nx\n```\nafter\n".len()
    );
}

/// A line that LOOKS like a fence opening but cannot be one.
///
/// The TypeScript scanned for ` ``` ` at a line start, then handed the position
/// to a matcher that rejected it -- and re-found the same line forever, hanging
/// the process. Requiring the scan to advance is the fix, and it is why this
/// test exists at all.
#[test]
fn an_indented_fence_is_prose_rather_than_an_infinite_loop() {
    let text = "  ```base\n  views: []\n  ```\n";
    let note = parse_note("A.md", text);
    assert_eq!(serialise(&note.segments), text);
    assert!(base_regions(&note).is_empty());
}

#[test]
fn links_are_extracted_in_document_order_with_byte_offsets() {
    let text = "[[A]] [[B#sub|C]] ![[Tickets.base]] [D](Target.md) [E](https://x.y) [F](#anchor)\n";
    let links = extract_links(text);
    let targets: Vec<(&str, Option<&str>, Option<&str>, bool)> = links
        .iter()
        .map(|l| {
            (
                l.target.as_str(),
                l.subpath.as_deref(),
                l.display.as_deref(),
                l.embedded,
            )
        })
        .collect();

    assert_eq!(
        targets,
        [
            ("A", None, None, false),
            ("B", Some("sub"), Some("C"), false),
            ("Tickets.base", None, None, true),
            ("Target", None, Some("D"), false),
        ]
    );
    // A span covers the link syntax, not just the target, and offsets are bytes.
    for link in &links {
        let source = &text[link.start..link.end];
        assert!(
            source.starts_with('[') || source.starts_with('!'),
            "a span starts at the link: {source}"
        );
        assert!(
            source.ends_with("]]") || source.ends_with(')'),
            "a span ends at the link: {source}"
        );
    }
}

#[test]
fn tasks_come_from_the_body_and_not_from_the_properties() {
    let note = parse_note(
        "A.md",
        "---\ntasks: [x]\n---\n\n- [ ] one\n* [x] two\n+ [-] three\n",
    );
    let tasks: Vec<TaskItem> = note.tasks.clone();

    assert_eq!(
        tasks,
        [
            TaskItem {
                checked: false,
                text: "one".into(),
                line: 1
            },
            TaskItem {
                checked: true,
                text: "two".into(),
                line: 2
            },
            TaskItem {
                checked: false,
                text: "three".into(),
                line: 3
            },
        ]
    );
    assert_eq!(extract_tasks("- [x] only\n").len(), 1);
}

#[test]
fn inline_tags_skip_code_and_look_like_tag_fragments() {
    assert_eq!(
        extract_inline_tags("a #real and `#FFF` and #tag/child and ##not and #2024\n"),
        ["real", "tag/child"]
    );
    // Repeated tags are reported once.
    assert_eq!(extract_inline_tags("#a #a #b\n"), ["a", "b"]);
}

#[test]
fn byte_offsets_survive_text_that_is_not_ascii() {
    let text = "---\ndescription: ä\n---\n\n![[Tickets.base]]\n";
    let note = parse_note_with_embeds("A.md", text);
    let embed = base_regions(&note).pop().expect("an embed");

    assert_eq!(embed.raw(), "![[Tickets.base]]");
    assert_eq!(&text[embed.start()..embed.end()], embed.raw());
    assert!(
        embed.start() > "ä".len(),
        "the span is a byte offset, not a character one"
    );
}

// ---------------------------------------------------------------------------
// The round-trip invariant, over the real testing vault
// ---------------------------------------------------------------------------

/// Every `.md` file in the testing vault, read-only.
fn vault_notes() -> Vec<(String, String)> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let mut entries: Vec<PathBuf> = fs::read_dir(dir)
            .expect("the testing vault is readable")
            .map(|entry| entry.expect("a directory entry").path())
            .collect();
        entries.sort();
        for entry in entries {
            if entry.is_dir() {
                walk(&entry, out);
            } else if entry.extension().is_some_and(|ext| ext == "md") {
                out.push(entry);
            }
        }
    }

    let mut files = Vec::new();
    walk(Path::new("test/vault"), &mut files);
    assert!(
        files.len() >= 7,
        "expected the testing vault's notes, found {}",
        files.len()
    );
    files
        .into_iter()
        .map(|path| {
            let text = fs::read_to_string(&path).expect("a note is valid UTF-8");
            (path.to_string_lossy().replace('\\', "/"), text)
        })
        .collect()
}

/// The invariant, asserted byte for byte over every note in the testing vault.
#[test]
fn segments_concatenate_back_to_the_note_exactly() {
    for (path, text) in vault_notes() {
        for note in [
            parse_note(&path, &text),
            parse_note_with_embeds(&path, &text),
        ] {
            // The whole note, byte for byte.
            assert_eq!(serialise(&note.segments), text, "{path} did not round-trip");

            // Every segment spans its own text, and the spans tile the note with
            // no gap and no overlap: start at 0, end at the length, each start
            // where the previous one ended.
            let mut cursor = 0;
            for segment in &note.segments {
                assert_eq!(
                    segment.start(),
                    cursor,
                    "{path}: a gap or overlap at {cursor}"
                );
                assert_eq!(
                    &text[segment.start()..segment.end()],
                    segment.raw(),
                    "{path}: span and text disagree"
                );
                cursor = segment.end();
            }
            assert_eq!(cursor, text.len(), "{path}: the last segment stops short");
        }
    }
}

/// The Base regions the testing vault is built around: three `.base` embeds and
/// one inline fence. If this drifts, the parity oracle stops being an oracle.
#[test]
fn the_testing_vault_has_the_base_regions_it_is_a_fixture_for() {
    let mut regions: Vec<(String, String)> = Vec::new();
    for (path, text) in vault_notes() {
        let note = parse_note_with_embeds(&path, &text);
        for region in base_regions(&note) {
            regions.push((path.clone(), region.raw().to_string()));
        }
    }

    let embeds = regions
        .iter()
        .filter(|(_, raw)| raw.contains(".base"))
        .count();
    let fences = regions
        .iter()
        .filter(|(_, raw)| raw.starts_with("```base\n"))
        .count();
    assert_eq!(
        (embeds, fences),
        (3, 1),
        "the vault's regions moved: {regions:?}"
    );
}

/// A CRLF note's Base region must still be a Base region, and must span the same
/// bytes the TypeScript tree's did.
///
/// Regression. Rust's `(?m)` treats only `\n` as a line terminator, so `$` does
/// not match before a `\r`. That made every CRLF note's region invisible, and
/// `write_note` then persisted the agent's deletion of it while reporting
/// `health: ok` — silent vault corruption on a note the tool claimed to have
/// protected. JavaScript's multiline `$` does match before `\r`, so this was a
/// port regression rather than inherited behaviour.
///
/// Making the region VISIBLE was only half of it. Matching it needs a `\r?` the
/// pattern consumes, so the span arrived one byte longer than JavaScript's, and a
/// region that owns its `\r` is a region `push_line` then joins to `\r\r\n`. The
/// span is pinned here for the same reason the match is: byte for byte, and equal
/// to what the TypeScript produced.
#[test]
fn a_crlf_host_note_still_has_a_base_region() {
    for (label, text) in [
        ("plain", "# Host\r\n\r\n![[T.base]]\r\n"),
        ("indented", "# Host\r\n\r\n  ![[T.base]]  \r\n"),
        ("with view", "# Host\r\n\r\n![[T.base#View]]\r\n"),
        ("no trailing newline", "# Host\r\n\r\n![[T.base]]"),
    ] {
        let note = parse_note_with_embeds("Host.md", text);
        let regions: Vec<_> = note.segments.iter().filter(|s| is_base_region(s)).collect();
        assert_eq!(
            regions.len(),
            1,
            "{label}: expected exactly one Base region, found {}",
            regions.len()
        );
        // The region covers its own line and stops before the `\r`. JavaScript's
        // multiline `$` matches there, so the TypeScript tree's span stopped
        // there too and the `\r` belonged to the prose that followed. Rust's
        // `(?m)` `$` does not match before a `\r`, so `base_embed_re` has to
        // consume it to reach `$`; `split_base_embeds` trims the match back to
        // this span rather than letting the region absorb a line ending it does
        // not own — an absorbed `\r` plus the `\r\n` `push_line` appends is
        // `\r\r\n`, which the next read cannot match at all.
        assert_eq!(
            &text[regions[0].start()..regions[0].end()],
            expected_region(label),
            "{label}: the region must span the line and nothing else"
        );
        assert_eq!(
            regions[0].base_path(),
            Some("T.base"),
            "{label}: the region must name its Base"
        );
    }
}

/// The byte span a Base region must occupy for each CRLF shape below.
///
/// Byte-identical to what the TypeScript tree produced, which is the point: the
/// two implementations are meant to agree on every span, and a region that owns
/// its `\r` agrees with neither JavaScript nor `push_line`.
fn expected_region(label: &str) -> &'static str {
    match label {
        "indented" => "  ![[T.base]]  ",
        "with view" => "![[T.base#View]]",
        "no trailing newline" => "![[T.base]]",
        _ => "![[T.base]]",
    }
}

/// A CRLF note must round-trip byte for byte, like every other note.
#[test]
fn a_crlf_note_round_trips_exactly() {
    for (label, text) in [
        (
            "a Base region on a terminated line",
            "# Host\r\n\r\nintro\r\n\r\n![[T.base]]\r\n\r\ntail\r\n",
        ),
        (
            "a Base region at end of file",
            "# Host\r\n\r\nintro\r\n\r\n![[T.base]]",
        ),
        (
            "a Base region pinning a view",
            "# Host\r\n\r\n![[T.base#View]]\r\n",
        ),
        (
            "an indented Base region with trailing spaces",
            "# Host\r\n\r\n  ![[T.base]]  \r\n",
        ),
        (
            "an inline fence",
            "# Host\r\n\r\n```base\r\nviews: []\r\n```\r\n",
        ),
        (
            "the same note with LF",
            "# Host\n\nintro\n\n![[T.base]]\n\ntail\n",
        ),
        (
            "a mixed note, left exactly as found",
            "# Host\r\n\nintro\n\n![[T.base]]\n",
        ),
    ] {
        let note = parse_note_with_embeds("Host.md", text);
        assert_eq!(serialise(&note.segments), text, "{label}: lost bytes");
    }
}
