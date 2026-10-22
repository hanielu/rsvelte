//! In loose mode an element's closing tag closes a block left open inside it
//! (`<div>{#if b}x</div>`), as upstream's `close()` does: it pops the block,
//! then the element. rsvelte ended the block but left its entry on the parser
//! stack, so the element's closing tag popped the block's entry instead of its
//! own, and the next continuation of an enclosing block (`{:else}`) was
//! rejected with `block_invalid_continuation_placement`. Deleting one `{/if}`
//! therefore failed the whole loose parse, and the language server lost every
//! template feature until the tag was back.
//!
//! Expected shapes: Svelte 5.56.8 `parse(source, { modern: true, loose: true })`.

use rsvelte_core::ast::template::{Root, TemplateNode};
use rsvelte_core::{ParseOptions, parse};

fn loose(source: &str) -> String {
    let allocator = oxc_allocator::Allocator::default();
    let options = ParseOptions {
        modern: true,
        loose: true,
        ..Default::default()
    };
    let root: Root = parse(source, &allocator, options).expect("loose parsing recovers");
    let mut out = String::new();
    walk(&root.fragment.nodes, 0, &mut out);
    out
}

fn walk(nodes: &[TemplateNode], depth: usize, out: &mut String) {
    let pad = "  ".repeat(depth);
    for node in nodes {
        match node {
            TemplateNode::RegularElement(e) => {
                out.push_str(&format!("{pad}{} {}-{}\n", e.name, e.start, e.end));
                walk(&e.fragment.nodes, depth + 1, out);
            }
            TemplateNode::IfBlock(b) => {
                out.push_str(&format!("{pad}if {}-{}\n", b.start, b.end));
                walk(&b.consequent.nodes, depth + 1, out);
                if let Some(alternate) = &b.alternate {
                    out.push_str(&format!("{pad}else\n"));
                    walk(&alternate.nodes, depth + 1, out);
                }
            }
            TemplateNode::EachBlock(b) => {
                out.push_str(&format!("{pad}each {}-{}\n", b.start, b.end));
                walk(&b.body.nodes, depth + 1, out);
            }
            TemplateNode::Text(t) if !t.data.trim().is_empty() => {
                out.push_str(&format!("{pad}{:?}\n", t.data.trim()));
            }
            _ => {}
        }
    }
}

#[test]
fn an_if_else_closed_by_its_element_before_the_outer_else() {
    assert_eq!(
        loose("{#if a}\n<div>\n{#if b}\nx\n{:else}\ny\n</div>\n{:else}\nz\n{/if}\n"),
        "if 0-56\n  div 8-40\n    if 14-34\n      \"x\"\n    else\n      \"y\"\nelse\n  \"z\"\n"
    );
}

#[test]
fn an_if_closed_by_its_element_before_the_outer_else() {
    assert_eq!(
        loose("{#if a}\n<div>\n{#if b}\nx\n</div>\n{:else}\nz\n{/if}\n"),
        "if 0-46\n  div 8-30\n    if 14-24\n      \"x\"\nelse\n  \"z\"\n"
    );
}

#[test]
fn an_each_closed_by_its_element_before_the_outer_else() {
    assert_eq!(
        loose("{#if a}\n<div>\n{#each xs as x}\nx\n</div>\n{:else}\nz\n{/if}\n"),
        "if 0-54\n  div 8-38\n    each 14-32\n      \"x\"\nelse\n  \"z\"\n"
    );
}

#[test]
fn an_if_closed_by_its_element_inside_an_each() {
    assert_eq!(
        loose("{#each xs as x}\n<li>\n{#if x}\nx\n</li>\n{/each}\n"),
        "each 0-44\n  li 16-36\n    if 21-31\n      \"x\"\n"
    );
}
