//! Svelte-specific normalization for tsgo code actions.

use std::path::Path;

use lsp_types::{Position, Range, Uri};
use rsvelte_core::{Allocator, ParseOptions, parse};
use serde_json::{Map, Value, json};

use crate::context::{EmbeddedRegions, possibly_component, script_bodies};
use crate::tsgo_completion::{ImportPlacement, is_new_import_edit, place_new_import_edit};
use crate::tsgo_overlay::TsgoOverlay;

use crate::text::LineIndex;

pub const SORT_IMPORTS_KIND: &str = "source.sortImports";
pub const ADD_MISSING_IMPORTS_KIND: &str = "source.addMissingImports";
pub const REMOVE_UNUSED_IMPORTS_KIND: &str = "source.removeUnusedImports";

const QUICK_FIX_KIND: &str = "quickfix";
const SOURCE_KIND: &str = "source";
const ORGANIZE_IMPORTS_KIND: &str = "source.organizeImports";
const FIX_ALL_KIND: &str = "source.fixAll";
const COMPONENT_SUFFIX: &str = "__SvelteComponent_";
const GENERATED_HELPERS: &[&str] = &[
    "__sveltets_",
    "__SvelteComponentTyped__",
    "/*Ωignore_startΩ*/",
    "/*Ωignore_endΩ*/",
];

/// Source state needed to normalize one code-action response.
pub struct TsgoCodeActionContext<'a> {
    pub source_uri: &'a Uri,
    pub source: &'a str,
    pub parser_error: bool,
    pub default_script_language: Option<&'a str>,
    pub diagnostic_codes: &'a [u32],
}

impl<'a> TsgoCodeActionContext<'a> {
    #[must_use]
    pub const fn new(source_uri: &'a Uri, source: &'a str) -> Self {
        Self {
            source_uri,
            source,
            parser_error: false,
            default_script_language: None,
            diagnostic_codes: &[],
        }
    }

    #[must_use]
    pub const fn with_parser_error(mut self, parser_error: bool) -> Self {
        self.parser_error = parser_error;
        self
    }

    #[must_use]
    pub const fn with_default_script_language(mut self, language: Option<&'a str>) -> Self {
        self.default_script_language = language;
        self
    }

    #[must_use]
    pub const fn with_diagnostic_codes(mut self, diagnostic_codes: &'a [u32]) -> Self {
        self.diagnostic_codes = diagnostic_codes;
        self
    }
}

/// Whether a kind is implemented by the tsgo-backed Svelte action path.
#[must_use]
pub fn is_supported_code_action_kind(kind: &str) -> bool {
    matches!(
        kind,
        QUICK_FIX_KIND
            | SOURCE_KIND
            | ORGANIZE_IMPORTS_KIND
            | SORT_IMPORTS_KIND
            | ADD_MISSING_IMPORTS_KIND
            | REMOVE_UNUSED_IMPORTS_KIND
            | FIX_ALL_KIND
    )
}

#[must_use]
pub fn is_source_code_action_kind(kind: &str) -> bool {
    kind == SOURCE_KIND || kind.starts_with("source.")
}

#[must_use]
pub fn is_organize_code_action_kind(kind: &str) -> bool {
    matches!(
        kind,
        ORGANIZE_IMPORTS_KIND | SORT_IMPORTS_KIND | REMOVE_UNUSED_IMPORTS_KIND
    )
}

#[must_use]
pub fn document_has_script(source: &str) -> bool {
    !script_regions(source).is_empty()
}

#[must_use]
pub fn document_has_parser_error(source: &str) -> bool {
    let allocator = Allocator::default();
    parse(
        source,
        &allocator,
        ParseOptions {
            modern: true,
            defer_script_parse: false,
            skip_non_css_lang_style: true,
            ..ParseOptions::default()
        },
    )
    .is_err()
}

/// Decide before forwarding whether the requested `only` set can produce a
/// safe action for this document.
#[must_use]
pub fn should_forward_code_action_request(
    only: Option<&[String]>,
    parser_error: bool,
    has_script: bool,
) -> bool {
    let Some(only) = only else {
        return true;
    };
    if only.is_empty() {
        return true;
    }
    only.iter().any(|kind| {
        is_supported_code_action_kind(kind)
            && !(parser_error && is_source_code_action_kind(kind))
            && (has_script || !is_organize_code_action_kind(kind))
    })
}

/// Normalize a tsgo `textDocument/codeAction` result after shadow mapping.
///
/// Returns the number of actions left in the response.
pub fn rewrite_code_action_response(
    value: &mut Value,
    context: &TsgoCodeActionContext<'_>,
) -> usize {
    let scripts = script_regions(context.source);
    let Some(actions) = code_actions_mut(value) else {
        return 0;
    };

    let mut normalized = Vec::with_capacity(actions.len() + 1);
    for mut action in std::mem::take(actions) {
        if normalize_action(&mut action, context, &scripts) {
            normalized.push(action);
        }
    }
    if needs_lang_ts_action(context)
        && let Some(action) = create_add_lang_ts_action(context, &scripts)
    {
        normalized.insert(0, action);
    }
    *actions = normalized;
    actions.len()
}

fn code_actions_mut(value: &mut Value) -> Option<&mut Vec<Value>> {
    match value {
        Value::Array(actions) => Some(actions),
        Value::Object(object) => object.get_mut("items")?.as_array_mut(),
        _ => None,
    }
}

