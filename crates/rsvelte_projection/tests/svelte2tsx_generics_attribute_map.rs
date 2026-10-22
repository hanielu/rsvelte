//! A TypeScript component's `generics` attribute becomes the type parameters of
//! `$$render<…>`. Upstream leaves the attribute's value in place between the
//! overwrites of the tag around it, so the type parameters map back to the
//! attribute; here the header was one generated string, so they mapped to the
//! `<script` tag (0:1), and a rename of `T` or an error in its constraint
//! landed there. The value now stays in place, and the code is unchanged.

use rsvelte_projection::{
    ByteRange, ProjectionArtifact, ProjectionEngine, svelte2tsx::Svelte2TsxOptions,
};

fn project(source: &str) -> ProjectionArtifact {
    ProjectionEngine::new()
        .project(
            source,
            Svelte2TsxOptions {
                filename: "List.svelte".to_string(),
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

/// `$$render<value>` holds the attribute's `value` and maps back to it exactly,
/// while `function $$render` keeps mapping to the tag.
fn assert_render_generics_map_to_the_attribute(source: &str, value: &str) -> ProjectionArtifact {
    let artifact = project(source);
    let code = &artifact.code;
    let generated = code
        .find(&format!("function $$render<{value}>() {{"))
        .unwrap_or_else(|| panic!("no $$render<{value}>:\n{code}"))
        + "function $$render<".len();
    let original = source.find(value).expect("attribute value");

    let map = artifact.exact_mappings.as_ref().expect("exact mappings");
    let generated_range =
        ByteRange::new(generated as u32, (generated + value.len()) as u32).unwrap();
    let original_range = ByteRange::new(original as u32, (original + value.len()) as u32).unwrap();
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
    let (line, column) = line_column(code, generated);
    let (value_line, value_column) = line_column(source, original);
    for offset in 0..value.len() as u32 {
        let token = source_map
            .lookup_token(line, column + offset)
            .expect("token");
        assert_eq!(
            (token.get_src_line(), token.get_src_col()),
            (value_line, value_column + offset),
            "column {offset} of {value:?}"
        );
    }
    let render = code.find("function $$render<").unwrap();
    let (render_line, render_column) = line_column(code, render);
    let token = source_map
        .lookup_token(render_line, render_column + 9)
        .expect("token");
    assert_eq!((token.get_src_line(), token.get_src_col()), (0, 1));
    artifact
}

#[test]
fn render_type_parameters_map_back_to_the_generics_attribute() {
    assert_render_generics_map_to_the_attribute(
        "<script lang=\"ts\" generics=\"T extends string, U = number\">\n\tlet { item, n }: { item: T; n: U } = $props();\n\tlet x: T = item;\n</script>\n<p>{item}{x}{n}</p>\n",
        "T extends string, U = number",
    );
    assert_render_generics_map_to_the_attribute(
        "<script generics='T extends { id: string }' lang=\"ts\">\n\tlet { items }: { items: T[] } = $props();\n</script>\n{#each items as item (item.id)}<p>{item.id}</p>{/each}\n",
        "T extends { id: string }",
    );
    assert_render_generics_map_to_the_attribute(
        "<script lang=\"ts\" generics=T>\n\tlet { item }: { item: T } = $props();\n</script>\n<p>{String(item)}</p>\n",
        "T",
    );
}

#[test]
fn render_type_parameters_map_back_after_a_hoisted_props_alias() {
    // The props type does not use `T`, so its alias leads the header.
    let source = "<script lang=\"ts\" generics=\"T\">\n\tlet { label }: { label: string } = $props();\n\tlet value: T | undefined;\n</script>\n<p>{label}{value}</p>\n";
    let artifact = assert_render_generics_map_to_the_attribute(source, "T");
    assert!(
        artifact
            .code
            .contains(";type $$ComponentProps =  { label: string };function $$render<T>() {"),
        "{}",
        artifact.code
    );
    let annotation = source.find("{ label: string }").unwrap() as u32;
    let map = artifact.exact_mappings.as_ref().unwrap();
    let alias = artifact.code.find("{ label: string }").unwrap() as u32;
    assert_eq!(map.generated_to_source(alias), Some(annotation));
}
