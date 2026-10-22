//! The `$$ComponentProps` alias repeats the author's `$props()` annotation.
//! Upstream moves the annotation into the alias, so the alias maps back to it;
//! here the alias text was generated, so it mapped to the `<script` tag the
//! header replaces (0:1): renaming a type the annotation uses edited the tag
//! (`<script` became `<Dimpt`) and left the annotation unchanged. The alias is
//! now a mapped copy of the annotation, wherever it is placed, and the code
//! stays what it was.

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

/// The alias in `code` holds `annotation` and maps back to it exactly: through
/// the exact mappings, through the source map one character at a time, and
/// with the alias's own `;type $$ComponentProps =  ` on its start.
fn assert_alias_maps_to_the_annotation(source: &str, annotation: &str, alias_line: &str) {
    let artifact = project(source);
    let code = &artifact.code;
    assert!(
        code.lines().any(|line| line.ends_with(alias_line)),
        "no line ending in {alias_line:?}:\n{code}"
    );
    let head = ";type $$ComponentProps =  ";
    let generated = code.find(&format!("{head}{annotation};")).expect("alias") + head.len();
    let original = source
        .find(&format!(": {annotation} = $props()"))
        .expect("annotation")
        + 2;

    let map = artifact.exact_mappings.as_ref().expect("exact mappings");
    let generated_range =
        ByteRange::new(generated as u32, (generated + annotation.len()) as u32).unwrap();
    let original_range =
        ByteRange::new(original as u32, (original + annotation.len()) as u32).unwrap();
    assert_eq!(
        map.generated_range_to_source(generated_range),
        Some(original_range)
    );
    assert_eq!(
        map.source_range_to_generated(original_range),
        vec![generated_range]
    );

    let source_map =
        sourcemap::SourceMap::from_slice(artifact.source_map.as_deref().unwrap().as_bytes())
            .expect("source map");
    let (alias_line_index, alias_column) = line_column(code, generated);
    let (annotation_line, annotation_column) = line_column(source, original);
    for column in 0..annotation.len() as u32 {
        let token = source_map
            .lookup_token(alias_line_index, alias_column + column)
            .expect("token");
        assert_eq!(
            (token.get_src_line(), token.get_src_col()),
            (annotation_line, annotation_column + column),
            "alias column {column} of {annotation:?}"
        );
    }
    let token = source_map
        .lookup_token(alias_line_index, alias_column - head.len() as u32 + 1)
        .expect("token");
    assert_eq!(
        (token.get_src_line(), token.get_src_col()),
        (annotation_line, annotation_column),
        "the alias's name maps to the annotation, not to the <script tag"
    );
}

#[test]
fn a_hoisted_alias_maps_back_to_the_annotation() {
    assert_alias_maps_to_the_annotation(
        "<script lang=\"ts\">\n\ttype Size = 'sm' | 'lg';\n\tlet s: Size = 'sm';\n\tlet { kind }: { kind: Size } = $props();\n</script>\n<p>{s}{kind}</p>\n",
        "{ kind: Size }",
        ";type $$ComponentProps =  { kind: Size };function $$render() {",
    );
}

#[test]
fn a_hoisted_alias_after_imports_and_hoisted_types_maps_back_to_the_annotation() {
    assert_alias_maps_to_the_annotation(
        "<script lang=\"ts\">\n\timport type { Kind } from './kind';\n\ttype Size = 'sm' | 'lg';\n\tlet { kind, size }: { kind: Kind; size: Size } = $props();\n</script>\n<p>{kind}{size}</p>\n",
        "{ kind: Kind; size: Size }",
        ";type $$ComponentProps =  { kind: Kind; size: Size };function $$render() {",
    );
}

#[test]
fn an_alias_before_module_hoisted_snippets_maps_back_to_the_annotation() {
    // Without imports the alias follows the `;` that replaces `<`; with them, the
    // last import.
    assert_alias_maps_to_the_annotation(
        "<script module lang=\"ts\">\n\texport { badge };\n</script>\n<script lang=\"ts\">\n\ttype Size = 'sm' | 'lg';\n\tlet { size }: { size: Size } = $props();\n</script>\n{#snippet badge()}<i>x</i>{/snippet}\n<p>{size}</p>\n",
        "{ size: Size }",
        ";type $$ComponentProps =  { size: Size };;",
    );
    assert_alias_maps_to_the_annotation(
        "<script module lang=\"ts\">\n\texport { badge };\n</script>\n<script lang=\"ts\">\n\timport type { Kind } from './kind';\n\ttype Size = 'sm' | 'lg';\n\tlet { kind, size }: { kind: Kind; size: Size } = $props();\n</script>\n{#snippet badge()}<i>x</i>{/snippet}\n<p>{kind}{size}</p>\n",
        "{ kind: Kind; size: Size }",
        ";type $$ComponentProps =  { kind: Kind; size: Size };;",
    );
}