fn normalize_action(
    action: &mut Value,
    context: &TsgoCodeActionContext<'_>,
    scripts: &[ScriptRegion],
) -> bool {
    let Some(object) = action.as_object_mut() else {
        return false;
    };
    let kind = object
        .get("kind")
        .and_then(Value::as_str)
        .map(str::to_string);
    if kind
        .as_deref()
        .is_some_and(|kind| !is_supported_code_action_kind(kind))
    {
        return false;
    }
    if context.parser_error && kind.as_deref().is_some_and(is_source_code_action_kind) {
        return false;
    }
    if scripts.is_empty() && kind.as_deref().is_some_and(is_organize_code_action_kind) {
        return false;
    }

    let import_action = is_import_action(object);
    rewrite_visible_fields(object);
    let stats = normalize_workspace_edit(object, context, scripts, kind.as_deref());

    if import_action && scripts.is_empty() {
        wrap_import_edits_in_script(object, context);
    }

    let final_stats = workspace_edit_stats(object);
    if stats.had_edits && final_stats.kept_edits == 0 {
        return false;
    }
    if kind.as_deref().is_some_and(is_organize_code_action_kind)
        && current_document_edit_count(object, context.source_uri) == 0
    {
        return false;
    }
    true
}

fn rewrite_visible_fields(object: &mut Map<String, Value>) {
    for (key, value) in object {
        if key == "data" || key == "command" || is_uri_key(key) {
            continue;
        }
        rewrite_visible_value(value);
    }
}

fn rewrite_visible_value(value: &mut Value) {
    match value {
        Value::String(text) => rewrite_visible_text(text),
        Value::Array(items) => {
            for item in items {
                rewrite_visible_value(item);
            }
        }
        Value::Object(object) => rewrite_visible_fields(object),
        _ => {}
    }
}

fn rewrite_visible_text(text: &mut String) {
    let had_component_suffix = text.contains(COMPONENT_SUFFIX);
    if had_component_suffix {
        *text = text.replace(COMPONENT_SUFFIX, "");
    }
    if text.contains(".svelte.tsx") {
        *text = text.replace(".svelte.tsx", ".svelte");
    }
    if had_component_suffix {
        *text = text
            .replace("import type ", "import ")
            .replace(" type ", " ");
    }
}

fn is_uri_key(key: &str) -> bool {
    matches!(
        key,
        "uri" | "documentUri" | "targetUri" | "oldUri" | "newUri"
    )
}

#[derive(Default)]
struct EditStats {
    had_edits: bool,
    kept_edits: usize,
}

fn normalize_workspace_edit(
    action: &mut Map<String, Value>,
    context: &TsgoCodeActionContext<'_>,
    scripts: &[ScriptRegion],
    kind: Option<&str>,
) -> EditStats {
    let mut stats = EditStats::default();
    let Some(edit) = action.get_mut("edit").and_then(Value::as_object_mut) else {
        return stats;
    };
    let protect_script = kind.is_some_and(is_source_code_action_kind);

    if let Some(changes) = edit.get_mut("changes").and_then(Value::as_object_mut) {
        for (uri, edits) in changes {
            let is_current = uri == context.source_uri.as_str();
            normalize_edit_array(
                edits,
                is_current,
                protect_script,
                context.source,
                scripts,
                &mut stats,
            );
        }
    }

    if let Some(changes) = edit
        .get_mut("documentChanges")
        .and_then(Value::as_array_mut)
    {
        for change in changes {
            let Some(change) = change.as_object_mut() else {
                continue;
            };
            let uri = change
                .get("textDocument")
                .and_then(Value::as_object)
                .and_then(|document| document.get("uri"))
                .and_then(Value::as_str);
            let is_current = uri == Some(context.source_uri.as_str());
            if let Some(edits) = change.get_mut("edits") {
                normalize_edit_array(
                    edits,
                    is_current,
                    protect_script,
                    context.source,
                    scripts,
                    &mut stats,
                );
            }
        }
    }
    stats
}

fn normalize_edit_array(
    value: &mut Value,
    is_current: bool,
    protect_script: bool,
    source: &str,
    scripts: &[ScriptRegion],
    stats: &mut EditStats,
) {
    let Some(edits) = value.as_array_mut() else {
        return;
    };
    stats.had_edits |= !edits.is_empty();
    let index = LineIndex::new(source);
    edits.retain_mut(|edit| {
        let Some(edit) = edit.as_object_mut() else {
            return false;
        };
        if !strip_generated_helper_text(edit) {
            return false;
        }
        if is_current && protect_script && !edit_range_in_scripts(edit, source, &index, scripts) {
            return false;
        }
        stats.kept_edits += 1;
        true
    });
}

fn strip_generated_helper_text(edit: &mut Map<String, Value>) -> bool {
    let Some(text) = edit.get("newText").and_then(Value::as_str) else {
        return true;
    };
    if !GENERATED_HELPERS.iter().any(|helper| text.contains(helper)) {
        return true;
    }
    let stripped = text
        .split_inclusive('\n')
        .filter(|line| !GENERATED_HELPERS.iter().any(|helper| line.contains(helper)))
        .collect::<String>();
    if stripped.is_empty() {
        return false;
    }
    edit.insert("newText".to_string(), Value::String(stripped));
    true
}

fn edit_range_in_scripts(
    edit: &Map<String, Value>,
    source: &str,
    index: &LineIndex,
    scripts: &[ScriptRegion],
) -> bool {
    let Some(range) = edit.get("range").and_then(parse_range) else {
        return false;
    };
    let start = index.offset(source, range.start);
    let end = index.offset(source, range.end).max(start);
    scripts
        .iter()
        .any(|script| script.content_start <= start && end <= script.content_end)
}

