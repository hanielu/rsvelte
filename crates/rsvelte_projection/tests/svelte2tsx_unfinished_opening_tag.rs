//! An opening tag being typed above existing markup (`<div class="x"` before
//! `<p>{a}</p>`) used to fail the loose parse, so the language server fell
//! back to a script-only snapshot and lost every template feature. Loose
//! parsing now backtracks to the tag it read as an attribute, as upstream's
//! does, and the component projects with both elements.

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
    .expect("an unfinished opening tag projects")
    .code
}

const HEAD: &str = "<script lang=\"ts\">\n\timport Child from \"./Child.svelte\";\n\tlet { a }: { a: string } = $props();\n</script>\n\n";

#[test]
fn an_element_being_typed_above_an_element() {
    let code = convert(&[HEAD, "<div class=\"x\"\n<p>{a}</p>\n"].concat());
    assert!(code.contains(
        "async () => {\n\n { svelteHTML.createElement(\"div\", { \"class\":`x`,});} { svelteHTML.createElement(\"p\", {});a; }\n};"
    ));
}

#[test]
fn a_component_being_typed_above_an_element() {
    let code = convert(&[HEAD, "<Child foo={a}\n<p>{a}</p>\n"].concat());
    assert!(code.contains(
        "new $$_dlihC0C({ target: __sveltets_2_any(), props: { \"foo\":a,}});} { svelteHTML.createElement(\"p\", {});a; }\n};"
    ));
}

#[test]
fn an_element_being_typed_inside_an_element() {
    let code = convert(
        &[
            HEAD,
            "<section>\n\t<div class=\"x\"\n\t<p>{a}</p>\n</section>\n",
        ]
        .concat(),
    );
    assert!(code.contains(
        " { svelteHTML.createElement(\"section\", {});\n\t  { svelteHTML.createElement(\"div\", { \"class\":`x`,});} { svelteHTML.createElement(\"p\", {});a; }\n }\n};"
    ));
}

/// A script tag that is never closed, with `</script>` inside an attribute
/// value: the content's end was searched from the tag's start and found in
/// the attribute, before the content, and slicing the content panicked
/// ("byte range starts at … but ends at …"; found by fuzzing). Official
/// svelte2tsx rejects the document; nothing may panic.
#[test]
fn a_script_tag_with_its_closing_tag_in_an_attribute() {
    for source in [
        "<script l=\"</script>\"/",
        "<script lang=\"ts\" l=\"</script>\"/>",
        "<script lang=\"ts\" l=\"</script>\"/>\n$: a = 1;\nimport A from './A.svelte';\n",
    ] {
        let _ = svelte2tsx(
            source,
            Svelte2TsxOptions {
                filename: "Test.svelte".to_string(),
                is_ts_file: true,
                emit_on_template_error: true,
                ..Svelte2TsxOptions::default()
            },
        );
    }
    // A closed script with the same attribute is unaffected.
    let code = convert(
        "<script lang=\"ts\" l=\"</script>\">\n\timport A from './A.svelte';\n\t$: b = 1;\n</script>\n<A />\n",
    );
    assert!(code.contains("import A from './A.svelte';"), "{code}");
    assert!(
        code.contains("let  b = __sveltets_2_invalidate(() => 1);"),
        "{code}"
    );
}

fn convert_as(source: &str, is_ts_file: bool) -> String {
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
    .expect("the component projects")
    .code
}

