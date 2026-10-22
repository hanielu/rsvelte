//! `export let { a = $bindable(1) } = $props()` mixes the legacy `export let`
//! with the runes props destructure (invalid Svelte, but a state an author
//! reaches while converting a component). The export path read the
//! `$bindable(…)` argument out of an empty script text and panicked ("start
//! byte index … out of bounds for string of length 0"); the language server
//! projects on its main loop, so the panic ended the whole server. Byte-exact
//! strings are official svelte2tsx 0.7.61's output.

use rsvelte_projection::svelte2tsx::{Svelte2TsxOptions, svelte2tsx};

fn convert(source: &str, is_ts_file: bool) -> String {
    svelte2tsx(
        source,
        Svelte2TsxOptions {
            filename: "Test.svelte".to_string(),
            is_ts_file,
            emit_jsdoc: true,
            emit_on_template_error: true,
            ..Svelte2TsxOptions::default()
        },
    )
    .expect("an exported props destructure projects")
    .code
}

#[test]
fn an_exported_props_destructure_with_a_bindable_default() {
    assert_eq!(
        convert(
            "<script lang=\"ts\">\n\texport let { a = $bindable(1) } = $props();\n</script>\n",
            true
        ),
        "///<reference types=\"svelte\" />\n;function $$render() {\n\n\t let { a = $bindable(1) } = $props();\n;\nasync () => {\n};\nreturn { props: {a: a} as {a?: typeof a}, exports: {}, bindings: \"\", slots: {}, events: {} }}\nconst Test__SvelteComponent_ = __sveltets_2_isomorphic_component(__sveltets_2_with_any_event($$render()));\n/*Ωignore_startΩ*/type Test__SvelteComponent_ = InstanceType<typeof Test__SvelteComponent_>;\n/*Ωignore_endΩ*/export default Test__SvelteComponent_;"
    );
}

#[test]
fn bindable_arguments_typed_out_in_typescript_and_javascript() {
    for is_ts_file in [true, false] {
        let lang = if is_ts_file { " lang=\"ts\"" } else { "" };
        let typed = "$bindable(value)";
        for end in 1..=typed.len() {
            let source = format!(
                "<script{lang}>\n\tlet value = 1;\n\texport let {{ f = {} }} = $props();\n</script>\n",
                &typed[..end]
            );
            let code = convert(&source, is_ts_file);
            assert!(code.contains("export default"), "{code}");
        }
    }
}
