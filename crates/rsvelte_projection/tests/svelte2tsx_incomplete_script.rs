//! The instance script is half-typed on almost every keystroke: `runtime.`
//! before `</script>`, a call or an initializer not finished yet, an import
//! still missing its `from`. Official svelte2tsx parses with TypeScript, whose
//! parser is error-tolerant, so such a component still gets every script
//! transform — its imports hoisted out of `$$render()`, where TypeScript would
//! resolve none of them, its props typed, its stores subscribed. The expected
//! strings below are official svelte2tsx 0.7.61's output for the same source
//! (`filename: "Test.svelte"`, `mode: 'ts'`, `emitOnTemplateError: true`,
//! `emitJsDoc: true`), minus the blank line that version adds after hoisted
//! imports in every component, one that parses included.

use rsvelte_projection::svelte2tsx::{Svelte2TsxOptions, svelte2tsx};
use rsvelte_projection::{ByteRange, ProjectionEngine, is_typescript_component};

fn options(source: &str) -> Svelte2TsxOptions {
    Svelte2TsxOptions {
        filename: "Test.svelte".to_string(),
        is_ts_file: is_typescript_component(source),
        emit_jsdoc: true,
        emit_on_template_error: true,
        ..Svelte2TsxOptions::default()
    }
}

fn convert(source: &str) -> String {
    svelte2tsx(source, options(source))
        .expect("an unfinished instance script is not an error")
        .code
}

#[test]
fn a_member_access_before_the_closing_tag() {
    assert_eq!(
        convert(
            "<script lang=\"ts\">\n\timport Child from \"./Child.svelte\";\n\timport { runtime } from \"./runtime\";\n\tlet { a }: { a: string } = $props();\n\truntime.\n</script>\n\n<Child />\n<p>{a}</p>\n"
        ),
        "///<reference types=\"svelte\" />\n;\nimport Child from \"./Child.svelte\";\nimport { runtime } from \"./runtime\";\n;type $$ComponentProps =  { a: string };function $$render() {\n\n\t\n\t\n\tlet { a }:/*Ωignore_startΩ*/$$ComponentProps/*Ωignore_endΩ*/ = $props();\n\truntime.\n;\nasync () => {\n\n { const $$_dlihC0C = __sveltets_2_ensureComponent(Child); new $$_dlihC0C({ target: __sveltets_2_any(), props: {}});}\n { svelteHTML.createElement(\"p\", {});a; }\n};\nreturn { props: {} as any as $$ComponentProps, exports: {}, bindings: __sveltets_$$bindings(''), slots: {}, events: {} }}\nconst Test__SvelteComponent_ = __sveltets_2_fn_component($$render());\n/*Ωignore_startΩ*/type Test__SvelteComponent_ = ReturnType<typeof Test__SvelteComponent_>;\n/*Ωignore_endΩ*/export default Test__SvelteComponent_;"
    );
}

#[test]
fn a_member_access_inside_a_function() {
    assert_eq!(
        convert(
            "<script lang=\"ts\">\n\timport { runtime } from \"./runtime\";\n\tlet { a }: { a: string } = $props();\n\tfunction go() {\n\t\truntime.\n\t}\n</script>\n\n<p>{a}</p>\n"
        ),
        "///<reference types=\"svelte\" />\n;\nimport { runtime } from \"./runtime\";\n;type $$ComponentProps =  { a: string };function $$render() {\n\n\t\n\tlet { a }:/*Ωignore_startΩ*/$$ComponentProps/*Ωignore_endΩ*/ = $props();\n\tfunction go() {\n\t\truntime.\n\t}\n;\nasync () => {\n\n { svelteHTML.createElement(\"p\", {});a; }\n};\nreturn { props: {} as any as $$ComponentProps, exports: {}, bindings: __sveltets_$$bindings(''), slots: {}, events: {} }}\nconst Test__SvelteComponent_ = __sveltets_2_fn_component($$render());\n/*Ωignore_startΩ*/type Test__SvelteComponent_ = ReturnType<typeof Test__SvelteComponent_>;\n/*Ωignore_endΩ*/export default Test__SvelteComponent_;"
    );
}

