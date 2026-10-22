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
