//! Block headers in the states they pass through while they are typed. The
//! language server projects every keystroke, and Zed auto-closes `{`, so typing
//! `{#each items as item}` passes through `{#each items as }` and typing
//! `{:then value}` through `{:then }`. Both panicked the projection, which on
//! the server's main loop ended the whole server. Official svelte2tsx rejects
//! them ("Expected identifier or destructure pattern"); they now project as the
//! header without the missing binding. Byte-exact strings are official
//! svelte2tsx 0.7.61's output for that header (`{#each items}`, `{:then}`,
//! `{:catch}`).

use rsvelte_projection::svelte2tsx::{Svelte2TsxOptions, svelte2tsx};

fn project(source: &str) -> Result<String, String> {
    svelte2tsx(
        source,
        Svelte2TsxOptions {
            filename: "Test.svelte".to_string(),
            is_ts_file: true,
            emit_jsdoc: true,
            emit_on_template_error: true,
            ..Svelte2TsxOptions::default()
        },
    )
    .map(|result| result.code)
    .map_err(|error| error.to_string())
}

fn convert(source: &str) -> String {
    project(source).expect("a half-typed block header is not an error in loose mode")
}

const HEAD: &str =
    "<script lang=\"ts\">\n\tlet items = [1, 2];\n\tlet p = Promise.resolve(1);\n</script>\n\n";

const TAIL: &str = "return { props: {} as Record<string, never>, exports: {}, bindings: \"\", slots: {}, events: {} }}\nconst Test__SvelteComponent_ = __sveltets_2_isomorphic_component(__sveltets_2_with_any_event($$render()));\n/*Ωignore_startΩ*/type Test__SvelteComponent_ = InstanceType<typeof Test__SvelteComponent_>;\n/*Ωignore_endΩ*/export default Test__SvelteComponent_;";

const RENDER: &str = "///<reference types=\"svelte\" />\n;function $$render() {\n\n\tlet items = [1, 2];\n\tlet p = Promise.resolve(1);\n;\nasync () => {\n\n";

/// Project every prefix of `typed` between `before` and `after`, as each
/// keystroke would. An unfinished keyword (`{:th}`) may be an error, which the
/// language server answers with its script-only snapshot; nothing may panic.
fn type_out(before: &str, typed: &str, after: &str) {
    for end in 1..=typed.len() {
        let _ = project(&[HEAD, before, &typed[..end], after].concat());
    }
}

#[test]
fn a_then_branch_with_nothing_typed_after_the_keyword() {
    assert_eq!(
        convert(&[HEAD, "{#await p}\n\t<p>loading</p>\n{:then }\n\t<p>done</p>\n{/await}\n"].concat()),
        [
            RENDER,
            "   { \n\t { svelteHTML.createElement(\"p\", {});  }\nawait (p);\n\t { svelteHTML.createElement(\"p\", {});  }\n}\n};\n",
            TAIL
        ]
        .concat()
    );
}

#[test]
fn a_catch_branch_with_nothing_typed_after_the_keyword() {
    assert_eq!(
        convert(&[HEAD, "{#await p}\n\t<p>loading</p>\n{:catch }\n\t<p>done</p>\n{/await}\n"].concat()),
        [
            RENDER,
            "   { \n\t { svelteHTML.createElement(\"p\", {});  }\ntry { await (p);} catch($$_e) { \n\t { svelteHTML.createElement(\"p\", {});  }\n}}\n};\n",
            TAIL
        ]
        .concat()
    );
}

#[test]
fn every_keystroke_of_a_then_binding_with_an_auto_closed_brace() {
    type_out(
        "{#await p}\n\t<p>loading</p>\n",
        "{:then value",
        "}\n\t<p>done</p>\n{/await}\n",
    );
    type_out(
        "{#await p}\n\t<p>loading</p>\n",
        "{:catch error",
        "}\n\t<p>done</p>\n{/await}\n",
    );
    type_out("", "{#await p then value", "}\n\t<p>done</p>\n{/await}\n");
}

#[test]
fn an_each_block_with_nothing_typed_after_as() {
    assert_eq!(
        convert(&[HEAD, "{#each items as }\n<p>after</p>\n"].concat()),
        [
            RENDER,
            " for(let $$each_item of __sveltets_2_ensureArray(items)){$$each_item;\n { svelteHTML.createElement(\"p\", {});  }}\n};\n",
            TAIL
        ]
        .concat()
    );
}

#[test]
fn an_each_block_with_an_index_but_no_context() {
    let code = convert(&[HEAD, "{#each items as , i}\n<p>{i}</p>\n{/each}\n"].concat());
    assert!(
        code.contains(
            " for(let $$each_item of __sveltets_2_ensureArray(items)){$$each_item;let i = 1;"
        ),
        "{code}"
    );
}

#[test]
fn every_keystroke_of_an_each_block_with_an_auto_closed_brace() {
    type_out("", "{#each items as item, i (item)", "}\n<p>after</p>\n");
    type_out("", "{#each items as { a, b }", "}\n<p>{a}</p>\n{/each}\n");
    type_out("", "{#each items as [a, b]", "}\n<p>{a}</p>\n{/each}\n");
    assert!(
        convert(&[HEAD, "{#each items as item}\n<p>{item}</p>\n{/each}\n"].concat())
            .contains("  for(let item of __sveltets_2_ensureArray(items)){\n"),
    );
}

#[test]
fn an_await_block_with_nothing_typed_after_await() {
    // Found by fuzzing: an empty promise in front of a pending block was
    // relocated with `move_range`, which panicked on the empty range.
    let code = convert(
        &[
            HEAD,
            "{#await }\n\t<p>loading</p>\n{:then value}\n\t<p>{value}</p>\n{/await}\n",
        ]
        .concat(),
    );
    assert!(code.contains("await ()"), "{code}");
    assert!(code.contains("const value = $$_value;"), "{code}");
    convert("{#await }\n{:then ");
}

#[test]
fn every_keystroke_of_an_await_block_with_an_auto_closed_brace() {
    type_out(
        "",
        "{#await p",
        "}\n\t<p>loading</p>\n{:then value}\n\t<p>done</p>\n{/await}\n",
    );
    type_out(
        "",
        "{#await p",
        "}\n\t<p>loading</p>\n{:catch error}\n\t<p>done</p>\n{/await}\n",
    );
    type_out("", "{#await p catch error", "}\n\t<p>done</p>\n{/await}\n");
}