#[test]
fn an_unclosed_call() {
    assert_eq!(
        convert(
            "<script lang=\"ts\">\n\timport { runtime } from \"./runtime\";\n\tlet { a }: { a: string } = $props();\n\truntime.go(\n</script>\n\n<p>{a}</p>\n"
        ),
        "///<reference types=\"svelte\" />\n;\nimport { runtime } from \"./runtime\";\n;type $$ComponentProps =  { a: string };function $$render() {\n\n\t\n\tlet { a }:/*Ωignore_startΩ*/$$ComponentProps/*Ωignore_endΩ*/ = $props();\n\truntime.go(\n;\nasync () => {\n\n { svelteHTML.createElement(\"p\", {});a; }\n};\nreturn { props: {} as any as $$ComponentProps, exports: {}, bindings: __sveltets_$$bindings(''), slots: {}, events: {} }}\nconst Test__SvelteComponent_ = __sveltets_2_fn_component($$render());\n/*Ωignore_startΩ*/type Test__SvelteComponent_ = ReturnType<typeof Test__SvelteComponent_>;\n/*Ωignore_endΩ*/export default Test__SvelteComponent_;"
    );
}

#[test]
fn a_dangling_initializer() {
    assert_eq!(
        convert(
            "<script lang=\"ts\">\n\timport { runtime } from \"./runtime\";\n\tlet { a }: { a: string } = $props();\n\tconst b = \n</script>\n\n<p>{a}</p>\n"
        ),
        "///<reference types=\"svelte\" />\n;\nimport { runtime } from \"./runtime\";\n;type $$ComponentProps =  { a: string };function $$render() {\n\n\t\n\tlet { a }:/*Ωignore_startΩ*/$$ComponentProps/*Ωignore_endΩ*/ = $props();\n\tconst b = \n;\nasync () => {\n\n { svelteHTML.createElement(\"p\", {});a; }\n};\nreturn { props: {} as any as $$ComponentProps, exports: {}, bindings: __sveltets_$$bindings(''), slots: {}, events: {} }}\nconst Test__SvelteComponent_ = __sveltets_2_fn_component($$render());\n/*Ωignore_startΩ*/type Test__SvelteComponent_ = ReturnType<typeof Test__SvelteComponent_>;\n/*Ωignore_endΩ*/export default Test__SvelteComponent_;"
    );
}

#[test]
fn an_unclosed_props_call() {
    assert_eq!(
        convert(
            "<script lang=\"ts\">\n\timport { runtime } from \"./runtime\";\n\tlet { a }: { a: string } = $props(\n</script>\n\n<p>{a}</p>\n"
        ),
        "///<reference types=\"svelte\" />\n;\nimport { runtime } from \"./runtime\";\n;type $$ComponentProps =  { a: string };function $$render() {\n\n\t\n\tlet { a }:/*Ωignore_startΩ*/$$ComponentProps/*Ωignore_endΩ*/ = $props(\n;\nasync () => {\n\n { svelteHTML.createElement(\"p\", {});a; }\n};\nreturn { props: {} as any as $$ComponentProps, exports: {}, bindings: __sveltets_$$bindings(''), slots: {}, events: {} }}\nconst Test__SvelteComponent_ = __sveltets_2_fn_component($$render());\n/*Ωignore_startΩ*/type Test__SvelteComponent_ = ReturnType<typeof Test__SvelteComponent_>;\n/*Ωignore_endΩ*/export default Test__SvelteComponent_;"
    );
}

#[test]
fn an_import_whose_specifier_is_being_typed() {
    assert_eq!(
        convert(
            "<script lang=\"ts\">\n\timport { runtime } from \"./runtime\";\n\tlet { a }: { a: string } = $props();\n\timport { abc } from './run\n</script>\n\n<p>{a}</p>\n"
        ),
        "///<reference types=\"svelte\" />\n;\nimport { runtime } from \"./runtime\";\nimport { abc } from './run;\n;type $$ComponentProps =  { a: string };function $$render() {\n\n\t\n\tlet { a }:/*Ωignore_startΩ*/$$ComponentProps/*Ωignore_endΩ*/ = $props();\n\t\n;\nasync () => {\n\n { svelteHTML.createElement(\"p\", {});a; }\n};\nreturn { props: {} as any as $$ComponentProps, exports: {}, bindings: __sveltets_$$bindings(''), slots: {}, events: {} }}\nconst Test__SvelteComponent_ = __sveltets_2_fn_component($$render());\n/*Ωignore_startΩ*/type Test__SvelteComponent_ = ReturnType<typeof Test__SvelteComponent_>;\n/*Ωignore_endΩ*/export default Test__SvelteComponent_;"
    );
}