fn workspace_edit_stats(action: &Map<String, Value>) -> EditStats {
    let mut stats = EditStats::default();
    let Some(edit) = action.get("edit").and_then(Value::as_object) else {
        return stats;
    };
    if let Some(changes) = edit.get("changes").and_then(Value::as_object) {
        for edits in changes.values() {
            let count = edits.as_array().map_or(0, Vec::len);
            stats.kept_edits += count;
            if count > 0 {
                stats.had_edits = true;
            }
        }
    }
    if let Some(changes) = edit.get("documentChanges").and_then(Value::as_array) {
        for change in changes {
            let Some(change) = change.as_object() else {
                continue;
            };
            let count = change
                .get("edits")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            stats.kept_edits += count;
            stats.had_edits |= count > 0;
        }
    }
    stats
}

fn current_document_edit_count(action: &Map<String, Value>, source_uri: &Uri) -> usize {
    let Some(edit) = action.get("edit").and_then(Value::as_object) else {
        return 0;
    };
    let changes = edit
        .get("changes")
        .and_then(Value::as_object)
        .and_then(|changes| changes.get(source_uri.as_str()))
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let document_changes = edit
        .get("documentChanges")
        .and_then(Value::as_array)
        .map_or(0, |changes| {
            changes
                .iter()
                .filter_map(Value::as_object)
                .filter(|change| {
                    change
                        .get("textDocument")
                        .and_then(Value::as_object)
                        .and_then(|document| document.get("uri"))
                        .and_then(Value::as_str)
                        == Some(source_uri.as_str())
                })
                .filter_map(|change| change.get("edits").and_then(Value::as_array))
                .map(Vec::len)
                .sum()
        });
    changes + document_changes
}

fn is_import_action(action: &Map<String, Value>) -> bool {
    if action
        .get("data")
        .and_then(Value::as_object)
        .is_some_and(|data| {
            data.get("fixName").and_then(Value::as_str) == Some("import")
                || data.get("fixId").and_then(Value::as_str) == Some("fixMissingImport")
        })
    {
        return true;
    }
    if action
        .get("title")
        .and_then(Value::as_str)
        .is_some_and(|title| title.to_ascii_lowercase().contains("import"))
    {
        return true;
    }
    contains_import_edit(action.get("edit"))
}

fn contains_import_edit(value: Option<&Value>) -> bool {
    let Some(value) = value else {
        return false;
    };
    match value {
        Value::String(text) => text.trim_start().starts_with("import "),
        Value::Array(items) => items.iter().any(|item| contains_import_edit(Some(item))),
        Value::Object(object) => object
            .iter()
            .any(|(key, value)| key != "data" && contains_import_edit(Some(value))),
        _ => false,
    }
}

fn wrap_import_edits_in_script(
    action: &mut Map<String, Value>,
    context: &TsgoCodeActionContext<'_>,
) {
    let mut imports = String::new();
    let line_ending = newline(context.source);
    for_each_current_edit_array(action, context.source_uri, &mut |edits| {
        for edit in edits.iter() {
            if let Some(text) = edit.get("newText").and_then(Value::as_str) {
                imports.push_str(text.trim_start_matches(['\r', '\n']));
                if !imports.ends_with(['\r', '\n']) {
                    imports.push_str(line_ending);
                }
            }
        }
        edits.clear();
    });
    if imports.is_empty() {
        return;
    }
    let language = match context.default_script_language {
        Some(language @ ("js" | "ts")) => format!(" lang=\"{language}\""),
        _ => String::new(),
    };
    let newline = newline(context.source);
    let text = format!("<script{language}>{newline}{imports}</script>{newline}");
    let insertion = json!({
        "range": zero_range(),
        "newText": text
    });
    insert_current_edit(action, context.source_uri, insertion);
}

fn for_each_current_edit_array(
    action: &mut Map<String, Value>,
    source_uri: &Uri,
    callback: &mut impl FnMut(&mut Vec<Value>),
) {
    let Some(edit) = action.get_mut("edit").and_then(Value::as_object_mut) else {
        return;
    };
    if let Some(edits) = edit
        .get_mut("changes")
        .and_then(Value::as_object_mut)
        .and_then(|changes| changes.get_mut(source_uri.as_str()))
        .and_then(Value::as_array_mut)
    {
        callback(edits);
    }
    if let Some(changes) = edit
        .get_mut("documentChanges")
        .and_then(Value::as_array_mut)
    {
        for change in changes {
            let Some(change) = change.as_object_mut() else {
                continue;
            };
            let is_current = change
                .get("textDocument")
                .and_then(Value::as_object)
                .and_then(|document| document.get("uri"))
                .and_then(Value::as_str)
                == Some(source_uri.as_str());
            if is_current && let Some(edits) = change.get_mut("edits").and_then(Value::as_array_mut)
            {
                callback(edits);
            }
        }
    }
}

fn insert_current_edit(action: &mut Map<String, Value>, source_uri: &Uri, insertion: Value) {
    let Some(edit) = action.get_mut("edit").and_then(Value::as_object_mut) else {
        return;
    };
    if let Some(edits) = edit
        .get_mut("changes")
        .and_then(Value::as_object_mut)
        .and_then(|changes| changes.get_mut(source_uri.as_str()))
        .and_then(Value::as_array_mut)
    {
        edits.push(insertion);
        return;
    }
    if let Some(changes) = edit
        .get_mut("documentChanges")
        .and_then(Value::as_array_mut)
    {
        for change in changes {
            let Some(change) = change.as_object_mut() else {
                continue;
            };
            let is_current = change
                .get("textDocument")
                .and_then(Value::as_object)
                .and_then(|document| document.get("uri"))
                .and_then(Value::as_str)
                == Some(source_uri.as_str());
            if is_current && let Some(edits) = change.get_mut("edits").and_then(Value::as_array_mut)
            {
                edits.push(insertion);
                return;
            }
        }
    }
}

fn needs_lang_ts_action(context: &TsgoCodeActionContext<'_>) -> bool {
    context
        .diagnostic_codes
        .iter()
        .any(|code| (8004..=8017).contains(code))
}