#[test]
fn an_alias_kept_inside_render_maps_back_to_the_annotation() {
    // A generic keeps it in `$$render`, before the declaration; here the
    // declaration opens the script.
    assert_alias_maps_to_the_annotation(
        "<script lang=\"ts\" generics=\"T\">let { item }: { item: T } = $props();\n</script>\n<p>{String(item)}</p>\n",
        "{ item: T }",
        ";type $$ComponentProps =  { item: T };let { item }:/*\u{3a9}ignore_start\u{3a9}*/$$ComponentProps/*\u{3a9}ignore_end\u{3a9}*/ = $props();",
    );
    // `typeof` of an instance value keeps it there too, after that value.
    assert_alias_maps_to_the_annotation(
        "<script lang=\"ts\">\n\tconst sizes = ['sm', 'lg'] as const;\n\tlet { size }: { size: (typeof sizes)[number] } = $props();\n</script>\n<p>{size}</p>\n",
        "{ size: (typeof sizes)[number] }",
        "const sizes = ['sm', 'lg'] as const;;type $$ComponentProps =  { size: (typeof sizes)[number] };",
    );
}

#[test]
fn an_alias_of_a_non_object_annotation_maps_back_to_it() {
    assert_alias_maps_to_the_annotation(
        "<script lang=\"ts\">\n\timport type { HTMLButtonAttributes } from 'svelte/elements';\n\tlet { kind, ...rest }: HTMLButtonAttributes & { kind: 'a' | 'b' } = $props();\n</script>\n<button {...rest}>{kind}</button>\n",
        "HTMLButtonAttributes & { kind: 'a' | 'b' }",
        ";type $$ComponentProps =  HTMLButtonAttributes & { kind: 'a' | 'b' };function $$render() {",
    );
}

/// The alias of an annotation that script recovery repaired holds the repaired
/// text, `alias_line` ends a line of the code, and every byte the author wrote
/// in `annotation` maps exactly to the same byte in the alias, both ways,
/// except the bytes `repaired` marks with `^`, which recovery replaced.
fn assert_repaired_alias_maps_the_authors_bytes(
    source: &str,
    annotation: &str,
    repaired: &str,
    alias_line: &str,
) {
    assert_eq!(annotation.len(), repaired.len());
    let artifact = project(source);
    let code = &artifact.code;
    assert!(
        code.lines().any(|line| line.ends_with(alias_line)),
        "no line ending in {alias_line:?}:\n{code}"
    );
    let original = source
        .find(&format!(": {annotation} = $props()"))
        .expect("annotation")
        + 2;
    let map = artifact.exact_mappings.as_ref().expect("exact mappings");
    let alias = code.find(";type $$ComponentProps =  ").expect("alias");
    for (index, mark) in repaired.bytes().enumerate() {
        let at = (original + index) as u32;
        let generated = map.source_range_to_generated(ByteRange::new(at, at + 1).unwrap());
        if mark == b'^' {
            assert_eq!(
                generated,
                vec![],
                "byte {index} of {annotation:?} was repaired"
            );
            continue;
        }
        assert_eq!(generated.len(), 1, "byte {index} of {annotation:?}");
        let generated = generated[0];
        assert!(
            generated.start() as usize > alias,
            "byte {index} maps into the alias"
        );
        assert_eq!(
            code.as_bytes()[generated.start() as usize],
            annotation.as_bytes()[index]
        );
        assert_eq!(
            map.generated_to_source(generated.start()),
            Some(at),
            "byte {index} maps back"
        );
    }
    // The alias's name maps to the annotation, not to the `<script` tag.
    let source_map =
        sourcemap::SourceMap::from_slice(artifact.source_map.as_deref().unwrap().as_bytes())
            .expect("source map");
    let (line, column) = line_column(code, alias + 1);
    let token = source_map.lookup_token(line, column).expect("token");
    assert_eq!(
        (token.get_src_line(), token.get_src_col()),
        line_column(source, original)
    );
}

#[test]
fn an_alias_of_a_repaired_annotation_maps_what_the_author_wrote() {
    // Recovery writes a placeholder where a type is missing; the placeholder is
    // generated code, so TypeScript's "Cannot find name '_'" is not reported.
    assert_repaired_alias_maps_the_authors_bytes(
        "<script lang=\"ts\">\n\tlet { a }: { a: string; b: } = $props();\n</script>\n<p>{a}</p>\n",
        "{ a: string; b: }",
        "               ^ ",
        ";type $$ComponentProps =  { a: string; b:/*\u{3a9}ignore_start\u{3a9}*/_/*\u{3a9}ignore_end\u{3a9}*/};function $$render() {",
    );
    // It blanks what it cannot finish.
    assert_repaired_alias_maps_the_authors_bytes(
        "<script lang=\"ts\">\n\tlet { a }: { a: Array<string } = $props();\n</script>\n<p>{a}</p>\n",
        "{ a: Array<string }",
        "          ^^^^^^^  ",
        ";type $$ComponentProps =  { a: Array        };function $$render() {",
    );
    assert_repaired_alias_maps_the_authors_bytes(
        "<script lang=\"ts\">\n\tlet { a }: { a: [string, { b: number ] } = $props();\n</script>\n<p>{a}</p>\n",
        "{ a: [string, { b: number ] }",
        "              ^ ^^ ^^^^^^    ",
        ";type $$ComponentProps =  { a: [string,             ] };function $$render() {",
    );
    // Kept inside `$$render` by a generic.
    assert_repaired_alias_maps_the_authors_bytes(
        "<script lang=\"ts\" generics=\"T\">\n\tlet { a, b }: { a: T; b: } = $props();\n</script>\n<p>{a}</p>\n",
        "{ a: T; b: }",
        "          ^ ",
        ";type $$ComponentProps =  { a: T; b:/*\u{3a9}ignore_start\u{3a9}*/_/*\u{3a9}ignore_end\u{3a9}*/};",
    );
}
