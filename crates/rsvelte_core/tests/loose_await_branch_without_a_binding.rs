//! A `{:then}` or `{:catch}` branch whose binding is not typed yet. Typing
//! `{:then value}` in an editor that auto-closes `{` passes through
//! `{:then }`: loose parsing read an empty pattern there and sliced the source
//! backwards (`byte range starts at X but ends at X-1`), a panic that took the
//! language server down. Upstream's loose parser rejects these states
//! ("Expected identifier or destructure pattern"); rsvelte reads them as the
//! branch without a binding, so the template keeps its types while the author
//! types.

use rsvelte_core::ast::template::{AwaitBlock, TemplateNode};
use rsvelte_core::{ParseOptions, parse};

/// `(value, error, then, catch)` presence of the document's await block.
fn await_shape(source: &str, loose: bool) -> Result<(bool, bool, bool, bool), String> {
    let allocator = oxc_allocator::Allocator::default();
    let options = ParseOptions {
        modern: true,
        loose,
        ..Default::default()
    };
    let root = parse(source, &allocator, options).map_err(|error| error.to_string())?;
    let block: &AwaitBlock = root
        .fragment
        .nodes
        .iter()
        .find_map(|node| match node {
            TemplateNode::AwaitBlock(block) => Some(&**block),
            _ => None,
        })
        .expect("an await block");
    Ok((
        block.value.is_some(),
        block.error.is_some(),
        block.then.is_some(),
        block.catch.is_some(),
    ))
}

#[test]
fn a_then_branch_with_nothing_typed_after_the_keyword() {
    assert_eq!(
        await_shape(
            "{#await p}\n\t<p>loading</p>\n{:then }\n\t<p>done</p>\n{/await}\n",
            true
        ),
        Ok((false, false, true, false))
    );
}

#[test]
fn a_catch_branch_with_nothing_typed_after_the_keyword() {
    assert_eq!(
        await_shape(
            "{#await p}\n\t<p>loading</p>\n{:catch }\n\t<p>failed</p>\n{/await}\n",
            true
        ),
        Ok((false, false, false, true))
    );
}

#[test]
fn an_opening_tag_with_nothing_typed_after_then() {
    assert_eq!(
        await_shape("{#await p then }\n\t<p>done</p>\n{/await}\n", true),
        Ok((false, false, true, false))
    );
}

#[test]
fn every_keystroke_of_a_binding_with_an_auto_closed_brace() {
    let typed = "{:then value}";
    for end in 1..typed.len() {
        let source = [
            "{#await p}\n\t<p>loading</p>\n",
            &typed[..end],
            "}\n\t<p>done</p>\n{/await}\n",
        ]
        .concat();
        // An unfinished keyword (`{:th}`) is still an error; nothing panics.
        let _ = await_shape(&source, true);
    }
    assert_eq!(
        await_shape(
            "{#await p}\n\t<p>loading</p>\n{:then value}\n\t<p>done</p>\n{/await}\n",
            true
        ),
        Ok((true, false, true, false))
    );
}

#[test]
fn strict_parsing_still_rejects_it() {
    assert!(
        await_shape(
            "{#await p}\n\t<p>loading</p>\n{:then }\n\t<p>done</p>\n{/await}\n",
            false
        )
        .is_err()
    );
}