fn create_add_lang_ts_action(
    context: &TsgoCodeActionContext<'_>,
    scripts: &[ScriptRegion],
) -> Option<Value> {
    let newline = newline(context.source);
    let edits = if scripts.is_empty() {
        vec![json!({
            "range": zero_range(),
            "newText": format!("<script lang=\"ts\"></script>{newline}")
        })]
    } else {
        let index = LineIndex::new(context.source);
        scripts
            .iter()
            .filter(|script| !script.has_lang)
            .map(|script| {
                let position = index.position(context.source, script.tag_name_end);
                json!({
                    "range": {
                        "start": position,
                        "end": position
                    },
                    "newText": " lang=\"ts\""
                })
            })
            .collect()
    };
    if edits.is_empty() {
        return None;
    }
    let title = if scripts.is_empty() {
        "Add <script lang=\"ts\"> tag"
    } else {
        "Add lang=\"ts\" to <script> tag"
    };
    Some(json!({
        "title": title,
        "kind": QUICK_FIX_KIND,
        "edit": {
            "documentChanges": [{
                "textDocument": {
                    "uri": context.source_uri.as_str(),
                    "version": null
                },
                "edits": edits
            }]
        }
    }))
}

#[derive(Clone, Copy, Debug)]
struct ScriptRegion {
    tag_name_end: usize,
    content_start: usize,
    content_end: usize,
    has_lang: bool,
}

fn script_regions(source: &str) -> Vec<ScriptRegion> {
    let allocator = Allocator::default();
    let options = ParseOptions {
        modern: true,
        loose: true,
        defer_script_parse: true,
        lenient_script: true,
        skip_non_css_lang_style: true,
        ..ParseOptions::default()
    };
    if let Ok(root) = parse(source, &allocator, options) {
        let mut regions = [root.module.as_deref(), root.instance.as_deref()]
            .into_iter()
            .flatten()
            .map(|script| {
                let tag_name_end = script.start as usize + "<script".len();
                let content_start = script.content_offset as usize;
                ScriptRegion {
                    tag_name_end,
                    content_start,
                    content_end: content_start + script.raw_content.len(),
                    has_lang: has_lang_attribute(&source[tag_name_end..content_start]),
                }
            })
            .collect::<Vec<_>>();
        regions.sort_by_key(|script| script.tag_name_end);
        return regions;
    }
    scan_script_regions(source)
}

fn scan_script_regions(source: &str) -> Vec<ScriptRegion> {
    let mut regions = Vec::new();
    let mut search = 0;
    while let Some(relative) = source[search..].find("<script") {
        let tag_start = search + relative;
        let tag_name_end = tag_start + "<script".len();
        if source[tag_name_end..]
            .chars()
            .next()
            .is_some_and(|character| !character.is_ascii_whitespace() && character != '>')
        {
            search = tag_name_end;
            continue;
        }
        let Some(content_start) = find_tag_end(source, tag_name_end) else {
            break;
        };
        let Some(relative_end) = source[content_start..].find("</script") else {
            break;
        };
        let content_end = content_start + relative_end;
        let opening_tag = &source[tag_name_end..content_start];
        regions.push(ScriptRegion {
            tag_name_end,
            content_start,
            content_end,
            has_lang: has_lang_attribute(opening_tag),
        });
        search = content_end + "</script".len();
    }
    regions
}

fn find_tag_end(source: &str, mut offset: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut quote = None;
    while offset < bytes.len() {
        let byte = bytes[offset];
        match quote {
            Some(current) if byte == current => quote = None,
            None if byte == b'\'' || byte == b'"' => quote = Some(byte),
            None if byte == b'>' => return Some(offset + 1),
            _ => {}
        }
        offset += 1;
    }
    None
}

fn has_lang_attribute(opening_tag: &str) -> bool {
    opening_tag
        .split_ascii_whitespace()
        .any(|attribute| attribute == "lang" || attribute.starts_with("lang="))
}

fn parse_range(value: &Value) -> Option<Range> {
    let object = value.as_object()?;
    Some(Range::new(
        parse_position(object.get("start")?)?,
        parse_position(object.get("end")?)?,
    ))
}

fn parse_position(value: &Value) -> Option<Position> {
    let object = value.as_object()?;
    Some(Position::new(
        u32::try_from(object.get("line")?.as_u64()?).ok()?,
        u32::try_from(object.get("character")?.as_u64()?).ok()?,
    ))
}

fn zero_range() -> Value {
    json!({
        "start": { "line": 0, "character": 0 },
        "end": { "line": 0, "character": 0 }
    })
}

fn newline(source: &str) -> &'static str {
    if source.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

/// Marker a code action carries while its new imports are held out of the
/// mapping ([`take_new_imports`]); its value indexes the held edits.
const NEW_IMPORTS_KEY: &str = "__rsvelte_new_imports";

