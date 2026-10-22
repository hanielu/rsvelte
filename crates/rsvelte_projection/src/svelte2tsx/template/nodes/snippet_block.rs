//! `{#snippet}` blocks. Mirrors `htmlxtojsx_v2/nodes/SnippetBlock.ts`.

use crate::ast::template::{Fragment, SnippetBlock, TemplateNode};
use crate::svelte2tsx::magic_string::MagicString;
use crate::svelte2tsx::svelte2tsx::{Svelte2TsxOptions, slice_src};

use crate::svelte2tsx::template::ctx::{Counter, TemplateNodeExt};
use crate::svelte2tsx::template::nodes::special_element::process_fragment_trimmed;
use crate::svelte2tsx::template::utils::expr::{get_expression_range, get_expression_text};
use crate::svelte2tsx::template::utils::source::close_block;

/// Hoist `{#snippet}` blocks to the top of their containing block/element.
///
/// Mirrors `hoistSnippetBlock` in the JS reference
/// (`htmlxtojsx_v2/nodes/SnippetBlock.ts`): each non-leading snippet child is
/// moved to `targetPosition`, the position of the first non-snippet,
/// non-empty-text child. This lets later content reference a snippet defined
/// further down in source (the generated `const foo = ...` declaration is
/// emitted before the `{const}` / `{let}` declaration tags and elements that
/// follow it).
///
/// Snippets that are already first (`targetPosition` still `None`) or already
/// at the target position are left untouched, matching the JS reference's
/// early-`continue` guards. Component / boundary containers are excluded by
/// their callers (they treat snippets as implicit props instead), so this is
/// only invoked for block and plain-element fragments.
pub fn hoist_snippet_blocks(fragment: &Fragment, source: &str, str: &mut MagicString<'_>) {
    let mut target_position: Option<u32> = None;
    for node in &fragment.nodes {
        if !matches!(node, TemplateNode::SnippetBlock(_)) {
            if target_position.is_none() {
                let is_empty_text = match node {
                    TemplateNode::Text(t) => source
                        .get(t.start as usize..t.end as usize)
                        .is_none_or(|s| s.trim().is_empty()),
                    _ => false,
                };
                if !is_empty_text {
                    // JS reference: `node.type === 'Text' ? node.end : node.start`
                    target_position = Some(match node {
                        TemplateNode::Text(t) => t.end,
                        _ => node.start(),
                    });
                }
            }
            continue;
        }

        // It's a snippet block.
        let Some(tp) = target_position else {
            // Already the first meaningful child — nothing to move.
            continue;
        };
        let s = node.start();
        if s == tp {
            continue;
        }
        str.move_range(s, node.end(), tp);
    }
}

/// Handle a snippet block: `{#snippet name(params)}...{/snippet}`.
///
/// Generates:
/// ```text
/// const name = (params): ReturnType<import('svelte').Snippet> => { async () => {
///   ...
/// };return __sveltets_2_any(0)};
/// ```
pub fn handle_snippet_block(
    block: &SnippetBlock,
    source: &str,
    options: &Svelte2TsxOptions,
    str: &mut MagicString<'_>,
    counter: &mut Counter,
    depth: u32,
) {
    handle_snippet_block_inner(block, source, options, str, counter, false, depth);
}

/// Transform a `{#snippet name(params)}` block that is a direct child of a
/// component into an **implicit prop**: `name:(params) => { async () => { …body…
/// };return __sveltets_2_any(0)},`. Unlike the standalone form there is no
/// leading `const`, no `: ReturnType<…>` annotation, and the closing ends in a
/// `,` so the result drops straight into the component's `props: { … }` object
/// literal (the caller relocates the range there via `move_range`). This mirrors
/// upstream svelte2tsx `addImplicitSnippetProp`, and lets TypeScript
/// contextually type the snippet's parameters from the prop's `Snippet<[T]>`
/// type while satisfying required snippet props (#780).
pub fn handle_snippet_block_as_component_prop(
    block: &SnippetBlock,
    source: &str,
    options: &Svelte2TsxOptions,
    str: &mut MagicString<'_>,
    counter: &mut Counter,
    depth: u32,
) {
    handle_snippet_block_inner(block, source, options, str, counter, true, depth);
}

