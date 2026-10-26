//! A `{const x = $derived(…)}` declaration tag is scoped to its block. The
//! binding is `Derived`, not one of the block-local kinds, so a lookup from
//! elsewhere in the template — a `bind:` setter's `(open) =>` parameter —
//! reached it through scope 0's copy of every declaration and warned
//! `state_referenced_locally` on the parameter.
//!
//! Every expectation is the official compiler's answer (svelte 5.57.2) for
//! the same source.

use rsvelte_core::compiler::CompileOptions;
use rsvelte_core::{GenerateMode, compile};

fn warnings(source: &str) -> Vec<String> {
    compile(
        source,
        CompileOptions {
            filename: Some("X.svelte".to_string()),
            generate: GenerateMode::Client,
            dev: false,
            ..Default::default()
        },
    )
    .expect("compile")
    .warnings
    .iter()
    .map(|w| {
        let start = w.start.as_ref().map(|p| (p.line, p.column));
        format!("{}@{start:?}", w.code)
    })
    .collect()
}

const SCRIPT: &str = "<script>\n\tlet items = $state([1]);\n\tlet set = (v) => v;\n</script>\n\n";
const DECLARED: &str =
    "{#each items as b}\n\t{const open = $derived(b > 0)}\n\t<p>{open}</p>\n{/each}\n";

#[test]
fn a_setter_parameter_in_a_sibling_block_is_not_the_declaration() {
    let source = format!(
        "{SCRIPT}{{#each items as a}}\n\t<input bind:value={{() => a, (open) => set(open)}} />\n{{/each}}\n{DECLARED}"
    );
    assert_eq!(warnings(&source), Vec::<String>::new());
}

#[test]
fn a_setter_parameter_at_the_top_level_is_not_the_declaration() {
    let source =
        format!("{SCRIPT}<input bind:value={{() => 1, (open) => set(open)}} />\n{DECLARED}");
    assert_eq!(warnings(&source), Vec::<String>::new());
}

/// Live control: inside its own block the declaration still warns.
#[test]
fn a_reference_inside_the_block_still_warns() {
    let source = "<script>\n\tlet items = $state([1]);\n</script>\n\n{#each items as b}\n\t{const open = $derived(b > 0)}\n\t{const copy = open}\n\t<p>{copy}</p>\n{/each}\n";
    assert_eq!(
        warnings(source),
        vec!["state_referenced_locally@Some((7, 15))".to_string()]
    );
}