/// [`crate::tsgo_completion::take_new_import_edits`] for a code-action
/// response: upstream places an import fix's edits with the same function
/// (`CodeActionsProvider.convertAndFixCodeFixAction` calls
/// `codeActionChangeToTextEdit`). Each action's new-import edits in the shadow
/// are taken out, placed in source coordinates and held; the action keeps a
/// marker [`restore_new_imports`] puts them back by after the mapping. A
/// component without a `<script>` is left to [`wrap_import_edits_in_script`].
pub fn take_new_imports(
    value: &mut Value,
    overlay: &TsgoOverlay,
    shadow_path: &Path,
    shadow_uri: &str,
    placement: &ImportPlacement<'_>,
) -> Vec<Vec<Value>> {
    let bodies = script_bodies(placement.source);
    if bodies.instance.is_none() && bodies.module.is_none() {
        return Vec::new();
    }
    let Some(actions) = code_actions_mut(value) else {
        return Vec::new();
    };
    let mut held = Vec::new();
    for action in actions {
        let mut imports = Vec::new();
        let mut take = |edits: &mut Value| {
            if let Some(edits) = edits.as_array_mut() {
                let (new, rest): (Vec<Value>, Vec<Value>) = std::mem::take(edits)
                    .into_iter()
                    .partition(is_new_import_edit);
                *edits = rest;
                imports.extend(new);
            }
        };
        let Some(edit) = action.get_mut("edit").and_then(Value::as_object_mut) else {
            continue;
        };
        if let Some(edits) = edit
            .get_mut("changes")
            .and_then(Value::as_object_mut)
            .and_then(|changes| changes.get_mut(shadow_uri))
        {
            take(edits);
        }
        for change in edit
            .get_mut("documentChanges")
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
        {
            if change.pointer("/textDocument/uri").and_then(Value::as_str) == Some(shadow_uri)
                && let Some(edits) = change.get_mut("edits")
            {
                take(edits);
            }
        }
        if imports.is_empty() {
            continue;
        }
        action[NEW_IMPORTS_KEY] = json!(held.len());
        held.push(
            imports
                .into_iter()
                .filter_map(|edit| place_new_import_edit(overlay, shadow_path, placement, edit))
                .collect(),
        );
    }
    held
}

/// Put the edits [`take_new_imports`] held back into their actions, now in
/// the source document `source_uri`.
pub fn restore_new_imports(value: &mut Value, source_uri: &str, mut held: Vec<Vec<Value>>) {
    let Some(actions) = code_actions_mut(value) else {
        return;
    };
    for action in actions {
        let Some(object) = action.as_object_mut() else {
            continue;
        };
        let Some(index) = object
            .remove(NEW_IMPORTS_KEY)
            .and_then(|index| index.as_u64())
            .and_then(|index| usize::try_from(index).ok())
        else {
            continue;
        };
        let Some(edits) = held.get_mut(index).map(std::mem::take) else {
            continue;
        };
        let edit = object.entry("edit").or_insert_with(|| json!({}));
        if let Some(changes) = edit
            .get_mut("documentChanges")
            .and_then(Value::as_array_mut)
        {
            match changes.iter_mut().find(|change| {
                change.pointer("/textDocument/uri").and_then(Value::as_str) == Some(source_uri)
            }) {
                Some(change) => {
                    if let Some(list) = change.get_mut("edits").and_then(Value::as_array_mut) {
                        list.extend(edits);
                    }
                }
                None => changes.push(json!({
                    "textDocument": { "uri": source_uri, "version": null },
                    "edits": edits,
                })),
            }
        } else if let Some(edit) = edit.as_object_mut() {
            let changes = edit.entry("changes").or_insert_with(|| json!({}));
            if let Some(changes) = changes.as_object_mut() {
                let list = changes
                    .entry(source_uri.to_string())
                    .or_insert_with(|| json!([]));
                if let Some(list) = list.as_array_mut() {
                    list.extend(edits);
                }
            }
        }
    }
}

/// `Cannot find name` and its "did you mean" variant.
const CANNOT_FIND_NAME: [u64; 2] = [2304, 2552];
/// The svelte2tsx helper a component's tag name is the argument of.
const ENSURE_COMPONENT: &str = "__sveltets_2_ensureComponent(";

/// A `Cannot find name` diagnostic on a component's tag name in the markup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComponentImportSite {
    /// The tag name, as written.
    pub name: String,
    /// The end of the tag name in the shadow (tsgo's encoding), where its
    /// completions are asked for.
    pub position: Position,
}

/// The component tags a code-action request's `Cannot find name` diagnostics
/// are on (`CodeActionsProvider.getComponentImportQuickFix`, `:1016-1061`).
///
/// TypeScript's import fix needs the missing name to be the export's own, and
/// a component's default export carries svelte2tsx's suffix
/// (`Widget__SvelteComponent_`), so upstream asks for auto-import completions
/// at the tag name instead. Like upstream, a diagnostic in a script, or on a
/// name that cannot be a component, is skipped, and one whose identifier is
/// not a tag name (not `__sveltets_2_ensureComponent`'s argument) means no
/// site at all, so TypeScript's own fixes answer.
#[must_use]
pub fn component_import_sites(
    params: &Value,
    source: &str,
    overlay: &TsgoOverlay,
    source_path: &Path,
    shadow_path: &Path,
) -> Vec<ComponentImportSite> {
    let Some(diagnostics) = params
        .pointer("/context/diagnostics")
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    let (Some(shadow), index) = (overlay.shadow_text(shadow_path), LineIndex::new(source)) else {
        return Vec::new();
    };
    let regions = EmbeddedRegions::new(source);
    let mut sites: Vec<ComponentImportSite> = Vec::new();
    for diagnostic in diagnostics {
        let cannot_find = diagnostic
            .get("code")
            .and_then(Value::as_u64)
            .is_some_and(|code| CANNOT_FIND_NAME.contains(&code));
        let Some(range) = diagnostic.get("range").and_then(parse_range) else {
            continue;
        };
        if !cannot_find {
            continue;
        }
        let start = index.offset(source, range.start);
        let end = index.offset(source, range.end);
        let Some(name) = source.get(start..end) else {
            continue;
        };
        if regions.in_script(start)
            || !possibly_component(name)
            || sites.iter().any(|site| site.name == name)
        {
            continue;
        }
        let tag_name = overlay
            .map_source_range(source_path, range)
            .and_then(|shadow_range| {
                let start = overlay.shadow_offset(shadow_path, shadow_range.start)?;
                let end = overlay.shadow_offset(shadow_path, shadow_range.end)?;
                (shadow.get(start..end) == Some(name)
                    && shadow.get(..start)?.ends_with(ENSURE_COMPONENT))
                .then_some(shadow_range.end)
            });
        let Some(position) = tag_name else {
            return Vec::new();
        };
        sites.push(ComponentImportSite {
            name: name.to_string(),
            position,
        });
    }
    sites
}