/// The parameter list is emitted as one verbatim source range, so comments,
/// defaults and spacing between the parentheses survive. Upstream anchors the
/// range at the first parameter's first leading comment; the Svelte parser
/// attaches every comment between `(` and that parameter to it, and no comment
/// can precede the `(` (the parser only allows whitespace there), so the first
/// `/` after the `(` opens that comment.
fn params_range(block: &SnippetBlock, source: &str) -> Option<(usize, usize)> {
    let (first_start, _) = block.parameters.first().and_then(get_expression_range)?;
    let (_, last_end) = block.parameters.last().and_then(get_expression_range)?;
    let start = params_open_paren(block, source)
        .and_then(|open| {
            let region = open + 1;
            let upto = (first_start as usize).min(source.len());
            source
                .as_bytes()
                .get(region..upto)
                .and_then(|gap| memchr::memchr(b'/', gap))
                .map(|offset| region + offset)
        })
        .unwrap_or(first_start as usize);
    Some((start, last_end as usize))
}

fn params_text<'a>(block: &SnippetBlock, source: &'a str) -> &'a str {
    match params_range(block, source) {
        Some((start, end)) => slice_src(source, start, end),
        None => "",
    }
}

/// Byte offset of the `(` that opens the snippet's parameter list. Only
/// whitespace and an optional `<…>` type-parameter list separate it from the
/// snippet name, and the type parameters may themselves contain `(`.
fn params_open_paren(block: &SnippetBlock, source: &str) -> Option<usize> {
    let (_, name_end) = get_expression_range(&block.expression)?;
    let bytes = source.as_bytes();
    let mut index = skip_whitespace(bytes, name_end as usize);
    if bytes.get(index) == Some(&b'<') {
        index = skip_whitespace(
            bytes,
            type_params_close(bytes, index).map_or(bytes.len(), |close| close + 1),
        );
    }
    (bytes.get(index) == Some(&b'(')).then_some(index)
}

fn skip_whitespace(bytes: &[u8], mut index: usize) -> usize {
    while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
        index += 1;
    }
    index
}

