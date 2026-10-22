//! An opening tag still being typed above existing markup (`<div class="x"`
//! before `<p>{a}</p>`) reads the next opening tag as one of its attributes
//! (`<p`), so the next closing tag (`</p>`) closes nothing that is open.
//! Upstream's loose parser (`element.js`, "If the previous element did
//! interpret the next opening tag as an attribute, backtrack") drops that
//! attribute, ends the unfinished element where it started, and reads the tag
//! again as a sibling; rsvelte rejected the document, so the language server
//! lost every template feature while the author typed. Upstream also leaves
//! the children read past the tag in the unfinished element, where they then
//! belong to two elements; rsvelte reads them only as the sibling's.
//!
//! Expected shapes: Svelte 5.56.8 `parse(source, { modern: true, loose: true })`.

use rsvelte_core::ast::template::{Attribute, Root, TemplateNode};
use rsvelte_core::{ParseOptions, parse};

fn loose(source: &str) -> String {
    let allocator = oxc_allocator::Allocator::default();
    let options = ParseOptions {
        modern: true,
        loose: true,
        ..Default::default()
    };
    let root = parse(source, &allocator, options).expect("loose parsing recovers");
    shape(&root)
}

/// Elements and their attributes with their spans, one per line.
fn shape(root: &Root) -> String {
    let mut out = String::new();
    walk(&root.fragment.nodes, 0, &mut out);
    out
}

fn walk(nodes: &[TemplateNode], depth: usize, out: &mut String) {
    for node in nodes {
        let (kind, name, start, end, attributes, children) = match node {
            TemplateNode::RegularElement(e) => (
                "element",
                e.name.as_str(),
                e.start,
                e.end,
                &e.attributes,
                &e.fragment.nodes,
            ),
            TemplateNode::Component(c) => (
                "component",
                c.name.as_str(),
                c.start,
                c.end,
                &c.attributes,
                &c.fragment.nodes,
            ),
            TemplateNode::ExpressionTag(t) => {
                out.push_str(&format!(
                    "{}{{}} {}-{}\n",
                    "  ".repeat(depth),
                    t.start,
                    t.end
                ));
                continue;
            }
            _ => continue,
        };
        out.push_str(&format!(
            "{}{kind} {name} {start}-{end}",
            "  ".repeat(depth)
        ));
        for attribute in attributes {
            if let Attribute::Attribute(a) = attribute {
                out.push_str(&format!(" [{}]", a.name));
            }
        }
        out.push('\n');
        walk(children, depth + 1, out);
    }
}

#[test]
fn a_bare_opening_tag_above_an_element() {
    assert_eq!(
        loose("<div\n<p>{a}</p>\n"),
        "element div 0-5\nelement p 5-15\n  {} 8-11\n"
    );
}

#[test]
fn an_opening_tag_with_attributes_above_an_element() {
    assert_eq!(
        loose("<div class=\"x\"\n<p>{a}</p>\n"),
        "element div 0-15 [class]\nelement p 15-25\n  {} 18-21\n"
    );
}

#[test]
fn a_component_tag_above_an_element() {
    assert_eq!(
        loose("<Child foo={a}\n<p>{a}</p>\n"),
        "component Child 0-15 [foo]\nelement p 15-25\n  {} 18-21\n"
    );
}

#[test]
fn an_opening_tag_inside_an_element() {
    assert_eq!(
        loose("<section>\n<div class=\"a\" id=\"b\"\n<p>{x}</p>\n<span>y</span>\n</section>\n"),
        "element section 0-68\n  element div 10-32 [class] [id]\n  element p 32-42\n    {} 35-38\n  element span 43-57\n"
    );
}

#[test]
fn a_tag_of_the_same_name_closes_the_unfinished_one_as_upstream_does() {
    // `</div>` matches the unfinished `<div`, so nothing backtracks.
    assert_eq!(loose("<div\n<div>x</div>\n"), "element div 0-17 [<div]\n");
}

#[test]
fn strict_parsing_still_rejects_it() {
    let allocator = oxc_allocator::Allocator::default();
    assert!(parse("<div\n<p>{a}</p>\n", &allocator, ParseOptions::default()).is_err());
}