#[test]
fn a_member_access_on_a_store() {
    assert_eq!(
        convert(
            "<script>\n\timport { writable } from 'svelte/store';\n\texport let a = 1;\n\tconst s = writable(1);\n\t$s.\n</script>\n\n<p>{a}</p>\n"
        ),
        "///<reference types=\"svelte\" />\n;\nimport { writable } from 'svelte/store';\nfunction $$render() {\n\n\t\n\t let a = 1;\n\tconst s = writable(1)/*Ωignore_startΩ*/;let $s = __sveltets_2_store_get(s);/*Ωignore_endΩ*/;\n\t$s.\n;\nasync () => {\n\n { svelteHTML.createElement(\"p\", {});a; }\n};\nreturn { props: {a: a}, exports: {}, bindings: \"\", slots: {}, events: {} }}\nexport const Test__SvelteComponent_ = __sveltets_2_isomorphic_component(__sveltets_2_partial(['a'], __sveltets_2_with_any_event($$render())));\n/*Ωignore_startΩ*//** @typedef {InstanceType<typeof Test__SvelteComponent_>} Test__SvelteComponent_ */\n/*Ωignore_endΩ*/export default Test__SvelteComponent_;"
    );
}

#[test]
fn a_legacy_prop_without_its_initializer() {
    assert_eq!(
        convert(
            "<script>\n\timport Child from './Child.svelte';\n\texport let a = 1;\n\texport let b =\n</script>\n\n<Child {a} {b} />\n"
        ),
        "///<reference types=\"svelte\" />\n;\nimport Child from './Child.svelte';\nfunction $$render() {\n\n\t\n\t let a = 1;\n\t let b =\n;\nasync () => {\n\n { const $$_dlihC0C = __sveltets_2_ensureComponent(Child); new $$_dlihC0C({ target: __sveltets_2_any(), props: {  a,b,}});}\n};\nreturn { props: {a: a , b: b}, exports: {}, bindings: \"\", slots: {}, events: {} }}\nexport const Test__SvelteComponent_ = __sveltets_2_isomorphic_component(__sveltets_2_partial(['a','b'], __sveltets_2_with_any_event($$render())));\n/*Ωignore_startΩ*//** @typedef {InstanceType<typeof Test__SvelteComponent_>} Test__SvelteComponent_ */\n/*Ωignore_endΩ*/export default Test__SvelteComponent_;"
    );
}

#[test]
fn the_unfinished_text_keeps_its_exact_mapping() {
    let source =
        "<script lang=\"ts\">\n\timport { runtime } from \"./runtime\";\n\truntime.\n</script>\n";
    let projection = ProjectionEngine::new()
        .project(source, options(source))
        .expect("projects");
    let mappings = projection.exact_mappings.expect("exact mappings");
    let start = u32::try_from(source.find("runtime.\n").unwrap()).unwrap();
    let range = ByteRange::new(start, start + 8).unwrap();
    let generated = mappings.source_range_to_generated(range);
    assert_eq!(generated.len(), 1);
    assert_eq!(&projection.code[generated[0].as_usize_range()], "runtime.");
    let import = projection.code.find("import { runtime }").unwrap();
    assert!(import < projection.code.find("function $$render").unwrap());
}

#[test]
fn an_import_without_from_does_not_swallow_the_next_declaration() {
    // TypeScript reads `let` as the missing module specifier here and loses
    // the props; the recovered import ends at its own line.
    let code = convert(
        "<script lang=\"ts\">\n\timport { runtime } from \"./runtime\";\n\timport { abc }\n\tlet { a }: { a: string } = $props();\n</script>\n\n<p>{a}</p>\n",
    );
    assert!(code.contains(
        "import { runtime } from \"./runtime\";\nimport { abc };\n;type $$ComponentProps =  { a: string };function $$render() {"
    ));
    assert!(
        code.contains(
            "\tlet { a }:/*Ωignore_startΩ*/$$ComponentProps/*Ωignore_endΩ*/ = $props();\n"
        )
    );
}

#[test]
fn a_member_access_in_the_module_script() {
    assert_eq!(
        convert(
            "<script module>\n\timport { writable } from \"svelte/store\";\n\texport const count = writable(0);\n\tcount.\n</script>\n\n<p>{$count}</p>\n"
        ),
        "///<reference types=\"svelte\" />\n;\n\timport { writable } from \"svelte/store\";\n\texport const count = writable(0)/*Ωignore_startΩ*/;let $count = __sveltets_2_store_get(count);/*Ωignore_endΩ*/;\n\tcount.\n;;function $$render() {\nasync () => {\n\n { svelteHTML.createElement(\"p\", {});$count; }\n};\nreturn { props: /** @type {Record<string, never>} */ ({}), exports: {}, bindings: \"\", slots: {}, events: {} }}\nexport const Test__SvelteComponent_ = __sveltets_2_isomorphic_component(__sveltets_2_partial(__sveltets_2_with_any_event($$render())));\n/*Ωignore_startΩ*//** @typedef {InstanceType<typeof Test__SvelteComponent_>} Test__SvelteComponent_ */\n/*Ωignore_endΩ*/export default Test__SvelteComponent_;"
    );
}