/// The completion items that import the component `name`: tsgo labels a
/// component's default export with svelte2tsx's suffix.
#[must_use]
pub fn component_import_candidates(completion: &Value, name: &str) -> Vec<Value> {
    let items = match completion {
        Value::Array(items) => items,
        other => match other.get("items").and_then(Value::as_array) {
            Some(items) => items,
            None => return Vec::new(),
        },
    };
    let suffixed = format!("{name}{COMPONENT_SUFFIX}");
    items
        .iter()
        .filter(|item| {
            item.get("label")
                .and_then(Value::as_str)
                .is_some_and(|label| label == name || label == suffixed)
        })
        .cloned()
        .collect()
}

/// The quick fix a resolved component completion makes: its import edits in
/// the shadow, titled like TypeScript's `Add import from "…"`.
#[must_use]
pub fn component_import_action(resolved: &Value, shadow_uri: &str) -> Option<Value> {
    let edits = resolved.get("additionalTextEdits")?.as_array()?;
    let import = edits
        .iter()
        .filter_map(|edit| edit.get("newText")?.as_str())
        .find(|text| text.contains("import "))?;
    let title = resolved
        .get("detail")
        .and_then(Value::as_str)
        .filter(|detail| detail.starts_with("Add import from"))
        .map(str::to_string)
        .or_else(|| {
            let from = import.find(" from ")? + " from ".len();
            let quoted = import[from..].trim_start();
            let quote = quoted.chars().next()?;
            let module = quoted.get(1..)?.split(quote).next()?;
            Some(format!("Add import from \"{module}\""))
        })?;
    Some(json!({
        "title": title,
        "kind": QUICK_FIX_KIND,
        "edit": { "changes": { shadow_uri: edits } },
    }))
}