/// The `>` that closes the type-parameter list opened by the `<` at `open`,
/// skipping quoted strings, as the parser reads it.
fn type_params_close(bytes: &[u8], open: usize) -> Option<usize> {
    let mut index = open;
    let mut depth = 0u32;
    while let Some(&byte) = bytes.get(index) {
        match byte {
            b'\'' | b'"' => {
                index += 1;
                while let Some(&inner) = bytes.get(index) {
                    index += 1;
                    if inner == b'\\' {
                        index += 1;
                    } else if inner == byte {
                        break;
                    }
                }
                continue;
            }
            b'<' => depth += 1,
            b'>' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
        index += 1;
    }
    None
}

/// The snippet's type parameters in the source (`T extends Size` in
/// `{#snippet pair<T extends Size>(…)}`), when they read as the parser read them.
fn type_params_range(block: &SnippetBlock, source: &str) -> Option<(u32, u32)> {
    let type_params = block.type_params.as_deref()?;
    let (_, name_end) = get_expression_range(&block.expression)?;
    let bytes = source.as_bytes();
    let open = skip_whitespace(bytes, name_end as usize);
    if bytes.get(open) != Some(&b'<') {
        return None;
    }
    let close = type_params_close(bytes, open)?;
    let inner = source.get(open + 1..close)?;
    let start = open + 1 + (inner.len() - inner.trim_start().len());
    let end = close - (inner.len() - inner.trim_end().len());
    (start < end && source.get(start..end) == Some(type_params))
        .then(|| Some((u32::try_from(start).ok()?, u32::try_from(end).ok()?)))
        .flatten()
}

/// Upstream reaches the standalone form through `transform()`, which overwrites
/// every non-empty gap *before* a kept source range with one space and may
/// leave one more before the header's `}` — so the count is the number of gaps,
/// not the width of the tail.
fn opener_pad(block: &SnippetBlock, source: &str) -> String {
    let Some((expr_start, expr_end)) = get_expression_range(&block.expression) else {
        return " ".to_string();
    };
    let params = params_range(block, source).filter(|(start, end)| start != end);
    let anchor = params.map_or(expr_end as usize, |(_, end)| end);
    let Some(rel) = source.get(anchor..).and_then(|rest| rest.find('}')) else {
        return " ".to_string();
    };
    let end = anchor + rel + 1;
    // `transform` widens a kept range by one character so the mapping of what
    // follows is not one character short.
    let widen = |(start, stop): (usize, usize)| {
        if stop + 1 < end {
            (start, stop + 1)
        } else {
            (start, stop)
        }
    };
    let mut ranges = Vec::with_capacity(2);
    if expr_start != expr_end {
        ranges.push(widen((expr_start as usize, expr_end as usize)));
    }
    if let Some(range) = params {
        ranges.push(widen(range));
    }
    ranges.sort_unstable();
    let mut remove_start = block.start as usize;
    let mut spaces = 0usize;
    for (start, stop) in ranges {
        if remove_start < start && start < end {
            spaces += 1;
        }
        remove_start = stop;
    }
    // The first character after the last kept range is deleted outright; only
    // what is still left before `}` collapses into a final space.
    if remove_start < end {
        remove_start += 1;
    }
    if remove_start < end {
        spaces += 1;
    }
    " ".repeat(spaces)
}

pub fn handle_snippet_block_inner(
    block: &SnippetBlock,
    source: &str,
    options: &Svelte2TsxOptions,
    str: &mut MagicString<'_>,
    counter: &mut Counter,
    as_component_prop: bool,
    // Snippet bodies always start at depth 0 (official resets `element` on
    // entry), so the inherited depth is intentionally unused.
    _depth: u32,
) {
    if block.start >= block.end {
        return;
    }

    let name_text = get_expression_text(&block.expression, source);

    let params_text = params_text(block, source);

    let has_body_nodes = !block.body.nodes.is_empty();
    let body_start = if has_body_nodes {
        block.body.nodes[0].start()
    } else {
        block.end
    };

    // Overwrite `{#snippet name(params)}` with function declaration.
    // Position markers are added to help the language server:
    // - `/*Ωignore_positionΩ*/` after the name and after `async ()`
    // - Return type wrapped in `/*Ωignore_startΩ*/.../*Ωignore_endΩ*/`
    //
    // Two emission modes match the JS reference (`SnippetBlock.ts`):
    // - TS syntax (TS file or non-JSDoc emit): `: ReturnType<...>` after the
    //   parameter list, with `<typeParams>` if the snippet declared generics
    // - JSDoc syntax (JS file + JSDoc emit): `/** @returns {ReturnType<...>} */`
    //   before the `(params)` arrow, no generic-params syntax
    let use_ts_syntax = options.is_ts_file || !options.emit_jsdoc;
    let type_params_str = match (use_ts_syntax, block.type_params.as_ref()) {
        (true, Some(tp)) => format!("<{tp}>"),
        _ => String::new(),
    };
    // Implicit-prop form (`name:(params) => …`) vs standalone declaration
    // (`const name = (params): ReturnType<…> => …`). The implicit form omits the
    // leading `const`, the return-type annotation, and the generic `<typeParams>`
    // — mirroring upstream's `addImplicitSnippetProp` transforms — and closes
    // with a trailing `,` so it slots into the component `props` object literal.
    //
    // The header is `{lead}{name}{mid_head}{type_params}({params}{tail}`.
    let (lead, mid_head, type_params_text, tail) = if as_component_prop {
        (
            String::new(),
            ":",
            "",
            ") => { async ()/*\u{03A9}ignore_position\u{03A9}*/ => {",
        )
    } else if use_ts_syntax {
        (
            format!("{}const ", opener_pad(block, source)),
            "/*\u{03A9}ignore_position\u{03A9}*/ = ",
            type_params_str.as_str(),
            ")/*\u{03A9}ignore_start\u{03A9}*/: ReturnType<import('svelte').Snippet>/*\u{03A9}ignore_end\u{03A9}*/ => { async ()/*\u{03A9}ignore_position\u{03A9}*/ => {",
        )
    } else {
        (
            format!("{}const ", opener_pad(block, source)),
            "/*\u{03A9}ignore_position\u{03A9}*/ = /** @returns {ReturnType<import('svelte').Snippet>} */ ",
            "",
            ") => { async ()/*\u{03A9}ignore_position\u{03A9}*/ => {",
        )
    };
    let closing = if as_component_prop {
        "};return __sveltets_2_any(0)},"
    } else {
        "};return __sveltets_2_any(0)};"
    };
    // The header ends where the body starts, or with the block when the body is
    // empty, and then also carries the closing.
    let (header_end, header_tail) = if has_body_nodes {
        (body_start, tail.to_string())
    } else {
        (block.end, format!("{tail}{closing}"))
    };
    let keep_type_params = !type_params_text.is_empty();
    match header_in_place(block, source, header_end, keep_type_params) {
        Some(HeaderRanges {
            name: (name_start, name_end),
            type_params,
            params,
        }) => {
            // Upstream overwrites the block's opening around the snippet's name
            // and its parameter list, which keep their own mappings: a rename of
            // the snippet, or of a type its parameters use, lands on the
            // author's text. The type parameters stay in place too (upstream
            // writes them out, so a rename there corrupts the opening).
            str.overwrite(block.start, name_start, &lead);
            let mut cursor = name_end;
            let mut text = mid_head.to_string();
            match type_params {
                Some((start, end)) => {
                    text.push('<');
                    str.overwrite(cursor, start, &text);
                    cursor = end;
                    text = ">(".to_string();
                }
                None => {
                    text.push_str(type_params_text);
                    text.push('(');
                }
            }
            if let Some((start, end)) = params {
                str.overwrite(cursor, start, &text);
                cursor = end;
                text.clear();
            }
            text.push_str(&header_tail);
            str.overwrite(cursor, header_end, &text);
        }
        None => {
            str.overwrite_fmt(
                block.start,
                header_end,
                format_args!(
                    "{lead}{name_text}{mid_head}{type_params_text}({params_text}{header_tail}"
                ),
            );
        }
    }
    // Empty body: the whole `{#snippet name(params)}{/snippet}` is the header,
    // which carries the closing (with a separate overwrite for it, both would
    // land at the same offset and the closing was lost).
    if has_body_nodes {
        // Process body at depth 0: official resets `element = undefined` when
        // entering a SnippetBlock, so element/component names inside a snippet
        // body always count depth from the snippet (e.g. `<Child>` directly in
        // a snippet is `$$_…C0C`), regardless of how deeply the snippet itself
        // is nested in elements / `<svelte:boundary>`. That reset also drops the
        // enclosing component's slot scope, so a `let:`/`slot=` inside the body
        // is a plain attribute rather than a `$$slot_def` consumer.
        let saved_slot = counter.slot_inst.take();
        hoist_snippet_blocks(&block.body, source, str);
        process_fragment_trimmed(&block.body.nodes, source, options, str, counter, 0);
        counter.slot_inst = saved_slot;

        let body_end = block.body.nodes.last().unwrap().end();
        close_block(str, body_end, block.end, closing);
    }
}

/// The ranges in a snippet's opening that the header keeps in place.
struct HeaderRanges {
    name: (u32, u32),
    type_params: Option<(u32, u32)>,
    params: Option<(u32, u32)>,
}

/// The ranges of the snippet's name, its type parameters (when
/// `keep_type_params`) and its parameter list, when the header can be written
/// around them: each lies inside the block's opening, in order, with generated
/// text to go before, between and after them.
fn header_in_place(
    block: &SnippetBlock,
    source: &str,
    header_end: u32,
    keep_type_params: bool,
) -> Option<HeaderRanges> {
    let (name_start, name_end) = get_expression_range(&block.expression)?;
    if !(block.start < name_start && name_start < name_end && name_end < header_end) {
        return None;
    }
    let type_params = type_params_range(block, source)
        .filter(|&(start, end)| keep_type_params && name_end < start && end < header_end);
    let after_type_params = type_params.map_or(name_end, |(_, end)| end);
    let params = match params_range(block, source) {
        Some((start, end)) if start < end => {
            let (start, end) = (u32::try_from(start).ok()?, u32::try_from(end).ok()?);
            if !(after_type_params < start && end < header_end) {
                return None;
            }
            Some((start, end))
        }
        _ => None,
    };
    Some(HeaderRanges {
        name: (name_start, name_end),
        type_params,
        params,
    })
}
