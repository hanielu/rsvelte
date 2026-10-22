//! Loose parsing (the language server's `emitOnTemplateError`) ends a block or
//! a component whose closing tag is not typed yet where the template, or an
//! enclosing element, ends. The projection must still close every scope it
//! opened for it: an unclosed `{` swallows the rest of `$$render()`, so the
//! component loses its default export (and every importer its type) while the
//! author is typing. Official svelte2tsx closes `{#if}` and `{#each}` this way
//! (`isImplicitlyClosedBlock`); its other blocks and components either throw
//! or leave the brace open, so those cases assert balanced output instead of
//! official's. Byte-exact strings are official svelte2tsx 0.7.61's output for
//! the same source, minus the blank line that version adds after hoisted
//! imports in every component.

use rsvelte_projection::svelte2tsx::{Svelte2TsxOptions, svelte2tsx};

fn convert(source: &str) -> String {
    svelte2tsx(
        source,
        Svelte2TsxOptions {
            filename: "Test.svelte".to_string(),
            is_ts_file: true,
            emit_on_template_error: true,
            ..Svelte2TsxOptions::default()
        },
    )
    .expect("a block left open is not an error")
    .code
}

/// The template part of the render function, which must close every scope.
fn template(code: &str) -> &str {
    let start = code.find("async () => {").expect("template function");
    let end = code[start..].find("\n};\nreturn ").expect("template end") + start;
    &code[start..end + 3]
}

fn assert_balanced(code: &str) {
    let template = template(code);
    let opened = template.matches('{').count();
    let closed = template.matches('}').count();
    assert_eq!(opened, closed, "unbalanced template:\n{template}");
}

const HEAD: &str = "<script lang=\"ts\">\n\timport Child from \"./Child.svelte\";\n\tlet { a, items }: { a: string; items: string[] } = $props();\n</script>\n\n";

#[test]
fn an_if_block_closed_by_the_end_of_the_template() {
    assert_eq!(
        convert(
            "<script lang=\"ts\">\n\timport Child from \"./Child.svelte\";\n\tlet { a, items }: { a: string; items: string[] } = $props();\n</script>\n\n{#if a}\n<p>{a}</p>\n"
        ),
        "///<reference types=\"svelte\" />\n;\nimport Child from \"./Child.svelte\";\n;type $$ComponentProps =  { a: string; items: string[] };function $$render() {\n\n\t\n\tlet { a, items }:/*Ωignore_startΩ*/$$ComponentProps/*Ωignore_endΩ*/ = $props();\n;\nasync () => {\n\nif(a){\n { svelteHTML.createElement(\"p\", {});a; }}\n};\nreturn { props: {} as any as $$ComponentProps, exports: {}, bindings: __sveltets_$$bindings(''), slots: {}, events: {} }}\nconst Test__SvelteComponent_ = __sveltets_2_fn_component($$render());\n/*Ωignore_startΩ*/type Test__SvelteComponent_ = ReturnType<typeof Test__SvelteComponent_>;\n/*Ωignore_endΩ*/export default Test__SvelteComponent_;"
    );
}

#[test]
fn an_each_block_closed_by_the_end_of_the_template() {
    assert_eq!(
        convert(
            "<script lang=\"ts\">\n\timport Child from \"./Child.svelte\";\n\tlet { a, items }: { a: string; items: string[] } = $props();\n</script>\n\n{#each items as item}\n<p>{item}</p>\n"
        ),
        "///<reference types=\"svelte\" />\n;\nimport Child from \"./Child.svelte\";\n;type $$ComponentProps =  { a: string; items: string[] };function $$render() {\n\n\t\n\tlet { a, items }:/*Ωignore_startΩ*/$$ComponentProps/*Ωignore_endΩ*/ = $props();\n;\nasync () => {\n\n  for(let item of __sveltets_2_ensureArray(items)){\n { svelteHTML.createElement(\"p\", {});item; }}\n};\nreturn { props: {} as any as $$ComponentProps, exports: {}, bindings: __sveltets_$$bindings(''), slots: {}, events: {} }}\nconst Test__SvelteComponent_ = __sveltets_2_fn_component($$render());\n/*Ωignore_startΩ*/type Test__SvelteComponent_ = ReturnType<typeof Test__SvelteComponent_>;\n/*Ωignore_endΩ*/export default Test__SvelteComponent_;"
    );
}

#[test]
fn an_if_else_block_left_open() {
    let code = convert(&[HEAD, "{#if a}\n<p>{a}</p>\n{:else}\n<p>no</p>\n"].concat());
    assert_balanced(&code);
    assert!(
        code.contains("}else{\n { svelteHTML.createElement(\"p\", {});  }}\n};"),
        "{code}"
    );
}

#[test]
fn an_else_if_branch_left_open() {
    let code = convert(&[HEAD, "{#if a}\n<p>{a}</p>\n{:else if items}\n<p>no</p>\n"].concat());
    assert_balanced(&code);
    assert!(
        code.contains("} else if (items){\n { svelteHTML.createElement(\"p\", {});  }}\n};"),
        "{code}"
    );
}

#[test]
fn nested_blocks_left_open() {
    let code = convert(&[HEAD, "{#if a}\n{#each items as item}\n<p>{item}</p>\n"].concat());
    assert_balanced(&code);
    assert!(
        code.contains("{ svelteHTML.createElement(\"p\", {});item; }}}\n};"),
        "{code}"
    );
}

#[test]
fn a_key_block_left_open() {
    let code = convert(&[HEAD, "{#key a}\n<p>{a}</p>\n"].concat());
    assert_balanced(&code);
    assert!(
        code.contains("a; {\n { svelteHTML.createElement(\"p\", {});a; }}\n};"),
        "{code}"
    );
}

#[test]
fn an_await_block_left_open() {
    let code = convert(&[HEAD, "{#await a}\n<p>wait</p>\n{:then v}\n<p>{v}</p>\n"].concat());
    assert_balanced(&code);
    assert!(
        code.contains("{ const v = $$_value; \n { svelteHTML.createElement(\"p\", {});v; }}}\n};"),
        "{code}"
    );
}

#[test]
fn a_component_left_open() {
    let code = convert(&[HEAD, "<Child>\n<p>{a}</p>\n"].concat());
    assert_balanced(&code);
    assert!(code.contains("props: {children:() => { return __sveltets_2_any(0); },}});\n { svelteHTML.createElement(\"p\", {});a; }}\n};"), "{code}");
}

#[test]
fn a_component_left_open_around_one_of_the_same_name() {
    let code = convert(&[HEAD, "<Child>\n<Child>\n<p>x</p>\n</Child>\n"].concat());
    assert_balanced(&code);
    assert!(
        code.contains("{ svelteHTML.createElement(\"p\", {});  }\n Child}}\n};"),
        "{code}"
    );
}

#[test]
fn an_element_left_open_around_one_of_the_same_name() {
    let code = convert(&[HEAD, "<div>\n<div>\n<p>x</p>\n</div>\n"].concat());
    assert_balanced(&code);
    assert!(
        code.contains("{ svelteHTML.createElement(\"p\", {});  }\n }}\n};"),
        "{code}"
    );
}

#[test]
fn a_snippet_left_open() {
    let code = convert(&[HEAD, "{#snippet row(x: string)}\n<p>{x}</p>\n"].concat());
    assert!(code.contains(
        "=> { async ()/*\u{03A9}ignore_position\u{03A9}*/ => {\n { svelteHTML.createElement(\"p\", {});x; }};return __sveltets_2_any(0)};\n"
    ));
    assert_balanced(&code);
}
