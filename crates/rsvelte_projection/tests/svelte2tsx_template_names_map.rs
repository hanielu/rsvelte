//! Author text that svelte2tsx keeps in place in the template's code, so that it
//! maps back to itself character by character: a rename, a reference, a hover
//! or a diagnostic lands on the author's text. The port wrote these pieces as
//! part of one generated string over their tag or block, which mapped them to
//! that string's start (or not at all): renaming a component left its closing
//! tags behind (or edited `</Chi`), and references reported zero-width
//! locations at the start of the block. The code itself is unchanged.

use rsvelte_projection::{
    ByteRange, ProjectionArtifact, ProjectionEngine, svelte2tsx::Svelte2TsxOptions,
};

fn project(source: &str) -> ProjectionArtifact {
    ProjectionEngine::new()
        .project(
            source,
            Svelte2TsxOptions {
                filename: "Card.svelte".to_string(),
                is_ts_file: true,
                ..Default::default()
            },
        )
        .expect("project component")
}

fn line_column(text: &str, offset: usize) -> (u32, u32) {
    let head = &text[..offset];
    let line_start = head.rfind('\n').map_or(0, |index| index + 1);
    (
        head.matches('\n').count() as u32,
        head[line_start..]
            .chars()
            .map(char::len_utf16)
            .sum::<usize>() as u32,
    )
}

/// Byte offset of the `nth` occurrence of `needle` in `text`, plus `skip`.
fn nth_offset(text: &str, needle: &str, nth: usize, skip: usize) -> usize {
    text.match_indices(needle)
        .nth(nth)
        .unwrap_or_else(|| panic!("no occurrence {nth} of {needle:?}"))
        .0
        + skip
}

/// The `len` bytes at `original` map exactly (both ways) to one generated range
/// holding the same text, and the source map agrees character by character.
fn assert_maps_in_place(artifact: &ProjectionArtifact, source: &str, original: usize, len: usize) {
    let code = &artifact.code;
    let text = &source[original..original + len];
    let map = artifact.exact_mappings.as_ref().expect("exact mappings");
    let original_range = ByteRange::new(original as u32, (original + len) as u32).unwrap();
    let generated = map.source_range_to_generated(original_range);
    assert_eq!(
        generated.len(),
        1,
        "{text:?} at {:?} maps to {generated:?}:\n{code}",
        line_column(source, original)
    );
    let generated = generated[0];
    assert_eq!(
        &code[generated.start() as usize..generated.end() as usize],
        text
    );
    assert_eq!(
        map.generated_range_to_source(generated),
        Some(original_range)
    );

    let source_map =
        sourcemap::SourceMap::from_slice(artifact.source_map.as_deref().unwrap().as_bytes())
            .expect("source map");
    let (line, column) = line_column(code, generated.start() as usize);
    let (text_line, text_column) = line_column(source, original);
    for offset in 0..text.chars().count() as u32 {
        let token = source_map
            .lookup_token(line, column + offset)
            .expect("token");
        assert_eq!(
            (token.get_src_line(), token.get_src_col()),
            (text_line, text_column + offset),
            "column {offset} of {text:?}"
        );
    }
}

fn assert_code_contains(artifact: &ProjectionArtifact, fragments: &[&str]) {
    for fragment in fragments {
        assert!(
            artifact.code.contains(fragment),
            "no {fragment:?} in:\n{}",
            artifact.code
        );
    }
}

#[test]
fn a_components_closing_tag_keeps_its_name_in_place() {
    let source = "<script lang=\"ts\">\n\timport Child from \"./Child.svelte\";\n\timport * as UI from \"./ui\";\n\tlet n = $state(1);\n</script>\n<Child kind=\"a\">text</Child>\n<UI.Box>{n}</UI.Box >\n<Child kind=\"b\"><p>{n}</p>\n</Child>\n";
    let artifact = project(source);
    assert_code_contains(
        &artifact,
        &[
            "\"kind\":`a`,}});  Child}",
            "props: {children:() => { return __sveltets_2_any(0); },}});n; UI.Box}",
            "{ svelteHTML.createElement(\"p\", {});n; }\n Child}",
        ],
    );
    assert_maps_in_place(&artifact, source, nth_offset(source, "</Child>", 0, 2), 5);
    assert_maps_in_place(&artifact, source, nth_offset(source, "</UI.Box", 0, 2), 6);
    assert_maps_in_place(&artifact, source, nth_offset(source, "</Child>", 1, 2), 5);
}

#[test]
fn a_named_slot_components_closing_tag_keeps_its_name_in_place() {
    // `<Inner slot="a">` closes its own block, then the named-slot block, with
    // its name between the two.
    let source = "<script lang=\"ts\">\n\timport Outer from \"./Outer.svelte\";\n\timport Inner from \"./Inner.svelte\";\n</script>\n<Outer><Inner slot=\"a\" let:x>{x}</Inner></Outer>\n<Outer><Inner slot=\"b\">t</Inner>\n</Outer>\n";
    let artifact = project(source);
    assert_code_contains(&artifact, &["x; }Inner} Outer}", "  }Inner}\n Outer}"]);
    for nth in 0..2 {
        assert_maps_in_place(&artifact, source, nth_offset(source, "</Inner>", nth, 2), 5);
        assert_maps_in_place(&artifact, source, nth_offset(source, "</Outer>", nth, 2), 5);
    }
}