/// `<script </script>` in an attribute value: the scan for script tags the
/// parser did not see took the `>` of `</script>` as the end of the opening
/// tag, so the content started after the closing tag and slicing it panicked
/// ("byte range starts at … but ends at …"; the review's fuzzing). Upstream's
/// `scriptRegex` needs a `</script>` after that `>`, and attributes it can
/// read before it, and finds no script here. Expected output from official
/// svelte2tsx 0.7.61.
#[test]
fn a_script_opening_tag_ended_by_its_own_closing_tag_in_an_attribute() {
    assert_eq!(
        convert_as("<p>a</p>\n<div title=\"<script </script>\"></div>\n", false),
        "///<reference types=\"svelte\" />\n;function $$render() {\nasync () => { { svelteHTML.createElement(\"p\", {});  }\n { svelteHTML.createElement(\"div\", { \"title\":`<script </script>`,}); }\n};\nreturn { props: /** @type {Record<string, never>} */ ({}), exports: {}, bindings: \"\", slots: {}, events: {} }}\nexport const Test__SvelteComponent_ = __sveltets_2_isomorphic_component(__sveltets_2_partial(__sveltets_2_with_any_event($$render())));\n/*Ωignore_startΩ*//** @typedef {InstanceType<typeof Test__SvelteComponent_>} Test__SvelteComponent_ */\n/*Ωignore_endΩ*/export default Test__SvelteComponent_;"
    );
    assert_eq!(
        convert_as(
            "<script lang=\"ts\">\n\tlet { a }: { a: string } = $props();\n</script>\n\n<div title=\"<script </script>\">{a}</div>\n<p>{a}</p>\n",
            true
        ),
        "///<reference types=\"svelte\" />\n;\n;type $$ComponentProps =  { a: string };function $$render() {\n\n\tlet { a }:/*Ωignore_startΩ*/$$ComponentProps/*Ωignore_endΩ*/ = $props();\n;\nasync () => {\n\n { svelteHTML.createElement(\"div\", { \"title\":`<script </script>`,});a; }\n { svelteHTML.createElement(\"p\", {});a; }\n};\nreturn { props: {} as any as $$ComponentProps, exports: {}, bindings: __sveltets_$$bindings(''), slots: {}, events: {} }}\nconst Test__SvelteComponent_ = __sveltets_2_fn_component($$render());\n/*Ωignore_startΩ*/type Test__SvelteComponent_ = ReturnType<typeof Test__SvelteComponent_>;\n/*Ωignore_endΩ*/export default Test__SvelteComponent_;"
    );
    // Nor are tags whose attributes upstream's regex does not read.
    assert_eq!(
        convert_as("<div title=\"<script a/> b </script>\"></div>\n", false),
        "///<reference types=\"svelte\" />\n;function $$render() {\nasync () => { { svelteHTML.createElement(\"div\", { \"title\":`<script a/> b </script>`,}); }\n};\nreturn { props: /** @type {Record<string, never>} */ ({}), exports: {}, bindings: \"\", slots: {}, events: {} }}\nexport const Test__SvelteComponent_ = __sveltets_2_isomorphic_component(__sveltets_2_partial(__sveltets_2_with_any_event($$render())));\n/*Ωignore_startΩ*//** @typedef {InstanceType<typeof Test__SvelteComponent_>} Test__SvelteComponent_ */\n/*Ωignore_endΩ*/export default Test__SvelteComponent_;"
    );
    assert_eq!(
        convert_as(
            "<div title=\"<script a </script> b </script>\"></div>\n",
            false
        ),
        "///<reference types=\"svelte\" />\n;function $$render() {\nasync () => { { svelteHTML.createElement(\"div\", { \"title\":`<script a </script> b </script>`,}); }\n};\nreturn { props: /** @type {Record<string, never>} */ ({}), exports: {}, bindings: \"\", slots: {}, events: {} }}\nexport const Test__SvelteComponent_ = __sveltets_2_isomorphic_component(__sveltets_2_partial(__sveltets_2_with_any_event($$render())));\n/*Ωignore_startΩ*//** @typedef {InstanceType<typeof Test__SvelteComponent_>} Test__SvelteComponent_ */\n/*Ωignore_endΩ*/export default Test__SvelteComponent_;"
    );
}

/// The review's minimized keystroke state: an each block whose collection is
/// being typed as `<script </script>`. Official svelte2tsx rejects it; the
/// projection keeps the each block instead of panicking.
#[test]
fn an_each_block_whose_collection_holds_an_unfinished_script_tag() {
    let code = convert_as("{#each <script </script>", false);
    assert!(
        code.contains(
            "for(let $$each_item of __sveltets_2_ensureArray(<script </script>)){$$each_item;}"
        ),
        "{code}"
    );
}
