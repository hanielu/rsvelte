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
    project_with(source, true)
}

/// A JavaScript component is projected as the language server does, with JSDoc.
fn project_with(source: &str, is_ts_file: bool) -> ProjectionArtifact {
    ProjectionEngine::new()
        .project(
            source,
            Svelte2TsxOptions {
                filename: "Card.svelte".to_string(),
                is_ts_file,
                emit_jsdoc: !is_ts_file,
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

#[test]
fn a_snippets_name_and_parameters_keep_their_place() {
    let source = "<script lang=\"ts\">\n\timport Child from \"./Child.svelte\";\n\ttype Size = \"sm\" | \"lg\";\n\ttype Row = { id: string; size: Size };\n\tlet rows: Row[] = $state([]);\n</script>\n{#snippet cell(x: Size, r: Row)}<td>{r.id}{x}</td>{/snippet}\n{#snippet empty()}{/snippet}\n{#snippet generic<T extends Size>(v: T)}{v}{/snippet}\n{#snippet commented(/* c */ a: number = 1)}{a}{/snippet}\n{#snippet  spaced ( s : Size ) }{s}{/snippet}\n{#snippet noparams()}<b>x</b>{/snippet}\n{#snippet onlyempty(e: Size)}{/snippet}\n{@render cell(\"sm\", rows[0])}\n<Child>{#snippet named(q: Size)}{q}{/snippet}</Child>\n<div>{#snippet inner(y: Size)}{y}{/snippet}{@render inner(\"lg\")}</div>\n";
    let artifact = project(source);
    assert_code_contains(
        &artifact,
        &[
            " const cell/*\u{03A9}ignore_position\u{03A9}*/ = (x: Size, r: Row)/*\u{03A9}ignore_start\u{03A9}*/: ReturnType<import('svelte').Snippet>/*\u{03A9}ignore_end\u{03A9}*/ => { async ()/*\u{03A9}ignore_position\u{03A9}*/ => { { svelteHTML.createElement(\"td\", {});r.id;x; }};return __sveltets_2_any(0)};  const empty/*\u{03A9}ignore_position\u{03A9}*/ = ()/*\u{03A9}ignore_start\u{03A9}*/: ReturnType<import('svelte').Snippet>/*\u{03A9}ignore_end\u{03A9}*/ => { async ()/*\u{03A9}ignore_position\u{03A9}*/ => {};return __sveltets_2_any(0)};  const generic/*\u{03A9}ignore_position\u{03A9}*/ = <T extends Size>(v: T)/*\u{03A9}ignore_start\u{03A9}*/",
            " const commented/*\u{03A9}ignore_position\u{03A9}*/ = (/* c */ a: number = 1)/*\u{03A9}ignore_start\u{03A9}*/",
            "   const spaced/*\u{03A9}ignore_position\u{03A9}*/ = (s : Size)/*\u{03A9}ignore_start\u{03A9}*/",
            " const onlyempty/*\u{03A9}ignore_position\u{03A9}*/ = (e: Size)/*\u{03A9}ignore_start\u{03A9}*/: ReturnType<import('svelte').Snippet>/*\u{03A9}ignore_end\u{03A9}*/ => { async ()/*\u{03A9}ignore_position\u{03A9}*/ => {};return __sveltets_2_any(0)};\n",
            "props: {named:(q: Size) => { async ()/*\u{03A9}ignore_position\u{03A9}*/ => {q;};return __sveltets_2_any(0)},}});",
            "{ svelteHTML.createElement(\"div\", {}); const inner/*\u{03A9}ignore_position\u{03A9}*/ = (y: Size)/*\u{03A9}ignore_start\u{03A9}*/",
        ],
    );
    for name in [
        "cell(",
        "empty(",
        "generic<",
        "commented(",
        " spaced ",
        "noparams(",
        "onlyempty(",
        "named(",
        "inner(",
    ] {
        let at = nth_offset(source, &format!("{{#snippet {name}"), 0, 10);
        let at = at + source[at..].find(|c: char| c != ' ').unwrap();
        let len = source[at..].find(|c: char| !c.is_alphanumeric()).unwrap();
        assert_maps_in_place(&artifact, source, at, len);
    }
    for params in [
        "x: Size, r: Row",
        "v: T",
        "/* c */ a: number = 1",
        "s : Size",
        "e: Size",
        "q: Size",
        "y: Size",
    ] {
        assert_maps_in_place(
            &artifact,
            source,
            nth_offset(source, params, 0, 0),
            params.len(),
        );
    }
}

#[test]
fn a_javascript_snippets_name_and_parameters_keep_their_place() {
    let source = "<script>\n\tlet n = 1;\n</script>\n{#snippet cell(x)}<td>{x}</td>{/snippet}\n{#snippet none()}{/snippet}\n{@render cell(n)}\n";
    let artifact = project_with(source, false);
    assert_code_contains(
        &artifact,
        &[
            " const cell/*\u{03A9}ignore_position\u{03A9}*/ = /** @returns {ReturnType<import('svelte').Snippet>} */ (x) => { async ()/*\u{03A9}ignore_position\u{03A9}*/ => { { svelteHTML.createElement(\"td\", {});x; }};return __sveltets_2_any(0)};  const none/*\u{03A9}ignore_position\u{03A9}*/ = /** @returns {ReturnType<import('svelte').Snippet>} */ () => { async ()/*\u{03A9}ignore_position\u{03A9}*/ => {};return __sveltets_2_any(0)};\n",
        ],
    );
    assert_maps_in_place(&artifact, source, nth_offset(source, "cell(x)", 0, 0), 4);
    assert_maps_in_place(&artifact, source, nth_offset(source, "cell(x)", 0, 5), 1);
    assert_maps_in_place(&artifact, source, nth_offset(source, "none()", 0, 0), 4);
}

#[test]
fn an_each_blocks_key_keeps_its_place() {
    let source = "<script lang=\"ts\">\n\ttype Row = { id: string };\n\tlet rows: Row[] = $state([]);\n\tlet items = $state([1, 2]);\n</script>\n{#each rows as row (row.id)}<p>{row.id}</p>{/each}\n{#each rows as { id }, i (id)}<p>{id}{i}</p>{/each}\n{#each items as items (items)}{items}{/each}\n{#each rows as row, i (row.id)}<p>{i}</p>{:else}<p>none</p>{/each}\n{#each rows as row ( row.id )}{row.id}{/each}\n{#each rows as row (row.id)}{/each}\n{#each rows, 2 as row (row)}{row}{/each}\n";
    let artifact = project(source);
    assert_code_contains(
        &artifact,
        &[
            "   for(let row of __sveltets_2_ensureArray(rows)){row.id; { svelteHTML.createElement(\"p\", {});row.id; }}\n",
            "    for(let { id } of __sveltets_2_ensureArray(rows)){let i = 1;id; { svelteHTML.createElement(\"p\", {});id;i; }}\n",
            "   { const $$_each = __sveltets_2_ensureArray(items); for(let items of $$_each){items;items;}}\n",
            "    for(let row of __sveltets_2_ensureArray(rows)){let i = 1;row.id; { svelteHTML.createElement(\"p\", {});i; }} { svelteHTML.createElement(\"p\", {});  }\n",
            "   for(let row of __sveltets_2_ensureArray(rows)){row.id;row.id;}\n",
            "   for(let row of __sveltets_2_ensureArray(rows)){row.id;}\n",
            "   for(let row of __sveltets_2_ensureArray((rows, 2))){row;row;}\n",
        ],
    );
    for (key, nth) in [
        ("(row.id)}<p>", 0),
        ("(id)}", 0),
        ("(items)}", 0),
        ("(row.id)}<p>", 1),
        ("( row.id )", 0),
        ("(row.id)}{/each}", 0),
        ("(row)}", 0),
    ] {
        let at = nth_offset(source, key, nth, 1);
        let at = at + source[at..].find(|c: char| c != ' ').unwrap();
        let len = source[at..].find([' ', ')']).unwrap();
        assert_maps_in_place(&artifact, source, at, len);
    }
}

#[test]
fn a_class_or_style_shorthand_keeps_its_name_in_place() {
    let source = "<script lang=\"ts\">\n\tlet active = $state(false);\n\tlet color = $state(\"red\");\n</script>\n<p class:active>y</p>\n<p style:color>x</p>\n<p style:color|important>z</p>\n<svelte:element this={\"div\"} class:active style:color>v</svelte:element>\n";
    let artifact = project(source);
    assert_code_contains(
        &artifact,
        &[
            " { svelteHTML.createElement(\"p\", { });active;  }\n",
            " { svelteHTML.createElement(\"p\", { });__sveltets_2_ensureType(String, Number, color);  }\n { svelteHTML.createElement(\"p\", { });__sveltets_2_ensureType(String, Number, color);  }\n",
            "active;__sveltets_2_ensureType(String, Number, color);",
        ],
    );
    for (name, nth) in [
        ("active>", 0),
        ("color>", 0),
        ("color|", 0),
        ("active style", 0),
        ("color>v", 0),
    ] {
        let at = nth_offset(source, name, nth, 0);
        let len = source[at..].find(|c: char| !c.is_alphanumeric()).unwrap();
        assert_maps_in_place(&artifact, source, at, len);
    }
}