/// Upstream sorts a name's import fixes by how many directories their module
/// path crosses, nearest first (`CodeActionsProvider.ts:1092-1101`).
pub fn sort_component_import_actions(actions: &mut [Value]) {
    actions.sort_by_key(|action| {
        action
            .get("title")
            .and_then(Value::as_str)
            .map_or(usize::MAX, |title| title.matches('/').count())
    });
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    fn uri() -> Uri {
        Uri::from_str("file:///workspace/App.svelte").unwrap()
    }

    fn context<'a>(uri: &'a Uri, source: &'a str) -> TsgoCodeActionContext<'a> {
        TsgoCodeActionContext::new(uri, source)
    }

    /// `source` as `App.svelte` beside `Widget.svelte`, projected: the
    /// overlay, the component's path and its shadow's path.
    fn projected(source: &str) -> (std::path::PathBuf, std::path::PathBuf, TsgoOverlay) {
        // Tests run in parallel: a counter keeps their workspaces apart.
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let nonce = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "rsvelte-component-import-{}-{nonce}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("Widget.svelte"), "<p>w</p>\n").unwrap();
        let path = root.join("App.svelte");
        std::fs::write(&path, source).unwrap();
        let overlay = TsgoOverlay::build_with(&root, None, None).unwrap();
        let shadow = crate::uri::uri_to_path(
            overlay
                .shadow_for_source(&path)
                .unwrap()
                .shadow_uri
                .as_str(),
        );
        (path, shadow, overlay)
    }

    fn cannot_find(source: &str, name: &str, nth: usize) -> Value {
        let at = source.match_indices(name).nth(nth).unwrap().0;
        let index = LineIndex::new(source);
        json!({
            "range": {
                "start": index.position(source, at),
                "end": index.position(source, at + name.len()),
            },
            "code": 2304,
            "message": format!("Cannot find name '{name}'."),
        })
    }

    fn sites(source: &str, diagnostics: Vec<Value>) -> Vec<ComponentImportSite> {
        let (path, shadow, overlay) = projected(source);
        let params = json!({ "context": { "diagnostics": diagnostics } });
        let sites = component_import_sites(&params, source, &overlay, &path, &shadow);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        sites
    }

    /// A component tag's `Cannot find name` is asked about at the end of the
    /// tag name in the shadow, the argument of `__sveltets_2_ensureComponent`.
    #[test]
    fn a_component_tags_missing_name_is_an_import_site() {
        let source = "<script lang=\"ts\">\n\tlet on = true;\n</script>\n\n<Widget />\n";
        let (path, shadow, overlay) = projected(source);
        let params = json!({ "context": { "diagnostics": [cannot_find(source, "Widget", 0)] } });
        let sites = component_import_sites(&params, source, &overlay, &path, &shadow);
        assert_eq!(sites.len(), 1, "{sites:?}");
        assert_eq!(sites[0].name, "Widget");
        let text = overlay.shadow_text(&shadow).unwrap();
        let end = overlay.shadow_offset(&shadow, sites[0].position).unwrap();
        assert!(
            text[..end].ends_with("__sveltets_2_ensureComponent(Widget"),
            "{}",
            &text[..end]
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// Upstream skips a name in a script and one that cannot be a component,
    /// and gives up on component imports altogether (TypeScript's own fixes
    /// answer) when a candidate is not a tag name.
    #[test]
    fn only_tag_names_are_import_sites() {
        let script = "<script lang=\"ts\">\n\tconst w = Widget;\n</script>\n";
        assert_eq!(
            sites(script, vec![cannot_find(script, "Widget", 0)]),
            vec![]
        );
        let lower = "<p>{widget}</p>\n";
        assert_eq!(sites(lower, vec![cannot_find(lower, "widget", 0)]), vec![]);
        let expression = "<p>{Widget}</p>\n<Widget />\n";
        assert_eq!(
            sites(
                expression,
                vec![
                    cannot_find(expression, "Widget", 0),
                    cannot_find(expression, "Widget", 1)
                ]
            ),
            vec![]
        );
    }

    /// The completion items that import a component are the ones labelled
    /// with its name, bare or with svelte2tsx's suffix; a resolved one makes a
    /// quick fix titled like TypeScript's, nearest module first.
    #[test]
    fn a_resolved_component_completion_makes_an_import_fix() {
        let completion = json!({ "items": [
            { "label": "Widget__SvelteComponent_" },
            { "label": "Widgets" },
            { "label": "Widget" },
        ]});
        let labels = component_import_candidates(&completion, "Widget")
            .iter()
            .map(|item| item["label"].clone())
            .collect::<Vec<_>>();
        assert_eq!(
            labels,
            vec![json!("Widget__SvelteComponent_"), json!("Widget")]
        );

        let edit = json!({
            "range": { "start": { "line": 3, "character": 0 }, "end": { "line": 3, "character": 0 } },
            "newText": "import Widget from './lib/Widget.svelte';\n",
        });
        let resolved = json!({ "label": "Widget", "additionalTextEdits": [edit] });
        let action = component_import_action(&resolved, "file:///App.svelte.tsx").unwrap();
        assert_eq!(action["title"], "Add import from \"./lib/Widget.svelte\"");
        assert_eq!(action["kind"], "quickfix");
        assert_eq!(
            action["edit"]["changes"]["file:///App.svelte.tsx"],
            json!([edit])
        );
        assert!(component_import_action(&json!({ "label": "Widget" }), "file:///a").is_none());

        let mut actions = vec![
            json!({ "title": "Add import from \"../../Widget.svelte\"" }),
            json!({ "title": "Add import from \"./Widget.svelte\"" }),
        ];
        sort_component_import_actions(&mut actions);
        assert_eq!(actions[0]["title"], "Add import from \"./Widget.svelte\"");
    }

    /// A code action's new import goes where a completion's does, and tsgo's
    /// import put before the script's first import lands at the start of that
    /// import's line, not after its indentation.
    #[test]
    fn a_code_actions_new_import_is_placed_like_a_completions() {
        let source = "<script lang=\"ts\">\n\timport Widget from './Widget.svelte';\n\tlet on = true;\n</script>\n\n<Widget />\n";
        let (path, shadow, overlay) = projected(source);
        let shadow_uri = overlay
            .shadow_for_source(&path)
            .unwrap()
            .shadow_uri
            .to_string();
        let text = overlay.shadow_text(&shadow).unwrap();
        let first_import = text
            .lines()
            .position(|line| line.starts_with("import "))
            .unwrap();
        let import = "import { helper } from './helpers';\n";
        let mut response = json!([{
            "title": "Add import from \"./helpers\"",
            "kind": "quickfix",
            "edit": { "changes": { shadow_uri.clone(): [{
                "range": {
                    "start": { "line": first_import, "character": 0 },
                    "end": { "line": first_import, "character": 0 },
                },
                "newText": import,
            }]}},
        }]);
        let placement = ImportPlacement {
            source,
            trigger: None,
            script_language: None,
        };
        let held = take_new_imports(&mut response, &overlay, &shadow, &shadow_uri, &placement);
        assert_eq!(response[0]["edit"]["changes"][&shadow_uri], json!([]));
        // The mapping moves `changes` to the source document's URI.
        let source_uri = "file:///App.svelte";
        let edits = response[0]["edit"]["changes"]
            .as_object_mut()
            .unwrap()
            .remove(&shadow_uri)
            .unwrap();
        response[0]["edit"]["changes"][source_uri] = edits;
        restore_new_imports(&mut response, source_uri, held);
        assert_eq!(
            response[0]["edit"]["changes"][source_uri],
            json!([{
                "range": { "start": { "line": 1, "character": 0 }, "end": { "line": 1, "character": 0 } },
                "newText": import,
            }])
        );
        assert!(response[0].get(NEW_IMPORTS_KEY).is_none());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn supported_kind_helpers_match_the_forwarded_surface() {
        for kind in [
            "quickfix",
            "source",
            "source.organizeImports",
            "source.sortImports",
            "source.addMissingImports",
            "source.removeUnusedImports",
            "source.fixAll",
        ] {
            assert!(is_supported_code_action_kind(kind), "{kind}");
        }
        assert!(!is_supported_code_action_kind("refactor.extract"));
        assert!(is_source_code_action_kind("source.fixAll.ts"));
        assert!(is_organize_code_action_kind("source.sortImports"));

        let source = vec!["source.organizeImports".to_string()];
        assert!(!should_forward_code_action_request(
            Some(&source),
            true,
            true
        ));
        assert!(!should_forward_code_action_request(
            Some(&source),
            false,
            false
        ));
        let quickfix = vec!["quickfix".to_string()];
        assert!(should_forward_code_action_request(
            Some(&quickfix),
            true,
            false
        ));
    }

    #[test]
    fn component_import_is_unsuffixed_and_wrapped_in_a_typescript_script() {
        let uri = uri();
        let source = "<Button />\r\n";
        let mut response = json!([{
            "title": "Add import from ./Button.svelte.tsx",
            "kind": "quickfix",
            "edit": {
                "changes": {
                    uri.as_str(): [{
                        "range": zero_range(),
                        "newText": "import type Button__SvelteComponent_ from './Button.svelte.tsx';\r\n"
                    }]
                }
            },
            "data": {
                "fixName": "import",
                "name": "Button__SvelteComponent_",
                "source": "./Button.svelte.tsx"
            }
        }]);
        let context = context(&uri, source).with_default_script_language(Some("ts"));
        assert_eq!(rewrite_code_action_response(&mut response, &context), 1);
        let action = &response[0];
        assert_eq!(action["title"], "Add import from ./Button.svelte");
        assert_eq!(
            action["edit"]["changes"][uri.as_str()][0]["newText"],
            "<script lang=\"ts\">\r\nimport Button from './Button.svelte';\r\n</script>\r\n"
        );
        assert_eq!(
            action["data"]["name"], "Button__SvelteComponent_",
            "resolve data belongs to tsgo"
        );
        assert_eq!(action["data"]["source"], "./Button.svelte.tsx");
    }

    #[test]
    fn parser_errors_suppress_source_actions_but_keep_quickfixes() {
        let uri = uri();
        let source = "<script>let broken = ;</script>";
        let mut response = json!([
            { "title": "Organize Imports", "kind": "source.organizeImports" },
            { "title": "Fix typo", "kind": "quickfix" },
            { "title": "Extract", "kind": "refactor.extract" }
        ]);
        let context = context(&uri, source).with_parser_error(true);
        assert_eq!(rewrite_code_action_response(&mut response, &context), 1);
        assert_eq!(response[0]["title"], "Fix typo");
    }

    #[test]
    fn organize_imports_keeps_script_edits_and_drops_markup_and_helpers() {
        let uri = uri();
        let source = "<script>\n  import { b, a } from './x';\n</script>\n<p />\n";
        let mut response = json!([{
            "title": "Organize Imports",
            "kind": "source.organizeImports",
            "edit": {
                "documentChanges": [{
                    "textDocument": { "uri": uri.as_str(), "version": null },
                    "edits": [
                        {
                            "range": {
                                "start": { "line": 1, "character": 2 },
                                "end": { "line": 1, "character": 36 }
                            },
                            "newText": "import { a, b } from './x';\n"
                        },
                        {
                            "range": {
                                "start": { "line": 0, "character": 0 },
                                "end": { "line": 0, "character": 0 }
                            },
                            "newText": "import { SvelteComponentTyped as __SvelteComponentTyped__ } from 'svelte';\n"
                        },
                        {
                            "range": {
                                "start": { "line": 3, "character": 0 },
                                "end": { "line": 3, "character": 0 }
                            },
                            "newText": "corrupt markup"
                        }
                    ]
                }]
            }
        }]);
        assert_eq!(
            rewrite_code_action_response(&mut response, &context(&uri, source)),
            1
        );
        let edits = response[0]["edit"]["documentChanges"][0]["edits"]
            .as_array()
            .unwrap();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0]["newText"], "import { a, b } from './x';\n");
    }

    #[test]
    fn organize_action_without_a_script_is_removed() {
        let uri = uri();
        let mut response = json!([{
            "title": "Organize Imports",
            "kind": "source.organizeImports",
            "edit": { "changes": { uri.as_str(): [] } }
        }]);
        assert_eq!(
            rewrite_code_action_response(&mut response, &context(&uri, "<p />")),
            0
        );
        assert_eq!(response, json!([]));
    }

    #[test]
    fn only_top_level_script_blocks_enable_source_actions() {
        assert!(!document_has_script(
            "<!-- <script></script> --><svelte:head><script></script></svelte:head><p />"
        ));
        assert!(document_has_script(
            "<svelte:head><script></script></svelte:head><script>let x;</script>"
        ));
        assert!(document_has_parser_error("<script>let = ;</script>"));
        assert!(!document_has_parser_error(
            "<script lang=\"ts\">let x: string;</script>"
        ));
    }

    #[test]
    fn ts_only_diagnostic_adds_or_updates_script_language() {
        let uri = uri();
        let codes = [8004];
        let mut no_script = json!([]);
        let no_script_context = context(&uri, "<p />").with_diagnostic_codes(&codes);
        assert_eq!(
            rewrite_code_action_response(&mut no_script, &no_script_context),
            1
        );
        assert_eq!(
            no_script[0]["edit"]["documentChanges"][0]["edits"][0]["newText"],
            "<script lang=\"ts\"></script>\n"
        );

        let source = "<script context=\"module\"></script>\n<script>let x;</script>";
        let mut scripts = json!([]);
        let context = context(&uri, source).with_diagnostic_codes(&codes);
        assert_eq!(rewrite_code_action_response(&mut scripts, &context), 1);
        let edits = scripts[0]["edit"]["documentChanges"][0]["edits"]
            .as_array()
            .unwrap();
        assert_eq!(edits.len(), 2);
        assert!(edits.iter().all(|edit| edit["newText"] == " lang=\"ts\""));
    }

    #[test]
    fn helper_only_quickfix_is_dropped_but_external_file_edit_is_preserved() {
        let uri = uri();
        let source = "<script>let x;</script>";
        let mut response = json!([
            {
                "title": "Generated helper",
                "kind": "quickfix",
                "edit": {
                    "changes": {
                        uri.as_str(): [{
                            "range": zero_range(),
                            "newText": "const x = __sveltets_2_any(0);"
                        }]
                    }
                }
            },
            {
                "title": "Update other file",
                "kind": "quickfix",
                "edit": {
                    "changes": {
                        "file:///workspace/other.ts": [{
                            "range": zero_range(),
                            "newText": "export const y = 1;"
                        }]
                    }
                }
            }
        ]);
        assert_eq!(
            rewrite_code_action_response(&mut response, &context(&uri, source)),
            1
        );
        assert_eq!(response[0]["title"], "Update other file");
    }
}
