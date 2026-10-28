//! The LSP message loop.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossbeam_channel::{Receiver, Sender, after, never, select, unbounded};
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, RequestId, Response};
use lsp_types::{
    CallHierarchyServerCapability, CancelParams, CodeActionKind, CodeActionOptions,
    CodeActionOrCommand, CodeActionParams, CodeActionProviderCapability, CodeLens, CodeLensOptions,
    CodeLensParams, ColorPresentationParams, ColorProviderCapability, CompletionOptions,
    CompletionOptionsCompletionItem, CompletionParams, CompletionTriggerKind, ConfigurationItem,
    ConfigurationParams, DiagnosticOptions, DiagnosticServerCapabilities,
    DidChangeTextDocumentParams, DidChangeWatchedFilesParams, DidChangeWorkspaceFoldersParams,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, DidSaveTextDocumentParams,
    DocumentColorParams, DocumentDiagnosticParams, DocumentDiagnosticReport,
    DocumentFormattingParams, DocumentHighlightParams, DocumentSymbolParams,
    DocumentSymbolResponse, ExecuteCommandOptions, ExecuteCommandParams, FoldingRange,
    FoldingRangeParams, FoldingRangeProviderCapability, FullDocumentDiagnosticReport, HoverParams,
    HoverProviderCapability, ImplementationProviderCapability, LinkedEditingRangeParams,
    LinkedEditingRangeServerCapabilities, NumberOrString, OneOf, Position, PositionEncodingKind,
    PublishDiagnosticsParams, RelatedFullDocumentDiagnosticReport, RenameOptions, RenameParams,
    SaveOptions, SelectionRangeParams, SelectionRangeProviderCapability, SemanticTokenModifier,
    SemanticTokenType, SemanticTokensFullOptions, SemanticTokensLegend, SemanticTokensOptions,
    SemanticTokensServerCapabilities, ServerCapabilities, SignatureHelpOptions,
    TextDocumentPositionParams, TextDocumentSyncCapability, TextDocumentSyncKind,
    TextDocumentSyncOptions, TextDocumentSyncSaveOptions, TextEdit,
    TypeDefinitionProviderCapability, Uri, WorkspaceFoldersServerCapabilities,
};

use rsvelte_projection::is_typescript_component;

use crate::background;
use crate::client::ClientState;
use crate::completions::TRIGGER_CHARACTERS;
use crate::context::ScriptOpeningTags;
use crate::document::{Document, DocumentStore};
use crate::log;
use crate::preprocess_sidecar::{
    PreprocessConfigCache, PreprocessEvent, PreprocessInput, PreprocessSidecar,
    PreprocessSidecarConfig,
};
use crate::settings::Settings;
use crate::text::LineIndex;
use crate::tsgo_client::{OpenBuffer, TsgoClient, TsgoClientClosed, TsgoConfig, TsgoEvent};
use crate::tsgo_code_actions::{
    ComponentImportSite, TsgoCodeActionContext, component_import_action,
    component_import_candidates, component_import_sites, document_has_parser_error,
    restore_new_imports, rewrite_code_action_response, sort_component_import_actions,
    take_new_imports,
};
use crate::tsgo_completion::{
    CompletionAction, CompletionRewriteContext, CompletionSite, ImportPlacement,
    adopt_upstream_completion_data, adopt_upstream_item_data, completion_action,
    restore_tsgo_completion_data, rewrite_completion_item_for_context,
    rewrite_completion_response_for_context, rewrite_visible_tsgo_response, take_new_import_edits,
    upstream_completion_data_site,
};
use crate::tsgo_component_info::{
    ComponentCompletionSite, ComponentInfoAction, ComponentInfoQuery, ComponentInfoRequestId,
    component_completion_site, generated_component_ranges,
};
use crate::tsgo_custom::{
    CodeLensKind, ComponentReference, ShadowUriPair, WillRenameMapping, code_lens_kind,
    component_probe_position, component_reference_code_lens, filter_component_references,
    prepare_code_lenses, resolve_code_lens, rewrite_will_rename_params, rewrite_will_rename_result,
};
use crate::tsgo_loop;
use crate::tsgo_overlay::{self, EditorBuffers, ProjectSpec, TsgoOverlay};
use crate::tsgo_rename::{
    PrepareRenamePlan, RenameDocument, merge_workspace_edits, prepare_rename_plan,
    rewrite_prepare_response, rewrite_workspace_edit, store_counterpart_position,
};
use crate::tsgo_response::{
    RequestDocumentContext, TsgoResponseMapper, empty_completion_list,
    filter_generated_inlay_hints, filter_source_inlay_hints, normalize_completion_documentation,
    normalize_definition_result, normalize_hover_result, rewrite_document_symbols,
    tsgo_unmapped_result, widen_hover_range_over_string_quotes,
};
use crate::uri::{path_to_uri, uri_to_path};
use crate::watchers::TsgoWatchers;
use crate::worker::{FileReferenceSource, Job, Outcome, PreprocessedAnalysis, Worker};

pub const SERVER_NAME: &str = "rsvelte-language-server";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long a burst of edits is coalesced before re-linting.
const LINT_DEBOUNCE: Duration = Duration::from_millis(300);

/// The `rsvelte` configuration section this server pulls from the client.
const CONFIG_SECTION: &str = "rsvelte";
const SVELTE_CONFIG_SECTION: &str = "svelte";
const JS_TS_CONFIG_SECTION: &str = "js/ts";
const TYPESCRIPT_CONFIG_SECTION: &str = "typescript";
const JAVASCRIPT_CONFIG_SECTION: &str = "javascript";
const EDITOR_CONFIG_SECTION: &str = "editor";
const EMMET_CONFIG_SECTION: &str = "emmet";

const TSGO_SEMANTIC_TOKEN_TYPES: &[&str] = &[
    "namespace",
    "class",
    "enum",
    "interface",
    "struct",
    "typeParameter",
    "type",
    "parameter",
    "variable",
    "property",
    "enumMember",
    "decorator",
    "event",
    "function",
    "method",
    "macro",
    "label",
    "comment",
    "string",
    "keyword",
    "number",
    "regexp",
    "operator",
];
const TSGO_SEMANTIC_TOKEN_MODIFIERS: &[&str] = &[
    "declaration",
    "definition",
    "readonly",
    "static",
    "deprecated",
    "abstract",
    "async",
    "modification",
    "documentation",
    "defaultLibrary",
    "local",
];

/// Serve the LSP over stdio until the client shuts the connection down.
///
/// **stdout belongs to the JSON-RPC session.** Anything printed there that is
/// not a framed message corrupts the stream with no way back, so no code
/// reachable from here may write to it — note that `rsvelte_fmt` does print to
/// stdout on its CLI paths, which is why only [`rsvelte_fmt::FormatSession`]
/// (which never does) is used.
///
/// # Errors
///
/// Returns an error when initializing or serving the JSON-RPC connection fails.
pub fn run_stdio() -> Result<ExitCode> {
    let (connection, io_threads) = crate::transport::stdio();
    let (id, params) = connection.initialize_start()?;
    let client = ClientState::from_initialize(&params);
    let tsgo = TsgoRuntime::start(&client, &params);

    connection.initialize_finish(
        id,
        serde_json::json!({
            "capabilities": capabilities(&client),
            "serverInfo": { "name": SERVER_NAME, "version": VERSION },
        }),
    )?;

    let (results, outcomes) = unbounded();
    let code = Server::new(
        connection.sender.clone(),
        client,
        Worker::spawn(results),
        outcomes,
        tsgo,
        params,
    )
    .run(&connection)?;

    // The writer thread ends only once every sender is gone, so the connection
    // (and the server's clone of it) must be dropped before joining.
    drop(connection);
    io_threads.join()?;
    Ok(code)
}

/// The token types of the semantic token legend: tsgo's, narrowed to the ones
/// the editor advertised.
fn semantic_token_types(client: &ClientState) -> Vec<&'static str> {
    TSGO_SEMANTIC_TOKEN_TYPES
        .iter()
        .copied()
        .filter(|token| {
            client
                .semantic_token_types
                .iter()
                .any(|supported| supported == token)
        })
        .collect()
}

fn capabilities(client: &ClientState) -> ServerCapabilities {
    let mut code_action_kinds = vec![
        CodeActionKind::QUICKFIX,
        CodeActionKind::SOURCE_ORGANIZE_IMPORTS,
        CodeActionKind::from(crate::tsgo_code_actions::SORT_IMPORTS_KIND),
        CodeActionKind::from(crate::tsgo_code_actions::ADD_MISSING_IMPORTS_KIND),
        CodeActionKind::from(crate::tsgo_code_actions::REMOVE_UNUSED_IMPORTS_KIND),
        CodeActionKind::SOURCE_FIX_ALL,
        CodeActionKind::from(crate::code_actions::FIX_ALL_KIND),
    ];
    if client.apply_edit {
        code_action_kinds.insert(1, CodeActionKind::REFACTOR);
    }
    ServerCapabilities {
        // The editor-facing protocol stays on the LSP default. The tsgo child
        // is negotiated separately to UTF-8 so all internal mapping is byte-based.
        position_encoding: Some(PositionEncodingKind::UTF16),
        text_document_sync: Some(TextDocumentSyncCapability::Options(
            TextDocumentSyncOptions {
                open_close: Some(true),
                change: Some(TextDocumentSyncKind::INCREMENTAL),
                save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                    include_text: Some(false),
                })),
                ..TextDocumentSyncOptions::default()
            },
        )),
        document_formatting_provider: Some(OneOf::Left(true)),
        completion_provider: Some(CompletionOptions {
            trigger_characters: Some(TRIGGER_CHARACTERS.map(str::to_string).to_vec()),
            resolve_provider: Some(true),
            completion_item: Some(CompletionOptionsCompletionItem {
                label_details_support: Some(true),
            }),
            ..CompletionOptions::default()
        }),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        signature_help_provider: Some(SignatureHelpOptions {
            trigger_characters: Some(vec!["(".to_string(), ",".to_string(), "<".to_string()]),
            retrigger_characters: Some(vec![")".to_string()]),
            ..SignatureHelpOptions::default()
        }),
        definition_provider: Some(OneOf::Left(true)),
        type_definition_provider: Some(TypeDefinitionProviderCapability::Simple(true)),
        implementation_provider: Some(ImplementationProviderCapability::Simple(true)),
        references_provider: Some(OneOf::Left(true)),
        code_action_provider: Some(CodeActionProviderCapability::Options(CodeActionOptions {
            code_action_kinds: Some(code_action_kinds),
            ..CodeActionOptions::default()
        })),
        code_lens_provider: Some(CodeLensOptions {
            resolve_provider: Some(true),
        }),
        execute_command_provider: client.apply_edit.then(|| ExecuteCommandOptions {
            commands: vec![crate::extract::COMMAND.to_string()],
            ..ExecuteCommandOptions::default()
        }),
        folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
        selection_range_provider: Some(SelectionRangeProviderCapability::Simple(true)),
        document_symbol_provider: Some(OneOf::Left(true)),
        linked_editing_range_provider: Some(LinkedEditingRangeServerCapabilities::Simple(true)),
        document_highlight_provider: Some(OneOf::Left(client.document_highlight)),
        workspace_symbol_provider: Some(OneOf::Left(true)),
        rename_provider: Some(if client.rename_prepare {
            OneOf::Right(RenameOptions {
                prepare_provider: Some(true),
                work_done_progress_options: Default::default(),
            })
        } else {
            OneOf::Left(true)
        }),
        inlay_hint_provider: Some(OneOf::Left(true)),
        // tsgo narrows its own legend to the token names the editor advertised,
        // and its token data indexes that narrowed legend. The same filter has
        // to run here or every index past a dropped entry names the wrong type.
        semantic_tokens_provider: Some(SemanticTokensServerCapabilities::SemanticTokensOptions(
            SemanticTokensOptions {
                legend: SemanticTokensLegend {
                    token_types: semantic_token_types(client)
                        .into_iter()
                        .map(SemanticTokenType::new)
                        .collect(),
                    token_modifiers: TSGO_SEMANTIC_TOKEN_MODIFIERS
                        .iter()
                        .filter(|modifier| {
                            client
                                .semantic_token_modifiers
                                .iter()
                                .any(|supported| supported == **modifier)
                        })
                        .map(|modifier| SemanticTokenModifier::new(modifier))
                        .collect(),
                },
                range: Some(true),
                full: Some(SemanticTokensFullOptions::Bool(true)),
                ..SemanticTokensOptions::default()
            },
        )),
        call_hierarchy_provider: Some(CallHierarchyServerCapability::Simple(true)),
        color_provider: Some(ColorProviderCapability::Simple(true)),
        diagnostic_provider: client.pull_diagnostics.then(|| {
            DiagnosticServerCapabilities::Options(DiagnosticOptions {
                identifier: Some(SERVER_NAME.to_string()),
                // A component's diagnostics come from a TypeScript program, so
                // editing one file can change another file's report.
                inter_file_dependencies: true,
                workspace_diagnostics: false,
                ..DiagnosticOptions::default()
            })
        }),
        workspace: Some(lsp_types::WorkspaceServerCapabilities {
            workspace_folders: Some(WorkspaceFoldersServerCapabilities {
                supported: Some(true),
                change_notifications: Some(OneOf::Left(true)),
            }),
            ..lsp_types::WorkspaceServerCapabilities::default()
        }),
        ..ServerCapabilities::default()
    }
}

/// A client request whose answer is still being computed. The response is sent
/// when the worker reports back, so a handler never has to block the loop to
/// produce one.
enum Pending {
    Formatting,
    /// `filter`, the word an incomplete answer is narrowed to
    /// (`PluginHost.ts:285-296`), when the client has the server filter.
    Completion {
        tsgo_fallback: Request,
        filter: Option<String>,
    },
    Hover {
        tsgo_fallback: Request,
    },
    ExtractComponent,
    CompiledCode,
    SelectionRange,
    FileReferences,
}

impl Pending {
    /// Whether tsgo is asked once the native worker answers.
    const fn asks_tsgo_next(&self) -> bool {
        matches!(self, Self::Completion { .. } | Self::Hover { .. })
    }
}

/// A request whose tsgo half went out when it arrived, while the worker
/// computes its native half (a diagnostic pull, code actions, code lenses,
/// folding ranges, document symbols: tsgo is asked whatever the worker
/// finds, and the two answers are merged). Whichever half comes back first
/// waits here for the other, so an edit that follows such a request never
/// waits for its native half, and tsgo never sees the edit first.
struct Join {
    method: &'static str,
    first: Option<JoinHalf>,
}

enum JoinHalf {
    /// The worker's answer, the fallback when tsgo has none.
    Native(Option<serde_json::Value>),
    /// tsgo's answer, already mapped back to the source.
    Tsgo(Response),
}

impl Join {
    const fn new(method: &'static str) -> Self {
        Self {
            method,
            first: None,
        }
    }
}

/// tsgo's answer to a joined request with the worker's merged in, as
/// [`Server::on_tsgo_message`] merges a fallback: the worker's items first,
/// and the worker's answer alone when tsgo failed.
fn joined(method: &str, mut response: Response, native: Option<serde_json::Value>) -> Response {
    let Some(native) = native else {
        return response;
    };
    match &mut response.response_result {
        Ok(result) => merge_tsgo_result(method, result, native),
        Err(_) => response.response_result = Ok(native),
    }
    response
}

struct PendingTsgoRequest {
    method: String,
    document: Option<RequestDocumentContext>,
    fallback_result: Option<serde_json::Value>,
    completion_site: Option<CompletionSite>,
    rename: Option<PendingRename>,
    code_action_diagnostic_codes: Vec<u32>,
    push_diagnostics: Option<(Uri, i32)>,
    component_site: Option<ComponentCompletionSite>,
    component_references: Option<Uri>,
    file_rename: Option<PendingFileRename>,
    code_lens_resolve: Option<PendingCodeLensResolve>,
    /// The source document and position an adopted completion `data` names.
    completion_site_data: Option<(String, serde_json::Value)>,
    /// For the editor's references request: its `includeDeclaration`, which
    /// the store follow-ups ask with ([`Server::continue_store_references`]).
    include_declaration: Option<bool>,
    /// For the editor's code-action request: the component tags its
    /// `Cannot find name` diagnostics are on ([`Server::continue_component_imports`]).
    component_imports: Vec<ComponentImportSite>,
}

/// The references of a store and of its `$store` value, asked together
/// ([`Server::continue_store_references`]).
#[derive(Default)]
struct StoreReferences {
    /// By the editor's references request: what has been gathered so far.
    gathering: HashMap<RequestId, StoreReferencesGathering>,
    /// Each follow-up request: the editor's request it belongs to.
    followups: HashMap<RequestId, RequestId>,
    /// The editor's requests whose gathered answer is being mapped.
    gathered: HashSet<RequestId>,
}

struct StoreReferencesGathering {
    /// The editor's request, held until every follow-up is answered.
    pending: PendingTsgoRequest,
    include_declaration: bool,
    /// tsgo's locations so far, in shadow coordinates.
    locations: Vec<serde_json::Value>,
    /// The positions (shadow, line, character) whose symbol's references are
    /// asked for or among the locations.
    covered: HashSet<(String, u32, u32)>,
    /// Follow-ups not answered yet.
    remaining: usize,
}

/// The "Add import" quick fixes of component tags, asked as auto-import
/// completions ([`Server::continue_component_imports`]).
#[derive(Default)]
struct ComponentImports {
    /// By the editor's code-action request: what has been gathered so far.
    gathering: HashMap<RequestId, ComponentImportsGathering>,
    /// Each follow-up request: the editor's request and the site it asks about.
    followups: HashMap<RequestId, (RequestId, ComponentImportFollowup)>,
    /// The editor's requests whose gathered answer is being mapped.
    gathered: HashSet<RequestId>,
}

struct ComponentImportsGathering {
    /// The editor's request, held until every follow-up is answered.
    pending: PendingTsgoRequest,
    /// tsgo's own answer to it, in shadow coordinates.
    actions: Vec<serde_json::Value>,
    /// The quick fixes made from resolved completions, in shadow coordinates.
    fixes: Vec<serde_json::Value>,
    shadow_uri: Uri,
    /// Follow-ups not answered yet.
    remaining: usize,
}

#[derive(Clone)]
enum ComponentImportFollowup {
    /// The completions at the tag of the component `name`.
    Completion { name: String },
    /// One of them, resolved for its import edits.
    Resolve,
}

/// How long a document change waits for the native half of the requests
/// that arrived before it ([`Server::finish_native_halves`]).
const NATIVE_HALF_WAIT: Duration = Duration::from_millis(250);

/// How long tsgo has to answer a request before the server answers it
/// without tsgo ([`Server::expire_tsgo_requests`]). tsrs occasionally never
/// answers a request; the editor would wait for it forever.
/// `RSVELTE_TSGO_REQUEST_TIMEOUT_MS` overrides it.
const TSGO_ANSWER_TIMEOUT: Duration = Duration::from_secs(30);

/// How many times a restart for the same method and document doubles the
/// time its next one waits for ([`RestartBackoff`]).
const TSGO_RESTART_BACKOFF_LIMIT: u32 = 3;

/// After how many request timeouts without a restart for a method and
/// document a request for them waits the plain timeout again
/// ([`RestartBackoff`]): the longest wait the doubling reaches.
const TSGO_RESTART_QUIET_TIMEOUTS: u32 = 1 << TSGO_RESTART_BACKOFF_LIMIT;

/// How long an overdue request tsgo loops on waits before it restarts tsgo.
/// Each restart for the same method and document doubles the next one's
/// wait, up to [`TSGO_RESTART_BACKOFF_LIMIT`] times, so that a state the
/// editor keeps asking about does not restart tsgo, which rebuilds every
/// project, every timeout. The doubling counts only restarts shortly before
/// the request: one sent [`TSGO_RESTART_QUIET_TIMEOUTS`] timeouts after the
/// last restart for its method and document waits the plain timeout again
/// (review 5: the doubling never reset, so a loop hours later still waited
/// eight timeouts, minutes of an editor without TypeScript answers).
#[derive(Default)]
struct RestartBackoff {
    /// By method and document: how many restarts in a row, and the last one.
    restarts: HashMap<(String, Option<String>), (u32, Instant)>,
}

impl RestartBackoff {
    /// How many times the wait of a request sent at `sent` doubles.
    fn doublings(&self, key: &(String, Option<String>), sent: Instant, timeout: Duration) -> u32 {
        self.restarts
            .get(key)
            .filter(|(_, last)| {
                sent.saturating_duration_since(*last) < timeout * TSGO_RESTART_QUIET_TIMEOUTS
            })
            .map_or(0, |(count, _)| (*count).min(TSGO_RESTART_BACKOFF_LIMIT))
    }

    /// Note a restart at `now` for a request sent at `sent`: one in a row
    /// with the last for its method and document if it was sent before the
    /// quiet period after that one ended.
    fn restarted(
        &mut self,
        key: (String, Option<String>),
        sent: Instant,
        now: Instant,
        timeout: Duration,
    ) {
        if self.doublings(&key, sent, timeout) == 0 {
            self.restarts.remove(&key);
        }
        let (count, last) = self.restarts.entry(key).or_insert((0, now));
        *count += 1;
        *last = now;
    }
}

/// A request tsgo has not answered yet.
struct SentRequest {
    at: Instant,
    method: String,
    /// The document it is about (`params.textDocument.uri`: a shadow's).
    document: Option<String>,
    /// Whether tsrs loops on it for good ([`TsgoRuntime::loops_on`]).
    loops: bool,
    /// Answered without tsgo once overdue, and cancelled; still tracked
    /// until tsgo answers it late (tsrs answers a slow request even once
    /// cancelled) or restarts: tsrs never answers one it loops on, and one
    /// the model ([`tsgo_loop`]) misses is told apart only by that
    /// ([`Server::tsgo_restart_at`]).
    overdue: bool,
}

impl SentRequest {
    /// An overdue completion or signature help the model does not say tsrs
    /// loops on: the two requests tsrs is known to loop on, so possibly a
    /// loop the model misses (review 6: regular expressions), possibly a
    /// slow one ([`SUSPECT_RESTART_TIMEOUTS`]).
    fn suspect(&self) -> bool {
        self.overdue && !self.loops && may_loop(&self.method)
    }
}

/// Whether tsrs can loop for good on a request of `method`
/// (`docs/upstream/tsrs-lost-request` in svelte-language-server-rs).
fn may_loop(method: &str) -> bool {
    matches!(
        method,
        "textDocument/completion" | "textDocument/signatureHelp"
    )
}

/// How many requests about one project tsrs runs at once: each project's
/// checker pool has four checkers, one of them kept for diagnostics
/// (`tsrs_project` `checkerpool.rs`), and a request holds one while it runs.
/// With this many looping, tsrs answers nothing else about the project
/// (review 5's loopstorm.mjs: three; review 6's missedloop.mjs: four), so
/// this many overdue requests restart tsgo whatever the model says
/// ([`Server::tsgo_restart_at`]).
const TSRS_QUERY_CHECKERS: usize = 3;

/// After how many timeouts an overdue completion or signature help the model
/// does not say tsrs loops on restarts tsgo ([`SentRequest::suspect`]): tsrs
/// answers a slow one once it is done, even cancelled, and never one it
/// loops on, which keeps a core busy for good (review 6: 100% CPU for the
/// rest of the session).
const SUSPECT_RESTART_TIMEOUTS: u32 = 4;

/// The [`RestartBackoff`] key of the restarts for a tsgo that answers nothing.
fn liveness_key() -> (String, Option<String>) {
    ("tsgo answers nothing".to_string(), None)
}

/// A reason to restart tsgo ([`Server::tsgo_restart_candidates`]).
struct TsgoRestart {
    /// When it falls due.
    at: Instant,
    /// What tsgo is still busy with, for the log.
    why: String,
    /// The [`RestartBackoff`] key it counts toward, and when the request
    /// behind it was sent.
    backoff: ((String, Option<String>), Instant),
}

/// Whether a request is the background's own, which has tsgo build a project
/// ([`Server::warm_up`]).
fn is_warm_up(id: &RequestId) -> bool {
    id.to_string().contains(WARM_UP_ID)
}

/// The start of the ids of [`Server::warm_up`]'s requests.
const WARM_UP_ID: &str = "rsvelte-background-warm-up-";

/// A request tsgo answers whenever it is done: a document's check can take
/// longer than any timeout and is not lost. tsrs answers a cancelled pull
/// only once its check is done, and throws the result away, so the next
/// pull checks again; a restart throws away every project.
fn waits_for_tsgo(method: &str) -> bool {
    method == "textDocument/diagnostic"
}

/// How long an unanswered editor request keeps the background from loading
/// projects: the background waits for tsgo to be idle, and a request tsgo
/// lost would otherwise hold it until the request expires.
const TSGO_BUSY_FOR: Duration = Duration::from_secs(2);

/// Work held while a component query probes in a shadow's own buffer.
enum Deferred {
    /// An editor request, replayed from the start.
    Request(Request),
    /// A request on its way to tsgo, with its native fallback.
    Forward(Request, Option<serde_json::Value>),
    /// A diagnostic pull for a client that takes pushed diagnostics.
    PushDiagnostics(Uri, i32, Vec<lsp_types::Diagnostic>),
}

struct PendingComponentQuery {
    query: ComponentInfoQuery,
    site: ComponentCompletionSite,
    result: serde_json::Value,
}

enum PendingRename {
    Prepare(PrepareRenamePlan),
    Primary {
        plan: PrepareRenamePlan,
        new_name: String,
    },
    Followup {
        editor_id: RequestId,
    },
}

struct RenameAggregate {
    plan: PrepareRenamePlan,
    new_name: String,
    edits: Vec<serde_json::Value>,
    remaining: usize,
}

struct PendingFileRename {
    old_source: Uri,
    old_shadow: Uri,
    new_source: Uri,
    new_shadow: Uri,
}

struct PendingCodeLensResolve {
    lens: serde_json::Value,
    kind: CodeLensKind,
    source_uri: Uri,
}

/// A request this server sent to the client, keyed by the id the client will
/// echo back.
enum Outgoing {
    Configuration,
    WatchedFilesRegistration,
    DiagnosticRefresh,
    ApplyEdit { command_id: RequestId },
    Tsgo { child_id: RequestId },
}

struct TsgoRuntime {
    client: TsgoClient,
    overlays: Vec<TsgoOverlay>,
    generation: Option<u64>,
    /// The overlay that owns each directory's documents, once resolved. The
    /// walk is a handful of `stat`s and `sync_tsgo_document` runs on every
    /// keystroke. Cleared whenever `overlays` is reordered or rebuilt.
    resolved_dirs: HashMap<PathBuf, usize>,
    /// Shadows tsgo holds as in-memory buffers: editor documents, and the
    /// JavaScript components they reach. tsgo reads every other component
    /// from the body the overlay keeps on disk.
    open_shadows: HashMap<String, Uri>,
    /// Overlays, by cache directory, that have served the editor.
    activated: HashSet<PathBuf>,
    /// JavaScript component shadows an editor document reached this
    /// session: they stay buffers, including after a change on disk.
    javascript_buffers: HashSet<String>,
    /// Each open document's `.svelte` imports, as a fingerprint: when they
    /// change, the JavaScript components it reaches may have.
    document_imports: HashMap<PathBuf, u64>,
    /// One component shadow per loaded project, by cache directory, held as
    /// a buffer so tsgo keeps the project: tsgo unloads a project none of
    /// whose files is open, and workspace-wide requests only see the
    /// projects it has. The source path names the component.
    keep_alive: HashMap<PathBuf, (PathBuf, Uri)>,
    /// Whether every loaded project is kept, which starts with the first
    /// background load or workspace-wide request.
    keep_all: bool,
    /// The requests tsgo has not answered yet. A cancelled request stays:
    /// tsgo answers those too, and one it never answers is still running.
    sent: RefCell<HashMap<RequestId, SentRequest>>,
    /// When tsgo last answered a request that is not a diagnostic pull (or
    /// started). tsrs checks a project's diagnostics with a checker of their
    /// own, so it may answer pulls while every request it runs with the other
    /// checkers loops ([`TSRS_QUERY_CHECKERS`]).
    last_query_answer: Cell<Instant>,
    /// For each document, when the last diagnostic pull of it tsgo answered
    /// was sent and answered ([`Server::waiting_for_check_until`]).
    pulls_answered: RefCell<HashMap<String, (Instant, Instant)>>,
}

/// tsgo takes a buffer's script kind from its language id. A JavaScript
/// component's body on disk is named `.svelte.tsx`, which tsgo would parse as
/// TSX and lose its JSDoc types to, so those stay open as buffers.
const JAVASCRIPT_SHADOW: &str = "javascriptreact";

/// The overlay that serves `source` (canonical): the project that owns it,
/// else the one with the deepest workspace containing it. A project can hold
/// a copy of another project's component, as deep as the owner's or deeper,
/// so depth alone could pick the copy.
fn serving_index(overlays: &[TsgoOverlay], source: &Path) -> Option<usize> {
    overlays
        .iter()
        .enumerate()
        .filter(|(_, overlay)| source.starts_with(overlay.workspace()))
        .max_by_key(|(_, overlay)| {
            (
                overlay.owns(source),
                overlay.workspace().components().count(),
            )
        })
        .map(|(index, _)| index)
}

/// Where the project that owns `source` (canonical) comes from.
#[derive(Debug, PartialEq, Eq)]
enum ProjectLocation {
    /// `overlays[index]` is the project; it may still need loading.
    Existing(usize),
    /// A nested config owns the document and has no overlay yet.
    Nested {
        config: PathBuf,
        mirror_root: PathBuf,
    },
}

/// The project that owns `source`: its nearest config within the workspace
/// folder containing it (`findTsConfigPath`, `utils.ts:146-167`), or the
/// folder's own project when it has none — which, when the folder has a
/// config, means `source` is a dependency's file under `node_modules`, where
/// the config search stops. `None` outside every workspace folder.
fn locate_project(overlays: &[TsgoOverlay], source: &Path) -> Option<ProjectLocation> {
    let mirror_root = overlays
        .iter()
        .filter(|overlay| {
            overlay.mirror_root() == overlay.workspace() && source.starts_with(overlay.workspace())
        })
        .max_by_key(|overlay| overlay.workspace().components().count())?
        .mirror_root()
        .to_path_buf();
    match tsgo_overlay::nearest_tsconfig(source, &mirror_root) {
        Some(config) => Some(
            overlays
                .iter()
                .position(|overlay| {
                    overlay.mirror_root() == mirror_root
                        && overlay.source_tsconfig() == Some(config.as_path())
                })
                .map_or(
                    ProjectLocation::Nested {
                        config,
                        mirror_root: mirror_root.clone(),
                    },
                    ProjectLocation::Existing,
                ),
        ),
        None => overlays
            .iter()
            .position(|overlay| overlay.workspace() == mirror_root)
            .map(ProjectLocation::Existing),
    }
}

/// Write the body of every component a loaded overlay holds and sweep the
/// bodies it no longer does. Returns the watched-file changes, which only
/// matter once tsgo has loaded the project.
pub(crate) fn persist_bodies(overlay: &TsgoOverlay) -> Vec<(Uri, lsp_types::FileChangeType)> {
    let mut events = Vec::new();
    for shadow in overlay.component_shadows() {
        let source = uri_to_path(shadow.source_uri.as_str());
        match overlay.persist_body(&source) {
            Ok(Some(change)) => events.push((shadow.shadow_uri.clone(), change)),
            Ok(None) => {}
            Err(error) => log::warn(format_args!(
                "could not persist tsgo shadow for {}: {error}",
                source.display()
            )),
        }
    }
    for removed in overlay.sweep_bodies(|_| true) {
        if let Some(uri) = path_to_uri(&removed) {
            events.push((uri, lsp_types::FileChangeType::DELETED));
        }
    }
    for route in overlay.unresolved_shadow_routes() {
        log::warn(format_args!(
            "tsgo shadow is not resolvable: {} -> {}",
            route.source_path.display(),
            route.shadow_path.display()
        ));
    }
    events
}

/// Open (or update) a shadow as a tsgo buffer.
fn open_shadow(
    client: &TsgoClient,
    open: &mut HashMap<String, Uri>,
    shadow: &tsgo_overlay::ShadowDocument,
) {
    open.insert(
        shadow.shadow_uri.as_str().to_string(),
        shadow.shadow_uri.clone(),
    );
    let _ = client.change_buffer(OpenBuffer::new(
        shadow.shadow_uri.clone(),
        shadow.language_id.clone(),
        shadow.version,
        shadow.text.clone(),
    ));
}

/// Close a shadow's tsgo buffer; tsgo falls back to the body on disk.
fn close_shadow(client: &TsgoClient, open: &mut HashMap<String, Uri>, uri: Uri) {
    open.remove(uri.as_str());
    let _ = client.close_buffer(uri);
}

struct PreprocessRuntime {
    client: PreprocessSidecar,
    generation: Option<u64>,
}

struct PreprocessDocumentState {
    version: i32,
    text: Arc<String>,
    map: Option<Arc<String>>,
    identity: bool,
}

impl TsgoRuntime {
    fn start(editor: &ClientState, initialize_params: &serde_json::Value) -> Option<Self> {
        let mut roots = editor
            .workspace_folders
            .iter()
            .map(|folder| uri_to_path(folder.uri.as_str()))
            .collect::<Vec<_>>();
        if roots.is_empty()
            && let Some(root) = &editor.root_uri
        {
            roots.push(uri_to_path(root.as_str()));
        }
        roots.retain(|root| root.is_dir());
        roots.sort();
        roots.dedup();
        let primary = roots.first()?.clone();

        // Nothing is discovered or projected here: a project is loaded when the
        // first document it owns reaches tsgo (`ensure_project_for`). Its
        // config and shadow tree live in its own cache directory, which no
        // other project's include reaches, so a later load never changes
        // another project's answer.
        let mut overlays = Vec::new();
        for root in &roots {
            match TsgoOverlay::build_deferred(root, None) {
                Ok(overlay) => overlays.push(overlay),
                Err(error) => log::warn(format_args!(
                    "could not prepare tsgo overlay for {}: {error}",
                    root.display()
                )),
            }
        }
        if overlays.is_empty() {
            return None;
        }

        let binary = match rsvelte_check::tsgo::find_compiler(&primary, true) {
            Ok(binary) => binary,
            Err(error) => {
                log::warn(format_args!(
                    "TypeScript language features unavailable: {error}"
                ));
                return None;
            }
        };
        let mut config = TsgoConfig::new(PathBuf::from(binary.program));
        config.args_prefix = binary.args_prefix.into_iter().map(OsString::from).collect();
        config.current_dir = Some(primary);
        config.root_uri = editor.root_uri.clone();
        config.workspace_folders = editor.workspace_folders.clone();
        config.editor_initialize_params = initialize_params.clone();
        let client = match TsgoClient::spawn(config) {
            Ok(client) => client,
            Err(error) => {
                log::warn(format_args!("could not start tsgo supervisor: {error}"));
                return None;
            }
        };
        Some(Self {
            client,
            overlays,
            generation: None,
            resolved_dirs: HashMap::new(),
            open_shadows: HashMap::new(),
            activated: HashSet::new(),
            javascript_buffers: HashSet::new(),
            document_imports: HashMap::new(),
            keep_alive: HashMap::new(),
            keep_all: false,
            sent: RefCell::new(HashMap::new()),
            last_query_answer: Cell::new(Instant::now()),
            pulls_answered: RefCell::new(HashMap::new()),
        })
    }

    /// Send tsgo a message, noting when each request goes out and what it is
    /// about ([`Server::expire_tsgo_requests`]).
    fn forward(&self, message: Message) -> Result<(), TsgoClientClosed> {
        let request = match &message {
            Message::Request(request) => {
                let document = request
                    .params
                    .pointer("/textDocument/uri")
                    .and_then(serde_json::Value::as_str);
                Some((
                    request.id.clone(),
                    SentRequest {
                        at: Instant::now(),
                        method: request.method.clone(),
                        document: document.map(str::to_string),
                        loops: self.loops_on(&request.method, &request.params),
                        overdue: false,
                    },
                ))
            }
            _ => None,
        };
        self.client.forward(message)?;
        if let Some((id, request)) = request {
            self.sent.borrow_mut().insert(id, request);
        }
        Ok(())
    }

    /// Whether tsrs loops for good on a request (tsgo's half, in shadow
    /// coordinates) in the text tsgo has for its document ([`tsgo_loop`]):
    /// only a completion or signature help can, and the walk that says so
    /// runs only for a text a few scans find it possible in.
    fn loops_on(&self, method: &str, params: &serde_json::Value) -> bool {
        if !matches!(
            method,
            "textDocument/completion" | "textDocument/signatureHelp"
        ) {
            return false;
        }
        let Some(path) = params
            .pointer("/textDocument/uri")
            .and_then(serde_json::Value::as_str)
            .map(uri_to_path)
        else {
            return false;
        };
        let Some(position) = params
            .get("position")
            .and_then(|position| serde_json::from_value::<Position>(position.clone()).ok())
        else {
            return false;
        };
        self.overlays
            .iter()
            .find_map(|overlay| {
                Some((
                    overlay.shadow_text(&path)?,
                    overlay.shadow_offset(&path, position)?,
                ))
            })
            .is_some_and(|(text, offset)| {
                tsgo_loop::text_can_loop(text) && tsgo_loop::request_loops(method, text, offset)
            })
    }

    /// A request about the same document as `params` that tsgo has not
    /// answered and loops on, when tsrs would loop on this one too, or an
    /// overdue completion or signature help there it may loop on
    /// ([`SentRequest::suspect`]), when this is one of those
    /// ([`Server::forward_tsgo_request_with_fallback`]).
    fn looping_on_document(&self, method: &str, params: &serde_json::Value) -> Option<RequestId> {
        if !may_loop(method) {
            return None;
        }
        let document = params
            .pointer("/textDocument/uri")
            .and_then(serde_json::Value::as_str)?;
        let sent = self.sent.borrow();
        let on_document = |sent: &&SentRequest| sent.document.as_deref() == Some(document);
        if let Some((suspect, _)) = sent
            .iter()
            .find(|(_, sent)| sent.suspect() && on_document(sent))
        {
            return Some(suspect.clone());
        }
        let looping = sent
            .iter()
            .find(|(_, sent)| sent.loops && on_document(sent))
            .map(|(id, _)| id.clone())?;
        drop(sent);
        self.loops_on(method, params).then_some(looping)
    }

    /// Forget a request tsgo answered, noting when tsgo answers
    /// ([`Self::last_query_answer`], [`Self::pulls_answered`]), and return
    /// it, if tsgo had not answered it yet: the server answers an overdue
    /// request through the same path, after it stopped tracking it.
    fn answered(&self, id: &RequestId) -> Option<SentRequest> {
        let request = self.sent.borrow_mut().remove(id)?;
        let now = Instant::now();
        if waits_for_tsgo(&request.method) {
            if let Some(document) = request.document.clone() {
                self.pulls_answered
                    .borrow_mut()
                    .insert(document, (request.at, now));
            }
        } else {
            self.last_query_answer.set(now);
        }
        Some(request)
    }

    /// Forget every request tsgo was asked: it restarted or exited.
    fn forget_requests(&self) {
        self.sent.borrow_mut().clear();
        self.pulls_answered.borrow_mut().clear();
        self.last_query_answer.set(Instant::now());
    }

    fn sent_at(&self, id: &RequestId) -> Option<Instant> {
        self.sent.borrow().get(id).map(|request| request.at)
    }

    /// Ask tsgo to drop a request nothing waits for any more.
    fn cancel(&self, id: &RequestId) {
        let _ = self.forward(
            Notification::new(
                "$/cancelRequest".to_string(),
                serde_json::json!({ "id": id }),
            )
            .into(),
        );
    }

    /// Make sure the project that owns `source` is built and loaded, and
    /// return its index with whether it loaded now. `svelte-language-server`
    /// keys a service by tsconfig path and resolves one **per document**
    /// (`plugins/typescript/service.ts:172-201`), so a subdirectory carrying
    /// its own config — `paths`, `types`, `strict` — is a project of its own,
    /// created on demand, so a repository with hundreds of configs only pays
    /// for the ones it opens. Loading writes the project's bodies before any
    /// of its documents reaches tsgo.
    fn ensure_project_for(&mut self, source: &Path) -> (Option<usize>, bool) {
        let source = crate::uri::canonicalize(source).unwrap_or_else(|_| source.to_path_buf());
        let Some(dir) = source.parent() else {
            return (None, false);
        };
        if let Some(&index) = self.resolved_dirs.get(dir) {
            return (Some(index), false);
        }
        let started = Instant::now();
        let (index, loaded) = match locate_project(&self.overlays, &source) {
            None => return (None, false),
            Some(ProjectLocation::Existing(index)) => match self.overlays[index].load() {
                Ok(loaded) => (index, loaded),
                Err(error) => {
                    log::warn(format_args!(
                        "could not load tsgo project {}: {error}",
                        self.overlays[index].workspace().display()
                    ));
                    (index, false)
                }
            },
            Some(ProjectLocation::Nested {
                config,
                mirror_root,
            }) => {
                let Some(root) = config.parent() else {
                    return (None, false);
                };
                match TsgoOverlay::build_nested(root, &config, &mirror_root) {
                    Ok(overlay) => {
                        self.overlays.push(overlay);
                        (self.overlays.len() - 1, true)
                    }
                    Err(error) => {
                        log::warn(format_args!(
                            "could not prepare tsgo overlay for {}: {error}",
                            root.display()
                        ));
                        return (None, false);
                    }
                }
            }
        };
        if loaded {
            let overlay = &self.overlays[index];
            let events = persist_bodies(overlay);
            log::info(format_args!(
                "loaded TypeScript project {} ({} components) in {} ms",
                overlay
                    .source_tsconfig()
                    .unwrap_or_else(|| overlay.workspace())
                    .display(),
                overlay.component_shadows().len(),
                started.elapsed().as_millis()
            ));
            let _ = self.client.notify_watched_files(events);
        }
        self.resolved_dirs.insert(dir.to_path_buf(), index);
        (Some(index), loaded)
    }

    fn overlay_for_source_mut(&mut self, source: &Path) -> Option<&mut TsgoOverlay> {
        let index = self.overlay_index_for_source(source)?;
        self.overlays.get_mut(index)
    }

    fn overlay_for_source(&self, source: &Path) -> Option<&TsgoOverlay> {
        self.overlays.get(self.overlay_index_for_source(source)?)
    }

    fn overlay_index_for_source(&self, source: &Path) -> Option<usize> {
        let source = crate::uri::canonicalize(source).unwrap_or_else(|_| source.to_path_buf());
        serving_index(&self.overlays, &source)
    }

    /// Every loaded overlay other than `owner` that holds a copy of `source`.
    fn copies_of(&self, owner: usize, source: &Path) -> Vec<usize> {
        self.overlays
            .iter()
            .enumerate()
            .filter(|(index, overlay)| {
                *index != owner
                    && overlay.is_loaded()
                    && overlay.shadow_for_source(source).is_some()
            })
            .map(|(index, _)| index)
            .collect()
    }

    /// The shadow of `source` in the project that serves it.
    fn owner_shadow(&self, source: &Path) -> Option<&tsgo_overlay::ShadowDocument> {
        self.overlay_for_source(source)?.shadow_for_source(source)
    }

    /// Set `overlays[index]` up for the editor, once: open the JavaScript
    /// components the editor documents `roots` reach, before the first of
    /// them reaches tsgo (see [`JAVASCRIPT_SHADOW`]). Returns whether it did.
    fn activate_project(&mut self, index: usize, roots: &[PathBuf]) -> bool {
        let overlay = &self.overlays[index];
        if !overlay.is_loaded() || !self.activated.insert(overlay.cache_dir().to_path_buf()) {
            return false;
        }
        self.open_reachable_javascript(index, roots.iter().cloned());
        true
    }

    /// Open, as buffers, the JavaScript components of `overlays[index]` that
    /// `roots` reach and tsgo does not hold yet. Their bodies are on disk
    /// too, so each open is a content change for tsgo rather than a new root
    /// file — but still one snapshot update each, which is why a component
    /// no open document reaches is left on disk.
    fn open_reachable_javascript(
        &mut self,
        index: usize,
        roots: impl IntoIterator<Item = PathBuf>,
    ) {
        let shadows = self.overlays[index]
            .reachable_javascript(roots)
            .into_iter()
            .filter(|shadow| !self.open_shadows.contains_key(shadow.shadow_uri.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        for shadow in &shadows {
            self.javascript_buffers
                .insert(shadow.shadow_uri.as_str().to_string());
            open_shadow(&self.client, &mut self.open_shadows, shadow);
        }
    }

    /// Hand tsgo a shadow `overlays[index]` projected from the file on disk:
    /// persist its body, and refresh the buffer when tsgo holds one.
    fn deliver_disk_shadow(
        &mut self,
        index: usize,
        shadow: &tsgo_overlay::ShadowDocument,
        events: &mut Vec<(Uri, lsp_types::FileChangeType)>,
    ) {
        let source = uri_to_path(shadow.source_uri.as_str());
        let overlay = &self.overlays[index];
        match overlay.persist_body(&source) {
            Ok(Some(change)) => events.push((shadow.shadow_uri.clone(), change)),
            Ok(None) => {}
            Err(error) => log::warn(format_args!(
                "could not persist tsgo shadow for {}: {error}",
                source.display()
            )),
        }
        let uri = shadow.shadow_uri.as_str();
        if self.javascript_buffers.contains(uri)
            || self.open_shadows.contains_key(uri)
            || self.is_keep_alive(uri)
        {
            open_shadow(&self.client, &mut self.open_shadows, shadow);
        }
    }

    /// Whether `uri` is a shadow held open only to keep its project in tsgo.
    fn is_keep_alive(&self, uri: &str) -> bool {
        self.keep_alive
            .values()
            .any(|(_, shadow)| shadow.as_str() == uri)
    }

    /// Hold one component shadow of `overlays[index]` open as a buffer, so
    /// tsgo keeps the project (see `keep_alive`), when every project is to
    /// be kept. A component the project owns, else a copy; the existing one
    /// stays while the project still holds it. Returns the shadow when it
    /// opened one now.
    fn keep_project_alive(&mut self, index: usize) -> Option<Uri> {
        let overlay = self.overlays.get(index)?;
        if !self.keep_all || !overlay.is_loaded() {
            return None;
        }
        let cache = overlay.cache_dir().to_path_buf();
        if let Some((source, uri)) = self.keep_alive.get(&cache)
            && overlay
                .shadow_for_source(source)
                .is_some_and(|shadow| &shadow.shadow_uri == uri)
        {
            return None;
        }
        let shadows = overlay.component_shadows();
        let shadow = shadows
            .iter()
            .find(|shadow| overlay.owns(&uri_to_path(shadow.source_uri.as_str())))
            .or_else(|| shadows.first())
            .map(|shadow| (*shadow).clone());
        let Some(shadow) = shadow else {
            self.keep_alive.remove(&cache);
            return None;
        };
        self.keep_alive.insert(
            cache,
            (
                uri_to_path(shadow.source_uri.as_str()),
                shadow.shadow_uri.clone(),
            ),
        );
        if self.open_shadows.contains_key(shadow.shadow_uri.as_str()) {
            return None;
        }
        open_shadow(&self.client, &mut self.open_shadows, &shadow);
        Some(shadow.shadow_uri)
    }

    /// The project that owns `source`, as a [`ProjectSpec`], whether or not
    /// it exists yet.
    fn project_spec_for(&self, source: &Path) -> Option<ProjectSpec> {
        let source = crate::uri::canonicalize(source).unwrap_or_else(|_| source.to_path_buf());
        match locate_project(&self.overlays, &source)? {
            ProjectLocation::Existing(index) => {
                let overlay = &self.overlays[index];
                Some(ProjectSpec {
                    config: (overlay.workspace() != overlay.mirror_root())
                        .then(|| overlay.source_tsconfig().map(Path::to_path_buf))
                        .flatten(),
                    mirror_root: overlay.mirror_root().to_path_buf(),
                })
            }
            ProjectLocation::Nested {
                config,
                mirror_root,
            } => Some(ProjectSpec {
                config: Some(config),
                mirror_root,
            }),
        }
    }

    /// Hand tsgo what an update of `overlays[index]` changed: shadows
    /// projected from disk are persisted, copies of editor buffers become
    /// buffers, and removed shadows are closed. The body changes are added
    /// to `events`, which the caller announces in one batch.
    fn deliver_update(
        &mut self,
        index: usize,
        update: &tsgo_overlay::OverlayUpdate,
        events: &mut Vec<(Uri, lsp_types::FileChangeType)>,
    ) {
        for shadow in &update.opened_or_changed {
            self.deliver_disk_shadow(index, shadow, events);
        }
        for shadow in &update.buffers {
            open_shadow(&self.client, &mut self.open_shadows, shadow);
        }
        for uri in &update.closed {
            if self.open_shadows.contains_key(uri.as_str()) {
                close_shadow(&self.client, &mut self.open_shadows, uri.clone());
            }
            events.push((uri.clone(), lsp_types::FileChangeType::DELETED));
        }
    }

    fn completion_document_context(
        &self,
        params: &serde_json::Value,
    ) -> Option<RequestDocumentContext> {
        let file_name = params
            .get("data")?
            .get("fileName")?
            .as_str()
            .map(Path::new)?;
        let mapper = TsgoResponseMapper::for_overlays(&self.overlays);
        for overlay in &self.overlays {
            let Some(source) = overlay.source_for_shadow(file_name) else {
                continue;
            };
            let Some(shadow) = overlay.shadow_for_source(source) else {
                continue;
            };
            let uri = &shadow.source_uri;
            if let Some(context) = mapper.document_context(uri) {
                return Some(context);
            }
        }
        None
    }
}

/// The projects loaded in the background (see [`background`]).
struct BackgroundProjects {
    worker: Option<background::Worker>,
    /// Bumped whenever the overlays are rebuilt or the folders change, so a
    /// result for the old set is dropped.
    generation: u64,
    /// The projects still to load, once enumerated.
    queue: Option<VecDeque<ProjectSpec>>,
    enumerating: bool,
    in_flight: Option<ProjectSpec>,
    /// File events that arrived while a load was in flight: its overlay was
    /// built from the disk before them.
    missed_events: Vec<(PathBuf, lsp_types::FileChangeType)>,
    /// The request that has tsgo build the project loaded last.
    warming: Option<RequestId>,
    /// Every project that owns a component is loaded.
    complete: bool,
    /// The editor's last message.
    last_activity: Instant,
}

impl BackgroundProjects {
    fn new() -> Self {
        Self {
            worker: background::Worker::spawn(),
            generation: 0,
            queue: None,
            enumerating: false,
            in_flight: None,
            missed_events: Vec::new(),
            warming: None,
            complete: false,
            last_activity: Instant::now(),
        }
    }

    /// Start over, for a new set of overlays. The caller has waited for the
    /// job in flight, which writes into the same caches.
    fn reset(&mut self) {
        self.generation += 1;
        self.queue = None;
        self.enumerating = false;
        self.in_flight = None;
        self.missed_events.clear();
        self.warming = None;
        self.complete = false;
    }
}

/// Requests whose answer spans projects: tsgo searches only the projects it
/// has, so every project that owns a component is loaded first.
fn is_workspace_wide(method: &str) -> bool {
    matches!(
        method,
        "workspace/symbol"
            | "textDocument/references"
            | "textDocument/implementation"
            | "textDocument/rename"
            | "callHierarchy/incomingCalls"
            | "callHierarchy/outgoingCalls"
            | "workspace/willRenameFiles"
    )
}

struct Server {
    sender: Sender<Message>,
    client: ClientState,
    settings: Settings,
    js_ts_settings: serde_json::Value,
    typescript_settings: serde_json::Value,
    javascript_settings: serde_json::Value,
    editor_settings: serde_json::Value,
    documents: DocumentStore,
    worker: Worker,
    outcomes: Receiver<Outcome>,
    /// Documents awaiting a lint, and when it comes due.
    scheduled: HashMap<String, Instant>,
    /// The content hash each document was last linted at.
    linted: HashMap<String, u64>,
    pending: HashMap<RequestId, Pending>,
    joins: HashMap<RequestId, Join>,
    /// [`TSGO_ANSWER_TIMEOUT`], or `RSVELTE_TSGO_REQUEST_TIMEOUT_MS`.
    tsgo_answer_timeout: Duration,
    /// The restarts for requests tsgo loops on ([`Server::restart_deadline`]).
    tsgo_restarts: RestartBackoff,
    pending_tsgo: HashMap<RequestId, PendingTsgoRequest>,
    /// tsgo's own completion `data`, by entry name, for the sites whose items
    /// currently carry upstream's `{name, uri, position}` payload instead.
    completion_data: HashMap<String, HashMap<String, serde_json::Value>>,
    /// Insertion order of `completion_data`, oldest first.
    completion_order: Vec<String>,
    rename_aggregates: HashMap<RequestId, RenameAggregate>,
    store_references: StoreReferences,
    component_imports: ComponentImports,
    component_queries: HashMap<RequestId, PendingComponentQuery>,
    component_query_requests: HashMap<RequestId, (RequestId, ComponentInfoRequestId)>,
    /// What waits for a probe in a shadow's buffer to end (see
    /// [`ComponentInfoQuery::in_place`]).
    deferred: Vec<Deferred>,
    outgoing: HashMap<RequestId, Outgoing>,
    next_request_id: u32,
    tsgo: Option<TsgoRuntime>,
    preprocess: Option<PreprocessRuntime>,
    preprocess_events: Receiver<PreprocessEvent>,
    preprocess_event_sender: Sender<PreprocessEvent>,
    preprocess_failures: HashMap<PathBuf, (i32, String)>,
    preprocess_documents: HashMap<PathBuf, PreprocessDocumentState>,
    /// The version each component was last sent to the sidecar at, until it
    /// answers that version ([`Self::awaiting_preprocess`]).
    preprocess_pending: HashMap<PathBuf, i32>,
    preprocess_dependencies: HashMap<PathBuf, HashSet<PathBuf>>,
    watched_preprocess_directories: HashSet<PathBuf>,
    /// Which Svelte config preprocesses each directory's documents.
    preprocess_configs: PreprocessConfigCache,
    /// No shadow and no open document has a Svelte config, so there is no
    /// sidecar to start. Without it every keystroke re-asks for every shadow.
    /// Cleared whenever the answer can change: a config, setting or folder
    /// change, a new project, a new source, or an editor document whose
    /// directory has a config.
    preprocess_known_empty: bool,
    initialize_params: serde_json::Value,
    background: BackgroundProjects,
    /// tsgo's watcher registrations, so the editor gets only those that add
    /// an event.
    tsgo_watchers: TsgoWatchers,
    shutdown_requested: bool,
    exiting: bool,
}

impl Server {
    fn new(
        sender: Sender<Message>,
        client: ClientState,
        worker: Worker,
        outcomes: Receiver<Outcome>,
        tsgo: Option<TsgoRuntime>,
        initialize_params: serde_json::Value,
    ) -> Self {
        let (preprocess_event_sender, preprocess_events) = unbounded();
        let settings = if client.pull_configuration {
            Settings::default()
        } else {
            Settings::from_sections(
                initialize_params
                    .pointer("/initializationOptions/configuration/rsvelte")
                    .unwrap_or(&serde_json::Value::Null),
                initialize_params
                    .pointer("/initializationOptions/configuration/svelte")
                    .unwrap_or(&serde_json::Value::Null),
            )
            .with_emmet(
                initialize_params
                    .pointer("/initializationOptions/configuration/emmet")
                    .or_else(|| initialize_params.pointer("/initializationOptions/emmetConfig"))
                    .unwrap_or(&serde_json::Value::Null),
            )
        };
        Self {
            sender,
            client,
            settings,
            js_ts_settings: serde_json::Value::Null,
            typescript_settings: serde_json::Value::Null,
            javascript_settings: serde_json::Value::Null,
            editor_settings: serde_json::Value::Null,
            documents: DocumentStore::default(),
            worker,
            outcomes,
            scheduled: HashMap::new(),
            linted: HashMap::new(),
            pending: HashMap::new(),
            joins: HashMap::new(),
            tsgo_answer_timeout: std::env::var("RSVELTE_TSGO_REQUEST_TIMEOUT_MS")
                .ok()
                .and_then(|ms| ms.parse().ok())
                .map_or(TSGO_ANSWER_TIMEOUT, Duration::from_millis),
            tsgo_restarts: RestartBackoff::default(),
            pending_tsgo: HashMap::new(),
            completion_data: HashMap::new(),
            completion_order: Vec::new(),
            rename_aggregates: HashMap::new(),
            store_references: StoreReferences::default(),
            component_imports: ComponentImports::default(),
            component_queries: HashMap::new(),
            component_query_requests: HashMap::new(),
            deferred: Vec::new(),
            outgoing: HashMap::new(),
            next_request_id: 0,
            tsgo,
            preprocess: None,
            preprocess_events,
            preprocess_event_sender,
            preprocess_failures: HashMap::new(),
            preprocess_documents: HashMap::new(),
            preprocess_pending: HashMap::new(),
            preprocess_dependencies: HashMap::new(),
            watched_preprocess_directories: HashSet::new(),
            preprocess_configs: PreprocessConfigCache::default(),
            preprocess_known_empty: false,
            initialize_params,
            background: BackgroundProjects::new(),
            tsgo_watchers: TsgoWatchers::default(),
            shutdown_requested: false,
            exiting: false,
        }
    }

    fn run(&mut self, connection: &Connection) -> Result<ExitCode> {
        if self.client.pull_configuration {
            self.request_configuration();
        } else {
            self.ensure_preprocess_runtime();
        }
        if self.client.dynamic_watched_files {
            self.register_watched_files();
        }
        // Cloned out of `self` so the handlers below can still borrow it.
        let outcomes = self.outcomes.clone();
        let preprocess_events = self.preprocess_events.clone();
        let tsgo_events = self
            .tsgo
            .as_ref()
            .map(|runtime| runtime.client.events().clone())
            .unwrap_or_else(never);
        let background_done = self
            .background
            .worker
            .as_ref()
            .map(|worker| worker.done().clone())
            .unwrap_or_else(never);

        while !self.exiting {
            // Rearmed each turn: the debounce deadline moves with every edit,
            // and the background waits for the editor to be quiet.
            let timer = match self
                .scheduled
                .values()
                .copied()
                .chain(self.background_deadline())
                .chain(self.tsgo_request_deadline())
                .min()
            {
                Some(deadline) => after(deadline.saturating_duration_since(Instant::now())),
                None => never(),
            };
            select! {
                recv(connection.receiver) -> message => {
                    // The editor's own requests and notifications, not its
                    // answers to the server's (a watcher registration tsgo
                    // made while a background project built).
                    if !matches!(message, Ok(Message::Response(_))) {
                        self.background.last_activity = Instant::now();
                    }
                    match message {
                        Ok(Message::Request(request)) => self.on_request(request),
                        Ok(Message::Notification(notification)) => self.on_notification(notification),
                        Ok(Message::Response(response)) => self.on_response(response),
                        Err(_) => break,
                    }
                },
                recv(background_done) -> done => match done {
                    Ok(done) => self.on_background_done(done),
                    Err(_) => self.background.worker = None,
                },
                recv(outcomes) -> outcome => match outcome {
                    Ok(outcome) => self.on_outcome(outcome),
                    Err(_) => break,
                },
                recv(tsgo_events) -> event => match event {
                    Ok(event) => self.on_tsgo_event(event),
                    Err(_) => self.tsgo = None,
                },
                recv(preprocess_events) -> event => match event {
                    Ok(event) => self.on_preprocess_event(event),
                    Err(_) => self.preprocess = None,
                },
                recv(timer) -> _ => {
                    self.run_scheduled_lints();
                    // tsgo's answers wait in the channel while the loop is
                    // busy elsewhere: one tsgo already sent is not lost,
                    // however late the loop gets to it.
                    if self
                        .tsgo_request_deadline()
                        .is_some_and(|deadline| deadline <= Instant::now())
                    {
                        while let Ok(event) = tsgo_events.try_recv() {
                            self.on_tsgo_event(event);
                        }
                    }
                    self.expire_tsgo_requests();
                    self.step_background();
                },
            }
        }

        // A client that drops the connection or exits without shutting down
        // first is an abnormal end, per the protocol.
        Ok(if self.shutdown_requested {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        })
    }

    fn on_request(&mut self, request: Request) {
        if request.method == "shutdown" {
            self.shutdown_requested = true;
            self.respond(Response::new_ok(request.id, ()));
            return;
        }
        if self.shutdown_requested {
            self.respond(Response::new_err(
                request.id,
                ErrorCode::InvalidRequest as i32,
                "server is shutting down".to_string(),
            ));
            return;
        }
        if self.probe_in_flight() {
            self.deferred.push(Deferred::Request(request));
            return;
        }
        match request.method.as_str() {
            "textDocument/formatting" => self.on_formatting(request),
            "textDocument/completion" => self.on_completion(request),
            "textDocument/hover" => self.on_hover(request),
            "textDocument/codeAction" => self.on_code_action(request),
            "textDocument/codeLens" => self.on_code_lens(request),
            "workspace/executeCommand" => self.on_execute_command(request),
            "textDocument/foldingRange" => self.on_folding_range(request),
            "textDocument/selectionRange" => self.on_selection_range(request),
            "textDocument/documentSymbol" => self.on_document_symbol(request),
            "textDocument/linkedEditingRange" => self.on_linked_editing_range(request),
            "textDocument/documentHighlight" => self.on_document_highlight(request),
            "html/tag" => self.on_tag_close(request),
            "textDocument/documentColor" => self.on_document_color(request),
            "textDocument/colorPresentation" => self.on_color_presentation(request),
            "textDocument/diagnostic" => self.on_document_diagnostic(request),
            // `server.ts`'s `onCompletionResolve` answers an item without
            // `data` — every Svelte, HTML, CSS and Emmet item — with the item
            // itself; only TypeScript's items carry any to resolve with.
            "completionItem/resolve" if !has_resolve_data(&request.params) => {
                self.respond(Response::new_ok(request.id, request.params));
            }
            "completionItem/resolve"
            | "textDocument/definition"
            | "textDocument/typeDefinition"
            | "textDocument/implementation"
            | "textDocument/references"
            | "textDocument/signatureHelp"
            | "textDocument/semanticTokens/full"
            | "textDocument/semanticTokens/range"
            | "textDocument/prepareCallHierarchy"
            | "callHierarchy/incomingCalls"
            | "callHierarchy/outgoingCalls"
            | "workspace/symbol"
            | "workspaceSymbol/resolve" => self.forward_tsgo_request(request),
            "textDocument/inlayHint" => self.on_inlay_hint(request),
            "textDocument/prepareRename" => self.on_prepare_rename(request),
            "textDocument/rename" => self.on_rename(request),
            "$/getFileReferences" => self.on_get_file_references(request),
            "$/getComponentReferences" => self.on_get_component_references(request),
            "$/getEditsForFileRename" => self.on_get_edits_for_file_rename(request),
            "$/getCompiledCode" => self.on_get_compiled_code(request),
            "codeLens/resolve" => self.on_resolve_code_lens(request),
            _ => self.respond(Response::new_err(
                request.id,
                ErrorCode::MethodNotFound as i32,
                format!("unhandled method {}", request.method),
            )),
        }
    }

    fn on_formatting(&mut self, request: Request) {
        let id = request.id;
        let params = match serde_json::from_value::<DocumentFormattingParams>(request.params) {
            Ok(params) => params,
            Err(err) => {
                // "I could not read your request" is not "I have nothing to
                // change": every other branch below answers no edits, so a
                // request the server never parsed must be distinguishable.
                self.respond(Response::new_err(
                    id,
                    ErrorCode::InvalidParams as i32,
                    format!("textDocument/formatting: {err}"),
                ));
                return;
            }
        };
        if !self.settings.format_enable {
            self.respond_no_edits(id);
            return;
        }
        let uri = params.text_document.uri;
        let Some(document) = self.documents.get(&uri) else {
            self.respond_no_edits(id);
            return;
        };
        let job = Job::Format {
            id: id.clone(),
            path: uri_to_path(uri.as_str()),
            text: document.shared_text(),
            range: document.full_range(),
            config: self.settings.format_config.clone(),
        };
        self.pending.insert(id, Pending::Formatting);
        self.worker.submit(job);
    }

    fn on_get_compiled_code(&mut self, request: Request) {
        let id = request.id;
        let Some(uri) = custom_request_uri(&request.params) else {
            self.respond_nothing(id);
            return;
        };
        let Some(document) = self.component_document(&uri) else {
            self.respond_nothing(id);
            return;
        };
        let path = uri_to_path(uri.as_str());
        let canonical = crate::uri::canonicalize(&path).unwrap_or_else(|_| path.clone());
        let processed = self
            .preprocess_documents
            .get(&canonical)
            .filter(|processed| processed.version == document.version);
        let (text, sourcemap) = processed.map_or_else(
            || (document.shared_text(), None),
            |processed| {
                (
                    Arc::clone(&processed.text),
                    processed.map.as_ref().map(Arc::clone),
                )
            },
        );
        self.pending.insert(id.clone(), Pending::CompiledCode);
        self.worker.submit(Job::Compile {
            id,
            path,
            text,
            sourcemap,
        });
    }

    fn on_document_diagnostic(&mut self, request: Request) {
        let tsgo_fallback = request.clone();
        let id = request.id;
        let params = match serde_json::from_value::<DocumentDiagnosticParams>(request.params) {
            Ok(params) => params,
            Err(err) => {
                log::warn(format_args!("textDocument/diagnostic: {err}"));
                self.respond_diagnostic_report(id, Vec::new());
                return;
            }
        };
        let uri = params.text_document.uri;
        let Some(document) = self.documents.get(&uri) else {
            self.forward_tsgo_request(tsgo_fallback);
            return;
        };
        if !self.settings.lint_enable || !is_lint_target(document) {
            self.forward_tsgo_request(tsgo_fallback);
            return;
        }
        if self.awaiting_preprocess(&uri_to_path(uri.as_str()), document.version) {
            // tsgo's half alone; the sidecar's answer asks the editor to pull again.
            self.join(id.clone(), tsgo_fallback);
            self.join_native(id, serde_json::to_value(diagnostic_report(Vec::new())).ok());
            return;
        }
        self.worker.submit(Job::PullDiagnostics {
            id: id.clone(),
            path: uri_to_path(uri.as_str()),
            text: document.shared_text(),
            preprocessed: self.preprocessed_analysis(&uri_to_path(uri.as_str()), document.version),
            warnings: self.settings.compiler_warnings.clone(),
            svelte_diagnostics: self.settings.svelte.enable && self.settings.svelte.diagnostics,
            css_diagnostics: self.settings.css.enable && self.settings.css.diagnostics,
        });
        self.join(id, tsgo_fallback);
    }

    fn on_completion(&mut self, request: Request) {
        let tsgo_fallback = request.clone();
        let id = request.id;
        let (params, trigger_character) =
            match serde_json::from_value::<CompletionParams>(request.params) {
                Ok(params) => (
                    params.text_document_position,
                    params
                        .context
                        .filter(|context| {
                            context.trigger_kind == CompletionTriggerKind::TRIGGER_CHARACTER
                        })
                        .and_then(|context| context.trigger_character),
                ),
                Err(err) => {
                    log::warn(format_args!("textDocument/completion: {err}"));
                    self.respond(Response::new_ok(id, empty_completion_list()));
                    return;
                }
            };
        // `rsvelte.completion.enable` has no upstream counterpart, so
        // `PluginHost.ts:298` — which is about every plugin declining — does not
        // govern a server the user configured not to answer at all.
        if !self.settings.completion_enable {
            self.respond_nothing(id);
            return;
        }
        if !self.settings.native_completion_enabled() {
            self.forward_tsgo_request(tsgo_fallback);
            return;
        }
        match self.locate(&params) {
            Some((path, text, offset)) => {
                let filter = self
                    .client
                    .filter_incomplete_completions
                    .then(|| incomplete_completion_filter_at(&text, offset));
                self.pending.insert(
                    id.clone(),
                    Pending::Completion {
                        tsgo_fallback,
                        filter,
                    },
                );
                self.worker.submit(Job::Complete {
                    id,
                    path,
                    text,
                    offset,
                    strict_mode: self.settings.format_config.strict_mode.unwrap_or(false),
                    markdown_documentation: self.client.markdown_documentation,
                    emmet: self.settings.emmet_gates(),
                    trigger_character,
                });
            }
            None => self.forward_tsgo_request(tsgo_fallback),
        }
    }

    fn on_hover(&mut self, request: Request) {
        let tsgo_fallback = request.clone();
        let id = request.id;
        let params = match serde_json::from_value::<HoverParams>(request.params) {
            Ok(params) => params.text_document_position_params,
            Err(err) => {
                log::warn(format_args!("textDocument/hover: {err}"));
                self.respond_nothing(id);
                return;
            }
        };
        if !self.settings.hover_enable {
            self.respond_nothing(id);
            return;
        }
        if !self.settings.native_hover_enabled() {
            self.forward_tsgo_request(tsgo_fallback);
            return;
        }
        match self.locate(&params) {
            Some((path, text, offset)) => {
                self.pending
                    .insert(id.clone(), Pending::Hover { tsgo_fallback });
                self.worker.submit(Job::Hover {
                    id,
                    path,
                    text,
                    offset,
                    markdown_hover: self.client.markdown_hover,
                });
            }
            None => self.forward_tsgo_request(tsgo_fallback),
        }
    }

    fn on_prepare_rename(&mut self, request: Request) {
        let Ok(params) =
            serde_json::from_value::<TextDocumentPositionParams>(request.params.clone())
        else {
            self.respond_nothing(request.id);
            return;
        };
        let path = uri_to_path(params.text_document.uri.as_str());
        if !is_svelte_document("", &path) {
            self.forward_tsgo_request(request);
            return;
        }
        let Some(plan) = self.svelte_rename_plan(&params) else {
            self.respond_nothing(request.id);
            return;
        };
        self.forward_rename_request(request.id, "textDocument/prepareRename", &plan, None);
    }

    fn on_rename(&mut self, request: Request) {
        let Ok(params) = serde_json::from_value::<RenameParams>(request.params.clone()) else {
            self.respond_nothing(request.id);
            return;
        };
        let position = params.text_document_position.clone();
        let path = uri_to_path(position.text_document.uri.as_str());
        if !is_svelte_document("", &path) {
            self.forward_tsgo_request(request);
            return;
        }
        let Some(plan) = self.svelte_rename_plan(&position) else {
            self.respond_nothing(request.id);
            return;
        };
        self.forward_rename_request(
            request.id,
            "textDocument/rename",
            &plan,
            Some(params.new_name),
        );
    }

    fn svelte_rename_plan(&self, params: &TextDocumentPositionParams) -> Option<PrepareRenamePlan> {
        let runtime = self.tsgo.as_ref()?;
        let source_path = uri_to_path(params.text_document.uri.as_str());
        let canonical_source =
            crate::uri::canonicalize(&source_path).unwrap_or_else(|_| source_path.clone());
        let source = self.documents.get(&params.text_document.uri)?.text();
        let overlay = runtime
            .overlays
            .iter()
            .filter(|overlay| canonical_source.starts_with(overlay.workspace()))
            .max_by_key(|overlay| overlay.workspace().components().count())?;
        let shadow = overlay.shadow_for_source(&source_path)?;
        let document = RenameDocument {
            source_uri: &shadow.source_uri,
            shadow_uri: &shadow.shadow_uri,
            source_text: source,
            generated_text: &shadow.text,
            projection_map: overlay.projection_map(&source_path)?,
            source_map: overlay.source_map(&source_path),
            parser_error: document_has_parser_error(source),
        };
        Some(prepare_rename_plan(document, params.position))
    }

    fn forward_rename_request(
        &mut self,
        id: RequestId,
        method: &str,
        plan: &PrepareRenamePlan,
        new_name: Option<String>,
    ) {
        let Some((uri, position, _)) = plan.request() else {
            self.respond_nothing(id);
            return;
        };
        if is_workspace_wide(method) {
            self.load_all_projects();
        }
        let Some(runtime) = &self.tsgo else {
            self.respond_nothing(id);
            return;
        };
        let mut params = serde_json::json!({
            "textDocument": { "uri": uri },
            "position": position,
        });
        if let Some(name) = &new_name {
            params["newName"] = serde_json::Value::String(name.clone());
        }
        let rename = match new_name {
            Some(new_name) => PendingRename::Primary {
                plan: plan.clone(),
                new_name,
            },
            None => PendingRename::Prepare(plan.clone()),
        };
        let pending = PendingTsgoRequest {
            method: method.to_string(),
            document: None,
            fallback_result: None,
            completion_site: None,
            rename: Some(rename),
            code_action_diagnostic_codes: Vec::new(),
            push_diagnostics: None,
            component_site: None,
            component_references: None,
            file_rename: None,
            code_lens_resolve: None,
            completion_site_data: None,
            include_declaration: None,
            component_imports: Vec::new(),
        };
        let child = Request::new(id.clone(), method.to_string(), params);
        if let Err(error) = runtime.forward(child.into()) {
            log::warn(format_args!("could not forward rename to tsgo: {error}"));
            self.respond_nothing(id);
            return;
        }
        self.pending_tsgo.insert(id, pending);
    }

    fn on_get_file_references(&mut self, request: Request) {
        let Some(uri) = custom_request_uri(&request.params) else {
            self.respond(Response::new_ok(
                request.id,
                Vec::<lsp_types::Location>::new(),
            ));
            return;
        };
        let roots = self
            .tsgo
            .as_ref()
            .map(|runtime| {
                runtime
                    .overlays
                    .iter()
                    .map(|overlay| overlay.workspace().to_path_buf())
                    .collect()
            })
            .unwrap_or_default();
        let open_documents = self
            .documents
            .iter()
            .map(|document| {
                let path = uri_to_path(document.uri.as_str());
                FileReferenceSource {
                    path: crate::uri::canonicalize(&path).unwrap_or(path),
                    uri: document.uri.clone(),
                    text: document.text().to_string(),
                }
            })
            .collect();
        let id = request.id;
        self.pending.insert(id.clone(), Pending::FileReferences);
        self.worker.submit(Job::FileReferences {
            id,
            target: uri_to_path(uri.as_str()),
            roots,
            open_documents,
        });
    }

    fn on_get_component_references(&mut self, request: Request) {
        let Some(source_uri) = custom_request_uri(&request.params) else {
            self.respond(Response::new_ok(
                request.id,
                Vec::<lsp_types::Location>::new(),
            ));
            return;
        };
        let source_path = uri_to_path(source_uri.as_str());
        self.ensure_tsgo_project(&source_path);
        self.load_all_projects();
        let Some(runtime) = &self.tsgo else {
            self.respond(Response::new_ok(
                request.id,
                Vec::<lsp_types::Location>::new(),
            ));
            return;
        };
        let Some(shadow) = runtime.owner_shadow(&source_path) else {
            self.respond(Response::new_ok(
                request.id,
                Vec::<lsp_types::Location>::new(),
            ));
            return;
        };
        let Some(position) = component_probe_position(&shadow.text) else {
            self.respond(Response::new_ok(
                request.id,
                Vec::<lsp_types::Location>::new(),
            ));
            return;
        };
        let mapper = TsgoResponseMapper::for_overlays(&runtime.overlays);
        let document = mapper.document_context(&source_uri);
        let child = Request::new(
            request.id.clone(),
            "textDocument/references".to_string(),
            serde_json::json!({
                "textDocument": { "uri": shadow.shadow_uri },
                "position": position,
                "context": { "includeDeclaration": false },
            }),
        );
        let pending = PendingTsgoRequest {
            method: "textDocument/references".to_string(),
            document,
            fallback_result: None,
            completion_site: None,
            rename: None,
            code_action_diagnostic_codes: Vec::new(),
            push_diagnostics: None,
            component_site: None,
            component_references: Some(source_uri),
            file_rename: None,
            code_lens_resolve: None,
            completion_site_data: None,
            include_declaration: None,
            component_imports: Vec::new(),
        };
        if runtime.forward(child.into()).is_ok() {
            self.pending_tsgo.insert(request.id, pending);
        } else {
            self.respond(Response::new_ok(
                request.id,
                Vec::<lsp_types::Location>::new(),
            ));
        }
    }

    fn on_get_edits_for_file_rename(&mut self, request: Request) {
        if !self.settings.svelte.enable || !self.settings.svelte.rename {
            self.respond_nothing(request.id);
            return;
        }
        let Some(old_uri) = request
            .params
            .get("oldUri")
            .and_then(serde_json::Value::as_str)
            .and_then(|uri| uri.parse::<Uri>().ok())
        else {
            self.respond_nothing(request.id);
            return;
        };
        let Some(new_uri) = request
            .params
            .get("newUri")
            .and_then(serde_json::Value::as_str)
            .and_then(|uri| uri.parse::<Uri>().ok())
        else {
            self.respond_nothing(request.id);
            return;
        };
        let old_path = uri_to_path(old_uri.as_str());
        let new_path = uri_to_path(new_uri.as_str());
        self.ensure_tsgo_project(&old_path);
        self.load_all_projects();
        let Some(runtime) = &self.tsgo else {
            self.respond_nothing(request.id);
            return;
        };
        let Some((old_shadow, new_shadow)) =
            runtime.overlay_for_source(&old_path).and_then(|overlay| {
                let old_shadow = overlay.shadow_for_source(&old_path)?.shadow_uri.clone();
                let new_shadow = overlay.prospective_shadow_uri(&new_path).ok()?;
                Some((old_shadow, new_shadow))
            })
        else {
            self.forward_tsgo_request(Request::new(
                request.id,
                "workspace/willRenameFiles".to_string(),
                serde_json::json!({
                    "files": [{ "oldUri": old_uri, "newUri": new_uri }]
                }),
            ));
            return;
        };
        let mapping = WillRenameMapping {
            old: ShadowUriPair {
                source_uri: &old_uri,
                shadow_uri: &old_shadow,
            },
            new: ShadowUriPair {
                source_uri: &new_uri,
                shadow_uri: &new_shadow,
            },
        };
        let mut params = serde_json::json!({
            "files": [{ "oldUri": old_uri, "newUri": new_uri }]
        });
        rewrite_will_rename_params(&mut params, &[mapping]);
        let child = Request::new(
            request.id.clone(),
            "workspace/willRenameFiles".to_string(),
            params,
        );
        let pending = PendingTsgoRequest {
            method: "workspace/willRenameFiles".to_string(),
            document: None,
            fallback_result: None,
            completion_site: None,
            rename: None,
            code_action_diagnostic_codes: Vec::new(),
            push_diagnostics: None,
            component_site: None,
            component_references: None,
            file_rename: Some(PendingFileRename {
                old_source: old_uri,
                old_shadow,
                new_source: new_uri,
                new_shadow,
            }),
            code_lens_resolve: None,
            completion_site_data: None,
            include_declaration: None,
            component_imports: Vec::new(),
        };
        if runtime.forward(child.into()).is_ok() {
            self.pending_tsgo.insert(request.id, pending);
        } else {
            self.respond_nothing(request.id);
        }
    }

    fn on_linked_editing_range(&mut self, request: Request) {
        let id = request.id;
        if !self.settings.html.enable || !self.settings.html.linked_editing {
            self.respond_nothing(id);
            return;
        }
        let params = match serde_json::from_value::<LinkedEditingRangeParams>(request.params) {
            Ok(params) => params.text_document_position_params,
            Err(err) => {
                log::warn(format_args!("textDocument/linkedEditingRange: {err}"));
                self.respond_nothing(id);
                return;
            }
        };
        let Some((_, text, offset)) = self.locate(&params) else {
            self.respond_nothing(id);
            return;
        };
        self.respond(Response::new_ok(
            id,
            crate::html_tags::linked_ranges(text.as_str(), offset),
        ));
    }

    fn on_document_highlight(&mut self, request: Request) {
        let tsgo_fallback = request.clone();
        let id = request.id;
        if !self.settings.svelte.enable || !self.settings.svelte.document_highlight {
            self.respond(Response::new_ok(
                id,
                Vec::<lsp_types::DocumentHighlight>::new(),
            ));
            return;
        }
        let params = match serde_json::from_value::<DocumentHighlightParams>(request.params) {
            Ok(params) => params.text_document_position_params,
            Err(err) => {
                log::warn(format_args!("textDocument/documentHighlight: {err}"));
                self.respond(Response::new_ok(
                    id,
                    Vec::<lsp_types::DocumentHighlight>::new(),
                ));
                return;
            }
        };
        let highlights = self
            .locate(&params)
            .map_or_else(Vec::new, |(_, text, offset)| {
                crate::html_tags::highlights(text.as_str(), offset)
            });
        if highlights.is_empty() {
            self.forward_tsgo_request(tsgo_fallback);
        } else {
            self.respond(Response::new_ok(id, highlights));
        }
    }

    fn on_tag_close(&mut self, request: Request) {
        let id = request.id;
        if !self.settings.html.enable || !self.settings.html.tag_complete {
            self.respond_nothing(id);
            return;
        }
        let Ok(params) = serde_json::from_value::<TextDocumentPositionParams>(request.params)
        else {
            self.respond_nothing(id);
            return;
        };
        let completion = self
            .locate(&params)
            .and_then(|(_, text, offset)| crate::html_tags::close_tag(&text, offset));
        self.respond(Response::new_ok(id, completion));
    }

    fn on_document_color(&mut self, request: Request) {
        let id = request.id;
        if !self.settings.css.enable || !self.settings.css.document_colors {
            self.respond(Response::new_ok(
                id,
                Vec::<lsp_types::ColorInformation>::new(),
            ));
            return;
        }
        let Ok(params) = serde_json::from_value::<DocumentColorParams>(request.params) else {
            self.respond(Response::new_ok(
                id,
                Vec::<lsp_types::ColorInformation>::new(),
            ));
            return;
        };
        let colors = self
            .documents
            .get(&params.text_document.uri)
            .map_or_else(Vec::new, |document| crate::css::colors(document.text()));
        self.respond(Response::new_ok(id, colors));
    }

    fn on_color_presentation(&mut self, request: Request) {
        let id = request.id;
        if !self.settings.css.enable || !self.settings.css.color_presentations {
            self.respond(Response::new_ok(
                id,
                Vec::<lsp_types::ColorPresentation>::new(),
            ));
            return;
        }
        let Ok(params) = serde_json::from_value::<ColorPresentationParams>(request.params) else {
            self.respond(Response::new_ok(
                id,
                Vec::<lsp_types::ColorPresentation>::new(),
            ));
            return;
        };
        self.respond(Response::new_ok(
            id,
            crate::css::color_presentations(params.color),
        ));
    }

    /// Resolve a position in an open component to what the worker needs. Only
    /// components have Svelte template syntax to answer for.
    fn locate(
        &self,
        params: &TextDocumentPositionParams,
    ) -> Option<(std::path::PathBuf, std::sync::Arc<String>, usize)> {
        let document = self.documents.get(&params.text_document.uri)?;
        if document.language_id != "svelte" {
            return None;
        }
        Some((
            uri_to_path(params.text_document.uri.as_str()),
            document.shared_text(),
            document.offset_at(params.position),
        ))
    }

    fn on_code_action(&mut self, request: Request) {
        let tsgo_fallback = request.clone();
        let id = request.id;
        let params = match serde_json::from_value::<CodeActionParams>(request.params) {
            Ok(params) => params,
            Err(err) => {
                log::warn(format_args!("textDocument/codeAction: {err}"));
                self.respond_no_actions(id);
                return;
            }
        };
        let uri = params.text_document.uri;
        let Some(document) = self.component_document(&uri) else {
            self.forward_tsgo_request(tsgo_fallback);
            return;
        };
        if !self.settings.svelte.enable || !self.settings.svelte.code_actions {
            self.forward_tsgo_request(tsgo_fallback);
            return;
        }
        let only = params.context.only.as_ref();
        let job = Job::CodeAction {
            id: id.clone(),
            path: uri_to_path(uri.as_str()),
            text: document.shared_text(),
            uri,
            diagnostics: params.context.diagnostics,
            quickfix: only.is_none_or(|kinds| kinds.contains(&CodeActionKind::QUICKFIX)),
            suggestions: only.is_none_or(|kinds| kinds.contains(&CodeActionKind::REFACTOR_REWRITE)),
            fix_all: only.is_none_or(|kinds| {
                kinds.contains(&CodeActionKind::from(crate::code_actions::FIX_ALL_KIND))
            }),
        };
        self.worker.submit(job);
        self.join(id, tsgo_fallback);
    }

    fn on_code_lens(&mut self, request: Request) {
        let tsgo_fallback = request.clone();
        let id = request.id;
        let Ok(params) = serde_json::from_value::<CodeLensParams>(request.params) else {
            return self.respond_no_lenses(id);
        };
        if !self.settings.runes_legacy_mode_code_lens_enable {
            return self.forward_tsgo_request(tsgo_fallback);
        }
        let Some((path, text)) = self.component(&params.text_document.uri) else {
            return self.forward_tsgo_request(tsgo_fallback);
        };
        self.worker.submit(Job::CodeLens {
            id: id.clone(),
            path,
            text,
        });
        self.join(id, tsgo_fallback);
    }

    fn on_resolve_code_lens(&mut self, request: Request) {
        let id = request.id;
        let lens = request.params;
        let Some(kind) = code_lens_kind(&lens) else {
            self.respond(Response::new_ok(id, lens));
            return;
        };
        let Some(source_uri) = lens
            .pointer("/data/uri")
            .and_then(serde_json::Value::as_str)
            .and_then(|uri| uri.parse::<Uri>().ok())
        else {
            self.respond(Response::new_ok(id, lens));
            return;
        };
        let Some(runtime) = &self.tsgo else {
            self.respond(Response::new_ok(id, lens));
            return;
        };
        let source_path = uri_to_path(source_uri.as_str());
        let component_lens = kind == CodeLensKind::Reference
            && source_path
                .extension()
                .is_some_and(|extension| extension == "svelte")
            && lens
                .pointer("/range/start/line")
                .and_then(serde_json::Value::as_u64)
                == Some(0)
            && lens
                .pointer("/range/start/character")
                .and_then(serde_json::Value::as_u64)
                == Some(0)
            && lens
                .pointer("/range/end/line")
                .and_then(serde_json::Value::as_u64)
                == Some(0)
            && lens
                .pointer("/range/end/character")
                .and_then(serde_json::Value::as_u64)
                == Some(1);
        let method = match kind {
            CodeLensKind::Reference => "textDocument/references",
            CodeLensKind::Implementation => "textDocument/implementation",
        };
        let mapper = TsgoResponseMapper::for_overlays(&runtime.overlays);
        let document = mapper.document_context(&source_uri);
        let mut params = serde_json::json!({
            "textDocument": { "uri": source_uri },
            "position": lens.pointer("/range/start").cloned().unwrap_or_default(),
        });
        if kind == CodeLensKind::Reference {
            params["context"] = serde_json::json!({ "includeDeclaration": false });
        }
        if component_lens {
            let Some(shadow) = runtime.owner_shadow(&source_path) else {
                self.respond(Response::new_ok(id, lens));
                return;
            };
            let Some(position) = component_probe_position(&shadow.text) else {
                self.respond(Response::new_ok(id, lens));
                return;
            };
            params["textDocument"]["uri"] =
                serde_json::Value::String(shadow.shadow_uri.as_str().to_string());
            params["position"] = serde_json::to_value(position).unwrap_or_default();
        } else {
            let mapper = TsgoResponseMapper::for_overlays_with_default_document(
                &runtime.overlays,
                document.clone(),
            );
            if !mapper.map_request(method, &mut params) {
                self.respond(Response::new_ok(id, lens));
                return;
            }
        }
        let child = Request::new(id.clone(), method.to_string(), params);
        let pending = PendingTsgoRequest {
            method: method.to_string(),
            document,
            fallback_result: None,
            completion_site: None,
            rename: None,
            code_action_diagnostic_codes: Vec::new(),
            push_diagnostics: None,
            component_site: None,
            component_references: component_lens.then(|| source_uri.clone()),
            file_rename: None,
            code_lens_resolve: Some(PendingCodeLensResolve {
                lens,
                kind,
                source_uri,
            }),
            completion_site_data: None,
            include_declaration: None,
            component_imports: Vec::new(),
        };
        if runtime.forward(child.into()).is_ok() {
            self.pending_tsgo.insert(id, pending);
        } else if let Some(resolve) = pending.code_lens_resolve {
            self.respond(Response::new_ok(id, resolve.lens));
        }
    }

    fn on_execute_command(&mut self, request: Request) {
        let id = request.id;
        let Ok(params) = serde_json::from_value::<ExecuteCommandParams>(request.params) else {
            return self.respond_nothing(id);
        };
        if params.command != crate::extract::COMMAND {
            return self.respond_nothing(id);
        }
        if !self.settings.svelte.enable || !self.settings.svelte.code_actions {
            return self.respond_nothing(id);
        }
        let Some(args) = params.arguments.into_iter().nth(1) else {
            return self.respond_nothing(id);
        };
        let Some(uri) = args
            .get("uri")
            .and_then(serde_json::Value::as_str)
            .and_then(|uri| uri.parse::<Uri>().ok())
        else {
            return self.respond_nothing(id);
        };
        let Some(document) = self.component_document(&uri) else {
            return self.respond_nothing(id);
        };
        let text = document.shared_text();
        let Some(range) = args
            .get("range")
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok())
        else {
            return self.respond_nothing(id);
        };
        let file_path = args
            .get("filePath")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        self.pending.insert(id.clone(), Pending::ExtractComponent);
        self.worker.submit(Job::ExtractComponent {
            id,
            uri,
            text,
            range,
            file_path,
        });
    }

    fn on_folding_range(&mut self, request: Request) {
        let tsgo_fallback = request.clone();
        let id = request.id;
        let params = match serde_json::from_value::<FoldingRangeParams>(request.params) {
            Ok(params) => params,
            Err(err) => {
                log::warn(format_args!("textDocument/foldingRange: {err}"));
                self.respond_no_ranges(id);
                return;
            }
        };
        if !self.settings.folding_range_enable {
            self.respond_no_ranges(id);
            return;
        }
        match self.component(&params.text_document.uri) {
            Some((path, text)) => {
                self.worker.submit(Job::FoldingRange {
                    id: id.clone(),
                    path,
                    text,
                    line_folding_only: self.client.line_folding_only,
                });
                self.join(id, tsgo_fallback);
            }
            None => self.forward_tsgo_request(tsgo_fallback),
        }
    }

    fn on_selection_range(&mut self, request: Request) {
        let tsgo_fallback = request.clone();
        let id = request.id;
        let params = match serde_json::from_value::<SelectionRangeParams>(request.params) {
            Ok(params) => params,
            Err(err) => {
                log::warn(format_args!("textDocument/selectionRange: {err}"));
                self.respond_nothing(id);
                return;
            }
        };
        if !self.settings.selection_range_enable {
            self.respond_nothing(id);
            return;
        }
        if !(self.settings.svelte.enable && self.settings.svelte.selection_range
            || self.settings.css.enable && self.settings.css.selection_range)
        {
            self.forward_tsgo_request(tsgo_fallback);
            return;
        }
        let Some(document) = self.component_document(&params.text_document.uri) else {
            self.respond_nothing(id);
            return;
        };
        let offsets = params
            .positions
            .iter()
            .map(|&position| document.offset_at(position))
            .collect();
        let job = Job::SelectionRange {
            id: id.clone(),
            path: uri_to_path(params.text_document.uri.as_str()),
            text: document.shared_text(),
            offsets,
        };
        self.pending.insert(id, Pending::SelectionRange);
        self.worker.submit(job);
    }

    fn on_document_symbol(&mut self, request: Request) {
        let tsgo_fallback = request.clone();
        let id = request.id;
        let params = match serde_json::from_value::<DocumentSymbolParams>(request.params) {
            Ok(params) => params,
            Err(err) => {
                log::warn(format_args!("textDocument/documentSymbol: {err}"));
                self.respond_no_symbols(id);
                return;
            }
        };
        if !self.settings.document_symbol_enable {
            self.respond_no_symbols(id);
            return;
        }
        if !(self.settings.html.enable && self.settings.html.document_symbols
            || self.settings.css.enable && self.settings.css.document_symbols)
        {
            self.forward_tsgo_request(tsgo_fallback);
            return;
        }
        let uri = params.text_document.uri;
        match self.component(&uri) {
            Some((path, text)) => {
                self.worker.submit(Job::DocumentSymbol {
                    id: id.clone(),
                    uri,
                    path,
                    text,
                    hierarchical: self.client.hierarchical_document_symbols,
                });
                self.join(id, tsgo_fallback);
            }
            None => self.forward_tsgo_request(tsgo_fallback),
        }
    }

    /// An open component, as the worker needs it. Only components have Svelte
    /// template structure to report.
    fn component(&self, uri: &Uri) -> Option<(std::path::PathBuf, std::sync::Arc<String>)> {
        let document = self.component_document(uri)?;
        Some((uri_to_path(uri.as_str()), document.shared_text()))
    }

    /// Whether the editor has `path` (canonical) open, so its shadow carries
    /// buffer text that must stay out of the body on disk.
    fn is_editor_document(&self, path: &Path) -> bool {
        self.documents.iter().any(|document| {
            let open = uri_to_path(document.uri.as_str());
            crate::uri::canonicalize(&open).unwrap_or(open) == path
        })
    }

    fn component_document(&self, uri: &Uri) -> Option<&Document> {
        let document = self.documents.get(uri)?;
        (document.language_id == "svelte").then_some(document)
    }

    fn on_notification(&mut self, notification: Notification) {
        let method = notification.method.clone();
        // Requests that arrived before this change are answered against the
        // text they were sent for: a probe that holds some ends, and they are
        // replayed, and every native answer sends its tsgo half, before it
        // applies.
        if matches!(
            method.as_str(),
            "textDocument/didChange" | "textDocument/didClose"
        ) {
            self.end_probes();
            self.finish_native_halves();
        }
        match method.as_str() {
            "$/cancelRequest" => {
                match serde_json::from_value::<CancelParams>(notification.params) {
                    Ok(params) => self.cancel_request(params.id),
                    Err(err) => log::warn(format_args!("{method}: {err}")),
                }
            }
            "workspace/didChangeWorkspaceFolders" => {
                let child_notification = notification.clone();
                match serde_json::from_value::<DidChangeWorkspaceFoldersParams>(notification.params)
                {
                    Ok(params) => {
                        self.update_tsgo_workspace_folders(
                            &params.event.added,
                            &params.event.removed,
                        );
                        self.client
                            .update_workspace_folders(params.event.added, &params.event.removed);
                        self.ensure_tsgo_runtime();
                        self.restart_preprocessing();
                        if let Some(runtime) = &self.tsgo {
                            let workspace_folders = self.client.workspace_folders.clone();
                            let root_uri = workspace_folders
                                .is_empty()
                                .then(|| self.client.root_uri.clone())
                                .flatten();
                            let current_dir = self
                                .client
                                .workspace_folders
                                .first()
                                .map(|folder| uri_to_path(folder.uri.as_str()))
                                .or_else(|| {
                                    self.client
                                        .root_uri
                                        .as_ref()
                                        .map(|root| uri_to_path(root.as_str()))
                                })
                                .filter(|path| path.is_dir());
                            let _ = runtime.client.update_workspace(
                                root_uri,
                                workspace_folders,
                                current_dir,
                            );
                            let _ = runtime.forward(child_notification.into());
                        }
                    }
                    Err(err) => log::warn(format_args!("{method}: {err}")),
                }
            }
            "workspace/didChangeWatchedFiles" => {
                let child_notification = notification.clone();
                match serde_json::from_value::<DidChangeWatchedFilesParams>(notification.params) {
                    Ok(params) => {
                        // The overlay cache holds this server's own output — the
                        // shadow bodies, the overlay configs — so an event there
                        // is an echo of a write, never a change to the project.
                        // tsgo still hears it below.
                        let changes = params
                            .changes
                            .iter()
                            .filter(|change| {
                                !tsgo_overlay::is_cache_path(&uri_to_path(change.uri.as_str()))
                            })
                            .collect::<Vec<_>>();
                        let project_changed =
                            changes.iter().any(|change| is_project_config(&change.uri));
                        let dependency_changed = changes.iter().any(|change| {
                            let path = uri_to_path(change.uri.as_str());
                            let path = crate::uri::canonicalize(&path).unwrap_or(path);
                            self.preprocess_dependencies
                                .values()
                                .any(|dependencies| dependencies.contains(&path))
                        });
                        let package_changed = changes.iter().any(|change| {
                            let path = uri_to_path(change.uri.as_str());
                            path.file_name().is_some_and(|name| name == "package.json")
                                && !path
                                    .components()
                                    .any(|component| component.as_os_str() == "node_modules")
                        });
                        if package_changed && !project_changed && !dependency_changed {
                            self.refresh_package_imports();
                        }
                        if project_changed || dependency_changed {
                            self.invalidate_project_config();
                            self.rebuild_tsgo_overlays();
                            self.restart_preprocessing();
                        } else if !changes.is_empty() {
                            let changes = changes
                                .iter()
                                .map(|change| (uri_to_path(change.uri.as_str()), change.typ))
                                .collect::<Vec<_>>();
                            if self.background.in_flight.is_some() {
                                self.background
                                    .missed_events
                                    .extend(changes.iter().cloned());
                            }
                            self.refresh_tsgo_overlays(&changes);
                        }
                        if let Some(runtime) = &self.tsgo {
                            let _ = runtime.forward(child_notification.into());
                        }
                    }
                    Err(err) => log::warn(format_args!("{method}: {err}")),
                }
            }
            "textDocument/didOpen" => {
                match serde_json::from_value::<DidOpenTextDocumentParams>(notification.params) {
                    Ok(params) => {
                        let doc = params.text_document;
                        let key = doc.uri.as_str().to_string();
                        self.documents
                            .open(doc.uri, doc.language_id, doc.version, doc.text);
                        self.sync_tsgo_document(&key);
                        let preprocessing = self.queue_preprocess_document(&key);
                        if !self.client.pull_diagnostics && !preprocessing {
                            self.schedule_lint(key, Duration::ZERO);
                        }
                    }
                    Err(err) => log::warn(format_args!("{method}: {err}")),
                }
            }
            "textDocument/didChange" => {
                match serde_json::from_value::<DidChangeTextDocumentParams>(notification.params) {
                    Ok(params) => {
                        let key = params.text_document.uri.as_str().to_string();
                        let mut changed = false;
                        if let Some(document) = self.documents.get_mut(&params.text_document.uri) {
                            document.apply(params.text_document.version, &params.content_changes);
                            changed = true;
                        }
                        self.sync_tsgo_document(&key);
                        let preprocessing = self.queue_preprocess_document(&key);
                        if changed && !self.client.pull_diagnostics && !preprocessing {
                            self.schedule_lint(key, LINT_DEBOUNCE);
                        }
                    }
                    Err(err) => log::warn(format_args!("{method}: {err}")),
                }
            }
            "textDocument/didSave" => {
                match serde_json::from_value::<DidSaveTextDocumentParams>(notification.params) {
                    Ok(params) => {
                        if !self.client.pull_diagnostics {
                            self.schedule_lint(
                                params.text_document.uri.as_str().to_string(),
                                Duration::ZERO,
                            );
                        }
                    }
                    Err(err) => log::warn(format_args!("{method}: {err}")),
                }
            }
            "textDocument/didClose" => {
                match serde_json::from_value::<DidCloseTextDocumentParams>(notification.params) {
                    Ok(params) => {
                        let uri = params.text_document.uri;
                        self.close_tsgo_document(&uri);
                        self.scheduled.remove(uri.as_str());
                        self.linted.remove(uri.as_str());
                        let version = self.documents.close(&uri).map_or(0, |d| d.version);
                        if !self.client.pull_diagnostics {
                            self.publish(uri, version, Vec::new());
                        }
                    }
                    Err(err) => log::warn(format_args!("{method}: {err}")),
                }
            }
            "workspace/didChangeConfiguration" => {
                // A `rsvelte-lint.json` / `.oxfmtrc` edit reaches the server as
                // a configuration change too, so the resolved-config caches are
                // dropped along with the client settings.
                self.invalidate_project_config();
                if self.client.pull_configuration {
                    self.request_configuration();
                } else {
                    let preprocessing_was_enabled = self.settings.preprocess_enable;
                    let settings = notification.params.get("settings");
                    self.settings = Settings::from_sections(
                        settings
                            .and_then(|settings| settings.get("rsvelte"))
                            .unwrap_or(&serde_json::Value::Null),
                        settings
                            .and_then(|settings| settings.get("svelte"))
                            .unwrap_or(&serde_json::Value::Null),
                    )
                    .with_emmet(
                        settings
                            .and_then(|settings| settings.get("emmet"))
                            .unwrap_or(&serde_json::Value::Null),
                    );
                    if preprocessing_was_enabled && !self.settings.preprocess_enable {
                        self.preprocess = None;
                        self.preprocess_failures.clear();
                        self.preprocess_documents.clear();
                        self.preprocess_pending.clear();
                        self.preprocess_dependencies.clear();
                        self.rebuild_tsgo_overlays();
                    } else {
                        self.ensure_preprocess_runtime();
                    }
                    if !self.client.pull_diagnostics {
                        self.relint_open_documents();
                    }
                }
            }
            "exit" => self.exiting = true,
            _ => {}
        }
    }

    fn cancel_request(&mut self, id: NumberOrString) {
        let id = match id {
            NumberOrString::Number(id) => RequestId::from(id),
            NumberOrString::String(id) => RequestId::from(id),
        };
        let native = self.pending.remove(&id).is_some();
        let joined = self.joins.remove(&id).is_some();
        let tsgo = self.pending_tsgo.remove(&id).is_some();
        let rename = self.rename_aggregates.remove(&id).is_some();
        let component = self.component_queries.remove(&id);
        self.cancel_component_requests(&id);
        if let Some(component) = &component {
            self.run_component_action(component.query.restore());
        }
        let child_rename_ids = self
            .pending_tsgo
            .iter()
            .filter_map(|(child_id, pending)| match &pending.rename {
                Some(PendingRename::Followup { editor_id }) if editor_id == &id => {
                    Some(child_id.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        for child_id in child_rename_ids {
            self.pending_tsgo.remove(&child_id);
            if let Some(runtime) = &self.tsgo {
                runtime.cancel(&child_id);
            }
        }
        if tsgo && let Some(runtime) = &self.tsgo {
            runtime.cancel(&id);
        }
        let deferred = self.deferred.len();
        self.deferred.retain(|request| match request {
            Deferred::Request(request) | Deferred::Forward(request, _) => request.id != id,
            Deferred::PushDiagnostics(..) => true,
        });
        if native
            || joined
            || tsgo
            || rename
            || component.is_some()
            || self.deferred.len() != deferred
        {
            self.respond(Response::new_err(
                id,
                ErrorCode::RequestCanceled as i32,
                "request cancelled by client".to_string(),
            ));
        }
        if component.is_some() {
            self.replay_deferred();
        }
    }

    fn forward_tsgo_request(&mut self, request: Request) {
        self.forward_tsgo_request_with_fallback(request, None);
    }

    fn forward_tsgo_request_with_fallback(
        &mut self,
        mut request: Request,
        fallback_result: Option<serde_json::Value>,
    ) {
        if self.probe_in_flight() {
            self.deferred
                .push(Deferred::Forward(request, fallback_result));
            return;
        }
        if !self.settings.tsgo_method_enabled(&request.method) {
            self.respond(Response::new_ok(
                request.id,
                fallback_result.unwrap_or_else(|| tsgo_unmapped_result(&request.method)),
            ));
            return;
        }
        if is_workspace_wide(&request.method) {
            self.load_all_projects();
        }
        let Ok(completion_site_data) = self.completion_data_site(&mut request) else {
            // Without tsgo's payload the child rejects the request outright; the
            // unresolved item is still a valid response to the editor.
            self.respond(Response::new_ok(request.id, request.params));
            return;
        };
        let completion_site = self.completion_site(&request);
        if completion_site == Some(CompletionSite::BlockMarker)
            && fallback_result.is_none()
            && self.is_opening_block_completion(&request)
        {
            // An unfinished unknown `{#...` now makes the compiler reject the
            // projection. Preserve the pre-diagnostic completion response when
            // the native provider also has nothing to offer.
            self.respond(Response::new_ok(request.id, empty_completion_list()));
            return;
        }
        let component_site = self.component_completion_site(&request);
        let code_action_diagnostic_codes = code_action_diagnostic_codes(&request);
        // A component the editor has not opened belongs to a project that may
        // not be loaded yet; load it so the request can be mapped.
        if let Some(runtime) = &self.tsgo
            && let Some(uri) = text_document_request_uri(&request)
            && uri.as_str().ends_with(".svelte")
        {
            let path = uri_to_path(uri.as_str());
            if runtime
                .overlays
                .iter()
                .all(|overlay| overlay.shadow_for_source(&path).is_none())
                && path.is_file()
            {
                self.ensure_tsgo_project(&path);
            }
        }
        let Some(runtime) = &self.tsgo else {
            self.respond(Response::new_ok(
                request.id,
                fallback_result.unwrap_or_else(|| tsgo_unmapped_result(&request.method)),
            ));
            return;
        };
        let initial = TsgoResponseMapper::for_overlays_request(&runtime.overlays, &request.params);
        let document = initial
            .default_document()
            .cloned()
            .or_else(|| runtime.completion_document_context(&request.params));
        let mapper = TsgoResponseMapper::for_overlays_with_default_document(
            &runtime.overlays,
            document.clone(),
        );
        let component_imports = if request.method == "textDocument/codeAction"
            && let Some(document) = &document
            && let Some((overlay, shadow_path)) = shadow_overlay(&runtime.overlays, document)
            && let Some(source) = self.documents.get(document.source_uri())
        {
            component_import_sites(
                &request.params,
                source.text(),
                overlay,
                &uri_to_path(document.source_uri().as_str()),
                &shadow_path,
            )
        } else {
            Vec::new()
        };
        if !mapper.map_request(&request.method, &mut request.params) {
            self.respond(Response::new_ok(
                request.id,
                fallback_result.unwrap_or_else(|| tsgo_unmapped_result(&request.method)),
            ));
            return;
        }
        let pending = PendingTsgoRequest {
            method: request.method.clone(),
            document,
            fallback_result,
            completion_site,
            rename: None,
            code_action_diagnostic_codes,
            push_diagnostics: None,
            component_site,
            component_references: None,
            file_rename: None,
            code_lens_resolve: None,
            completion_site_data,
            include_declaration: (request.method == "textDocument/references").then(|| {
                request
                    .params
                    .pointer("/context/includeDeclaration")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(true)
            }),
            component_imports,
        };
        let id = request.id.clone();
        // tsrs never answers a request it loops on, and each one holds one of
        // its few workers for good: with three or so looping, it answers
        // nothing else until it restarts (review 5, loopstorm.mjs). One per
        // document is sent, and restarts tsgo once overdue; the editor's
        // others meanwhile (it asks again as the cursor moves) are answered
        // at once without tsgo, as the first will be. Likewise after an
        // overdue completion or signature help the model clears, which may
        // be a loop it misses (review 6).
        if let Some(looping) = runtime.looping_on_document(&request.method, &request.params) {
            log::warn(format_args!(
                "not sending {} ({id}) to tsgo, which loops on it, or may, and has not answered {looping}",
                request.method
            ));
            self.respond(Response::new_ok(
                id,
                pending
                    .fallback_result
                    .unwrap_or_else(|| tsgo_unmapped_result(&pending.method)),
            ));
            return;
        }
        if let Err(error) = runtime.forward(request.into()) {
            log::warn(format_args!("could not forward request to tsgo: {error}"));
            self.respond(Response::new_ok(
                id,
                pending
                    .fallback_result
                    .unwrap_or_else(|| tsgo_unmapped_result(&pending.method)),
            ));
            return;
        }
        self.pending_tsgo.insert(id, pending);
    }

    fn pull_and_publish_tsgo_diagnostics(
        &mut self,
        uri: Uri,
        version: i32,
        native: Vec<lsp_types::Diagnostic>,
    ) {
        if self.probe_in_flight() {
            self.deferred
                .push(Deferred::PushDiagnostics(uri, version, native));
            return;
        }
        if !self.settings.tsgo_method_enabled("textDocument/diagnostic") {
            self.publish(uri, version, native);
            return;
        }
        let native_fallback = native.clone();
        let fallback = serde_json::to_value(diagnostic_report(native)).ok();
        let Some(runtime) = &self.tsgo else {
            self.publish(uri, version, native_fallback);
            return;
        };
        let mut params = serde_json::json!({ "textDocument": { "uri": uri } });
        let mapper = TsgoResponseMapper::for_overlays_request(&runtime.overlays, &params);
        let document = mapper.default_document().cloned();
        if !mapper.map_request("textDocument/diagnostic", &mut params) {
            self.publish(uri, version, native_fallback);
            return;
        }
        self.next_request_id += 1;
        let id = RequestId::from(format!(
            "rsvelte-tsgo-push-diagnostics-{}",
            self.next_request_id
        ));
        let request = Request::new(id.clone(), "textDocument/diagnostic".to_string(), params);
        let pending = PendingTsgoRequest {
            method: "textDocument/diagnostic".to_string(),
            document,
            fallback_result: fallback,
            completion_site: None,
            rename: None,
            code_action_diagnostic_codes: Vec::new(),
            push_diagnostics: Some((uri.clone(), version)),
            component_site: None,
            component_references: None,
            file_rename: None,
            code_lens_resolve: None,
            completion_site_data: None,
            include_declaration: None,
            component_imports: Vec::new(),
        };
        if runtime.forward(request.into()).is_ok() {
            self.pending_tsgo.insert(id, pending);
        } else {
            self.publish(uri, version, native_fallback);
        }
    }

    /// The source site a completion request is for. On a resolve, the item the
    /// editor sends back carries upstream's payload, so tsgo's own `data` has
    /// to go back on it first — everything downstream reads `data.fileName`.
    /// `Err` means the payload is unrecoverable and the child must not be asked.
    fn completion_data_site(
        &mut self,
        request: &mut Request,
    ) -> Result<Option<(String, serde_json::Value)>, ()> {
        match request.method.as_str() {
            "textDocument/completion" => Ok(request
                .params
                .pointer("/textDocument/uri")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
                .zip(request.params.get("position").cloned())),
            "completionItem/resolve" => {
                let Some(site) = upstream_completion_data_site(&request.params) else {
                    return Ok(None);
                };
                let key = completion_data_key(&site.0, &site.1);
                if self
                    .completion_data
                    .get(&key)
                    .is_some_and(|stash| restore_tsgo_completion_data(&mut request.params, stash))
                {
                    Ok(Some(site))
                } else {
                    Err(())
                }
            }
            _ => Ok(None),
        }
    }

    fn remember_completion_data(&mut self, key: String, stash: HashMap<String, serde_json::Value>) {
        // Keep the most recent sites rather than dropping the current one: a
        // resolve arrives right after the completion it belongs to.
        while self.completion_order.len() >= 8 {
            let oldest = self.completion_order.remove(0);
            self.completion_data.remove(&oldest);
        }
        self.completion_order.retain(|entry| entry != &key);
        self.completion_order.push(key.clone());
        self.completion_data.insert(key, stash);
    }

    fn completion_site(&self, request: &Request) -> Option<CompletionSite> {
        if request.method != "textDocument/completion" {
            return None;
        }
        let uri = request
            .params
            .pointer("/textDocument/uri")?
            .as_str()?
            .parse::<Uri>()
            .ok()?;
        let position = serde_json::from_value(request.params.get("position")?.clone()).ok()?;
        let document = self.documents.get(&uri)?;
        if document.language_id != "svelte" {
            return Some(CompletionSite::Script);
        }
        let text = document.text();
        let offset = document.offset_at(position);
        if crate::context::EmbeddedRegions::new(text).contains(offset) {
            return Some(if inside_element_body(text, offset, "style") {
                CompletionSite::Style
            } else {
                CompletionSite::Script
            });
        }
        if let Some(site) = start_tag_completion_site(text, offset) {
            return Some(site);
        }
        let before = text.get(..offset)?;
        // `CompletionProvider.ts:204-214` tests the WORD at the cursor, which
        // `getWordAtPosition` grows left while the character is neither
        // whitespace nor `.` — so `{#i` is a block marker and `{#if cond` is
        // not, because its word is `cond`.
        let word_start = before
            .char_indices()
            .rev()
            .find(|(_, character)| character.is_whitespace() || *character == '.')
            .map_or(0, |(index, character)| index + character.len_utf8());
        let word = before.get(word_start..)?;
        if let Some(rest) = word.strip_prefix('{')
            && rest.starts_with(['#', '@', ':', '/'])
        {
            return Some(CompletionSite::BlockMarker);
        }
        let brace = before.rfind('{');
        let close = before.rfind('}');
        if brace > close {
            return Some(CompletionSite::TemplateExpression);
        }
        Some(CompletionSite::RawTemplateText)
    }

    fn is_opening_block_completion(&self, request: &Request) -> bool {
        let uri = request
            .params
            .pointer("/textDocument/uri")
            .and_then(serde_json::Value::as_str)
            .and_then(|uri| uri.parse::<Uri>().ok());
        let position = request
            .params
            .get("position")
            .cloned()
            .and_then(|position| serde_json::from_value(position).ok());
        let (Some(uri), Some(position)) = (uri, position) else {
            return false;
        };
        let Some(document) = self.documents.get(&uri) else {
            return false;
        };
        let offset = document.offset_at(position);
        let Some(before) = document.text().get(..offset) else {
            return false;
        };
        before
            .rfind('{')
            .and_then(|brace| before.get(brace + 1..))
            .is_some_and(|marker| marker.trim_start().starts_with('#'))
    }

    fn component_completion_site(&self, request: &Request) -> Option<ComponentCompletionSite> {
        if request.method != "textDocument/completion" {
            return None;
        }
        let params = serde_json::from_value::<CompletionParams>(request.params.clone()).ok()?;
        let document = self
            .documents
            .get(&params.text_document_position.text_document.uri)?;
        if document.language_id != "svelte" {
            return None;
        }
        component_completion_site(
            document.text(),
            params.text_document_position.position,
            params.context.as_ref(),
            document_has_parser_error(document.text()),
        )
        .ok()
    }

    fn invalidate_project_config(&mut self) {
        self.preprocess_configs.clear();
        self.preprocess_known_empty = false;
        self.worker.submit(Job::ClearCaches);
        if !self.client.pull_diagnostics {
            self.relint_open_documents();
        }
    }

    fn sync_tsgo_document(&mut self, key: &str) {
        let Some(document) = self.documents.get_by_key(key) else {
            return;
        };
        let uri = document.uri.clone();
        let language_id = document.language_id.clone();
        let version = document.version;
        let text = document.text().to_string();
        let path = uri_to_path(uri.as_str());
        let path = crate::uri::canonicalize(&path).unwrap_or(path);
        let svelte = is_svelte_document(&language_id, &path);
        let script = !svelte && is_typescript_or_javascript(&language_id, &path);
        if !svelte && !script {
            return;
        }
        if let Some(shadow_uri) = self.probed_shadow_of(&path) {
            self.end_probe_in(&shadow_uri);
        }
        let Some(owner) = self.ensure_tsgo_project(&path) else {
            return;
        };
        let Some(runtime) = &mut self.tsgo else {
            return;
        };
        let updated = if svelte {
            // The owner first, then every copy another loaded project keeps for
            // its imports, so no program sees a stale component: upstream's
            // snapshots are shared by every service.
            let updated = std::iter::once(owner)
                .chain(runtime.copies_of(owner, &path))
                .collect::<Vec<_>>();
            for &index in &updated {
                match runtime.overlays[index].open_or_update(&path, &text, version) {
                    Ok(shadow) => {
                        open_shadow(&runtime.client, &mut runtime.open_shadows, &shadow);
                    }
                    Err(error) => log::warn(format_args!(
                        "could not update tsgo shadow for {}: {error}",
                        path.display()
                    )),
                }
            }
            updated
        } else {
            match runtime.overlays[owner].open_plain(&path, &text, version, &language_id) {
                Ok(shadow) => {
                    open_shadow(&runtime.client, &mut runtime.open_shadows, &shadow);
                }
                Err(error) => log::warn(format_args!(
                    "could not route TypeScript buffer {}: {error}",
                    path.display()
                )),
            }
            vec![owner]
        };
        // The JavaScript components the document reaches, when its imports
        // changed (and when it opens).
        let fingerprint = tsgo_overlay::svelte_import_fingerprint(&text);
        let reach_changed =
            runtime.document_imports.insert(path.clone(), fingerprint) != Some(fingerprint);
        // A new import in the buffer gets its copy now, not when it is saved.
        let documents = &self.documents;
        let is_open =
            |target: &Path| path_to_uri(target).is_some_and(|uri| documents.get(&uri).is_some());
        let following = updated
            .iter()
            .copied()
            .filter(|&index| runtime.overlays[index].has_new_imports(&path, &text, is_open))
            .collect::<Vec<_>>();
        if !following.is_empty() {
            let editor = editor_buffers(documents);
            let mut events = Vec::new();
            for index in following {
                match runtime.overlays[index].follow_buffer_imports(&path, &text, &editor) {
                    Ok(update) => runtime.deliver_update(index, &update, &mut events),
                    Err(error) => log::warn(format_args!(
                        "could not follow the imports of {}: {error}",
                        path.display()
                    )),
                }
            }
            let _ = runtime.client.notify_watched_files(events);
            self.preprocess_known_empty = false;
        }
        if reach_changed && let Some(runtime) = &mut self.tsgo {
            for index in updated {
                runtime.open_reachable_javascript(index, [path.clone()]);
            }
        }
    }

    /// Make sure the project that owns `path` is loaded and set up for the
    /// editor — the one way every caller reaches a project. A project that
    /// loads now reads its copy of every component from disk, so the open
    /// editor documents it holds are mirrored into it at once: an unsaved
    /// buffer must not lose to its file in whichever project loaded last
    /// (upstream's snapshots are shared by every service). The first time a
    /// project serves the editor, its JavaScript components open as buffers
    /// and its members are queued for preprocessing.
    fn ensure_tsgo_project(&mut self, path: &Path) -> Option<usize> {
        // The background may be loading this very project into the same
        // cache: take its result rather than race it.
        if let Some(spec) = &self.background.in_flight
            && self
                .tsgo
                .as_ref()
                .and_then(|runtime| runtime.project_spec_for(path))
                .is_some_and(|wanted| wanted.same_project(spec))
        {
            self.wait_for_background();
        }
        let runtime = self.tsgo.as_mut()?;
        let (owner, loaded) = runtime.ensure_project_for(path);
        let owner = owner?;
        if loaded {
            self.mirror_editor_buffers(owner);
            if let Some(runtime) = &mut self.tsgo {
                runtime.keep_project_alive(owner);
            }
        }
        let roots = self
            .documents
            .iter()
            .map(|document| {
                let path = uri_to_path(document.uri.as_str());
                crate::uri::canonicalize(&path).unwrap_or(path)
            })
            .collect::<Vec<_>>();
        let activated = self
            .tsgo
            .as_mut()
            .is_some_and(|runtime| runtime.activate_project(owner, &roots));
        if activated {
            self.preprocess_loaded_project(owner);
        }
        Some(owner)
    }

    /// When the next background step is due: the editor's first project has
    /// been served, no request is waiting on tsgo, nothing is loading, and
    /// the editor has been quiet for [`background::IDLE`].
    fn background_deadline(&self) -> Option<Instant> {
        let background = &self.background;
        if !self.settings.background_projects_enable
            || background.complete
            || background.worker.is_none()
            || background.enumerating
            || background.in_flight.is_some()
            || background.warming.is_some()
            || !self.component_queries.is_empty()
        {
            return None;
        }
        let runtime = self.tsgo.as_ref()?;
        // tsgo is idle once it answered every editor request, or once those
        // it has not answered are old enough to be lost.
        let tsgo_idle = self
            .pending_tsgo
            .keys()
            .map(|id| {
                runtime
                    .sent_at(id)
                    .map_or(Instant::now(), |sent| sent + TSGO_BUSY_FOR)
            })
            .max();
        runtime
            .overlays
            .iter()
            .any(TsgoOverlay::is_loaded)
            .then(|| {
                let quiet = background.last_activity + background::IDLE;
                tsgo_idle.map_or(quiet, |idle| quiet.max(idle))
            })
    }

    /// When the next request tsgo has not answered is due to expire, or
    /// tsgo to restart ([`Self::tsgo_restart_candidates`]).
    fn tsgo_request_deadline(&self) -> Option<Instant> {
        let runtime = self.tsgo.as_ref()?;
        let expiry = runtime
            .sent
            .borrow()
            .values()
            .filter(|request| !request.overdue && !waits_for_tsgo(&request.method))
            .map(|request| request.at + self.tsgo_answer_timeout)
            .min();
        expiry
            .into_iter()
            .chain(
                self.tsgo_restart_candidates()
                    .iter()
                    .map(|restart| restart.at),
            )
            .min()
    }

    /// When an overdue request restarts tsgo on its own: one tsgo loops on
    /// ([`SentRequest::loops`]) after the timeout, a completion or signature
    /// help it may loop on ([`SentRequest::suspect`]) after
    /// [`SUSPECT_RESTART_TIMEOUTS`]; any other is slow, and answered whenever
    /// tsgo is done, unless tsgo stops answering altogether
    /// ([`Self::tsgo_restart_candidates`]). Each restart for the same method
    /// and document doubles the next one's wait.
    ///
    /// tsrs answers a request about a document whose check is running only
    /// once the check is done (review 5), so while a pull of the document is
    /// with tsgo, an overdue request may be waiting for it: it restarts tsgo
    /// only once the pull is answered, which keeps the pull's answer.
    fn restart_deadline(&self, request: &SentRequest) -> Option<Instant> {
        if !request.overdue {
            return None;
        }
        let key = (request.method.clone(), request.document.clone());
        let doublings = self
            .tsgo_restarts
            .doublings(&key, request.at, self.tsgo_answer_timeout);
        if request.suspect() {
            return self.waiting_for_check_until(request).map(|until| {
                (request.at
                    + self.tsgo_answer_timeout * SUSPECT_RESTART_TIMEOUTS * 2_u32.pow(doublings))
                .max(until)
            });
        }
        if !request.loops {
            return None;
        }
        let checking = self.tsgo.as_ref().is_some_and(|runtime| {
            runtime
                .sent
                .borrow()
                .values()
                .any(|sent| waits_for_tsgo(&sent.method) && sent.document == request.document)
        });
        if checking {
            return None;
        }
        Some(request.at + self.tsgo_answer_timeout * 2_u32.pow(doublings))
    }

    /// Until when a request may be waiting for a check of its document rather
    /// than lost: tsrs answers a request about a document whose check runs
    /// once the check is done (review 5). While a pull of the document is with
    /// tsgo, until it is answered (`None`: a check can take minutes, and
    /// longer still on a busy machine, review 6's `slowmix.mjs` at a load of
    /// 40); then for a timeout after a pull that ran past the request's own
    /// timeout, the time tsrs takes to answer what the check held back.
    fn waiting_for_check_until(&self, request: &SentRequest) -> Option<Instant> {
        let timeout = self.tsgo_answer_timeout;
        let Some(runtime) = &self.tsgo else {
            return Some(request.at);
        };
        let Some(document) = request.document.as_deref() else {
            return Some(request.at);
        };
        if runtime
            .sent
            .borrow()
            .values()
            .any(|sent| waits_for_tsgo(&sent.method) && sent.document.as_deref() == Some(document))
        {
            return None;
        }
        let checked = runtime
            .pulls_answered
            .borrow()
            .get(document)
            .filter(|(sent, answered)| {
                *sent < request.at + timeout && request.at + timeout <= *answered
            })
            .map(|(_, answered)| *answered + timeout);
        Some(checked.map_or(request.at, |until| until.max(request.at)))
    }

    /// Every reason to restart tsgo there is or will be, given the requests
    /// it has not answered, each with the time it falls due
    /// ([`Self::expire_tsgo_requests`] restarts tsgo for those due):
    ///
    /// * an overdue request tsgo loops on, or may ([`Self::restart_deadline`]);
    /// * a request tsgo has answered nothing since it was sent, for a timeout:
    ///   a slow tsrs still answers the editor's other requests meanwhile (the
    ///   hovers while `slowsig.mjs`'s check runs), one that loops on every
    ///   request it can run answers none (review 6: four completions the
    ///   model missed, after which every hover went unanswered);
    /// * [`TSRS_QUERY_CHECKERS`] requests overdue at once, as many as tsrs
    ///   runs about a project.
    ///
    /// The last two need no model of what tsrs loops on, so a loop the model
    /// misses costs at most a timeout once it holds tsrs back; they skip the
    /// background's own requests (a large project may take that long to
    /// build while the editor is quiet) and diagnostic pulls (a check may),
    /// and wait for a check the request may be waiting for
    /// ([`Self::waiting_for_check_until`]). Their restarts back off together.
    fn tsgo_restart_candidates(&self) -> Vec<TsgoRestart> {
        let Some(runtime) = &self.tsgo else {
            return Vec::new();
        };
        let timeout = self.tsgo_answer_timeout;
        let sent = runtime.sent.borrow();
        let mut restarts = Vec::new();
        for (id, request) in sent.iter() {
            if let Some(at) = self.restart_deadline(request) {
                restarts.push(TsgoRestart {
                    at,
                    why: format!(
                        "{} ({id}){}",
                        request.method,
                        if request.loops {
                            ""
                        } else {
                            ", which it may loop on"
                        }
                    ),
                    backoff: (
                        (request.method.clone(), request.document.clone()),
                        request.at,
                    ),
                });
            }
        }
        // Each request the liveness rules count, with the time it falls due.
        let mut due = sent
            .iter()
            .filter(|(id, request)| !waits_for_tsgo(&request.method) && !is_warm_up(id))
            .filter_map(|(id, request)| {
                let doublings = self
                    .tsgo_restarts
                    .doublings(&liveness_key(), request.at, timeout);
                let at = (request.at + timeout * 2_u32.pow(doublings))
                    .max(self.waiting_for_check_until(request)?);
                Some((at, id, request))
            })
            .collect::<Vec<_>>();
        due.sort_by_key(|(at, _, _)| *at);
        let last_answer = runtime.last_query_answer.get();
        if let Some((at, id, request)) =
            due.iter().find(|(_, _, request)| request.at >= last_answer)
        {
            restarts.push(TsgoRestart {
                at: *at,
                why: format!(
                    "nothing since {} ({id}), which it has not answered either",
                    request.method
                ),
                backoff: (liveness_key(), request.at),
            });
        }
        if let Some((at, _, _)) = due.get(TSRS_QUERY_CHECKERS - 1) {
            restarts.push(TsgoRestart {
                at: *at,
                why: format!(
                    "{} requests it has not answered: {}",
                    due.iter().filter(|(due, _, _)| due <= at).count(),
                    due.iter()
                        .take_while(|(due, _, _)| due <= at)
                        .map(|(_, id, request)| format!("{} ({id})", request.method))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                backoff: (liveness_key(), due[0].2.at),
            });
        }
        restarts
    }

    /// Answer every request tsgo has left unanswered for
    /// [`TSGO_ANSWER_TIMEOUT`] as if tsgo had nothing (each editor request
    /// gets its native fallback, or the empty answer, a component query goes
    /// on without that member), and ask tsgo to cancel it. A request tsrs
    /// loses would otherwise never be answered, and the background, which
    /// waits for tsgo to be idle, would never load another project.
    ///
    /// A diagnostic pull is not one of them ([`waits_for_tsgo`]): a check can
    /// take that long, and the editor gets tsgo's answer when it is done.
    ///
    /// tsrs loses a request by looping on it for good, one core busy and a
    /// cancellation ignored (`docs/upstream/tsrs-lost-request`), while it
    /// answers a slow one once it is done, cancelled or not. An overdue
    /// request stays tracked until tsgo answers it, and tsgo is restarted for
    /// those it loops on, or may, and when it stops answering
    /// ([`Self::tsgo_restart_candidates`]).
    fn expire_tsgo_requests(&mut self) {
        let Some(runtime) = &self.tsgo else {
            return;
        };
        let now = Instant::now();
        let expired = runtime
            .sent
            .borrow()
            .iter()
            .filter(|(_, request)| {
                !request.overdue
                    && !waits_for_tsgo(&request.method)
                    && now.duration_since(request.at) >= self.tsgo_answer_timeout
            })
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in expired {
            let Some(runtime) = &self.tsgo else {
                return;
            };
            let Some(mut request) = runtime.sent.borrow_mut().remove(&id) else {
                continue;
            };
            runtime.cancel(&id);
            let warm_up = self.background.warming.as_ref() == Some(&id);
            let method = request.method.clone();
            log::warn(format_args!(
                "tsgo did not answer {method} ({id}) in {} ms; answering without it{}",
                self.tsgo_answer_timeout.as_millis(),
                if request.loops {
                    " (a request tsrs loops on)"
                } else if may_loop(&method) {
                    " (a request tsrs may loop on)"
                } else {
                    ""
                }
            ));
            let waited_on = warm_up
                || self.pending_tsgo.contains_key(&id)
                || self.component_query_requests.contains_key(&id)
                || self.store_references.followups.contains_key(&id)
                || self.component_imports.followups.contains_key(&id);
            if waited_on {
                let response = if self.component_query_requests.contains_key(&id) {
                    Response::new_err(
                        id.clone(),
                        ErrorCode::RequestCanceled as i32,
                        "tsgo did not answer".to_string(),
                    )
                } else {
                    Response::new_ok(id.clone(), serde_json::Value::Null)
                };
                self.on_tsgo_message(Message::Response(response));
            }
            if let Some(runtime) = &self.tsgo {
                request.overdue = true;
                runtime.sent.borrow_mut().insert(id, request);
            }
        }
        let due = self
            .tsgo_restart_candidates()
            .into_iter()
            .filter(|restart| restart.at <= now)
            .collect::<Vec<_>>();
        if due.is_empty() {
            return;
        }
        log::warn(format_args!(
            "restarting tsgo, which is still busy with {}",
            due.iter()
                .map(|restart| restart.why.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        ));
        let mut counted = HashSet::new();
        for TsgoRestart {
            backoff: (key, sent),
            ..
        } in due
        {
            if counted.insert(key.clone()) {
                self.tsgo_restarts
                    .restarted(key, sent, now, self.tsgo_answer_timeout);
            }
        }
        self.abort_tsgo_requests("TypeScript service restarted: a request never finished");
        if let Some(runtime) = &mut self.tsgo {
            runtime.generation = None;
            let _ = runtime.client.restart();
        }
    }

    /// Hand the background thread its next job, when one is due: find the
    /// projects that own a component, then load them one at a time.
    fn step_background(&mut self) {
        if self
            .background_deadline()
            .is_none_or(|deadline| Instant::now() < deadline)
        {
            return;
        }
        let Some(runtime) = &mut self.tsgo else {
            return;
        };
        if !runtime.keep_all {
            runtime.keep_all = true;
            for index in 0..runtime.overlays.len() {
                runtime.keep_project_alive(index);
            }
        }
        let generation = self.background.generation;
        let Some(worker) = &self.background.worker else {
            return;
        };
        let Some(queue) = &mut self.background.queue else {
            let roots = runtime
                .overlays
                .iter()
                .filter(|overlay| overlay.workspace() == overlay.mirror_root())
                .map(|overlay| overlay.workspace().to_path_buf())
                .collect();
            if worker.submit(background::Job::Enumerate { generation, roots }) {
                self.background.enumerating = true;
            } else {
                self.background.worker = None;
            }
            return;
        };
        while let Some(spec) = queue.pop_front() {
            if runtime
                .overlays
                .iter()
                .any(|overlay| overlay.is_project(&spec) && overlay.is_loaded())
            {
                continue;
            }
            if worker.submit(background::Job::Load {
                generation,
                spec: spec.clone(),
            }) {
                self.background.in_flight = Some(spec);
            } else {
                self.background.worker = None;
            }
            return;
        }
        self.background.complete = true;
        log::info(format_args!(
            "every TypeScript project that owns a component is loaded"
        ));
    }

    fn on_background_done(&mut self, done: background::Done) {
        match done {
            background::Done::Enumerated {
                generation,
                projects,
            } => {
                if generation == self.background.generation {
                    self.background.enumerating = false;
                    self.background.queue = Some(projects.into());
                }
            }
            background::Done::Loaded {
                generation,
                spec,
                loaded,
            } => {
                if generation != self.background.generation {
                    return;
                }
                self.background.in_flight = None;
                match loaded {
                    Ok(loaded) => {
                        let loaded = *loaded;
                        let elapsed = loaded.elapsed;
                        let Some(index) =
                            self.install_project(&spec, loaded.overlay, loaded.events)
                        else {
                            return;
                        };
                        if let Some(runtime) = &self.tsgo {
                            let overlay = &runtime.overlays[index];
                            log::info(format_args!(
                                "loaded TypeScript project {} ({} components) in {} ms in the background",
                                overlay
                                    .source_tsconfig()
                                    .unwrap_or_else(|| overlay.workspace())
                                    .display(),
                                overlay.component_shadows().len(),
                                elapsed.as_millis()
                            ));
                        }
                        self.warm_up(index);
                    }
                    Err(error) => log::warn(format_args!(
                        "could not load tsgo project {}: {error}",
                        spec.config
                            .as_deref()
                            .unwrap_or(&spec.mirror_root)
                            .display()
                    )),
                }
            }
        }
    }

    /// Install a project loaded off the main loop, unless the editor got
    /// there first: replay the file events it missed, mirror the editor's
    /// buffers it holds, and keep it in tsgo. Returns its index.
    fn install_project(
        &mut self,
        spec: &ProjectSpec,
        overlay: TsgoOverlay,
        events: Vec<(Uri, lsp_types::FileChangeType)>,
    ) -> Option<usize> {
        let runtime = self.tsgo.as_mut()?;
        if runtime
            .overlays
            .iter()
            .any(|overlay| overlay.is_project(spec) && overlay.is_loaded())
        {
            return None;
        }
        let index = match runtime
            .overlays
            .iter()
            .position(|overlay| overlay.is_project(spec))
        {
            Some(index) => {
                runtime.overlays[index] = overlay;
                index
            }
            None => {
                runtime.overlays.push(overlay);
                runtime.overlays.len() - 1
            }
        };
        runtime.resolved_dirs.clear();
        let _ = runtime.client.notify_watched_files(events);
        let missed = std::mem::take(&mut self.background.missed_events);
        if !missed.is_empty() {
            let editor = editor_buffers(&self.documents);
            if let Some(runtime) = &mut self.tsgo {
                let overlay = &mut runtime.overlays[index];
                let update = match overlay.apply_file_changes(&missed, &editor) {
                    Ok(Some(update)) => Ok(update),
                    Ok(None) => overlay.refresh_except(&editor),
                    Err(error) => Err(error),
                };
                match update {
                    Ok(update) => {
                        let mut events = Vec::new();
                        runtime.deliver_update(index, &update, &mut events);
                        let _ = runtime.client.notify_watched_files(events);
                    }
                    Err(error) => log::warn(format_args!(
                        "could not refresh tsgo overlay for {}: {error}",
                        runtime.overlays[index].workspace().display()
                    )),
                }
            }
        }
        self.mirror_editor_buffers(index);
        if let Some(runtime) = &mut self.tsgo {
            runtime.keep_project_alive(index);
        }
        Some(index)
    }

    /// Have tsgo build `overlays[index]` now, while the editor is quiet,
    /// rather than at the editor's next request: one cheap request about the
    /// shadow that keeps the project open.
    fn warm_up(&mut self, index: usize) {
        let Some(runtime) = &self.tsgo else {
            return;
        };
        let Some((_, uri)) = runtime
            .overlays
            .get(index)
            .and_then(|overlay| runtime.keep_alive.get(overlay.cache_dir()))
        else {
            return;
        };
        self.next_request_id += 1;
        let id = RequestId::from(format!("{WARM_UP_ID}{}", self.next_request_id));
        let request = Request::new(
            id.clone(),
            "textDocument/documentSymbol".to_string(),
            serde_json::json!({ "textDocument": { "uri": uri } }),
        );
        if runtime.forward(request.into()).is_ok() {
            self.background.warming = Some(id);
        }
    }

    /// Wait for the background job in flight, if any, and take its result.
    fn wait_for_background(&mut self) {
        while self.background.enumerating || self.background.in_flight.is_some() {
            let Some(done) = self
                .background
                .worker
                .as_ref()
                .and_then(|worker| worker.done().recv().ok())
            else {
                self.background.enumerating = false;
                self.background.in_flight = None;
                return;
            };
            self.on_background_done(done);
        }
    }

    /// Load every project that owns a component before a workspace-wide
    /// request reaches tsgo, which searches only the projects it has: what
    /// the background has not loaded yet is loaded now.
    fn load_all_projects(&mut self) {
        if self.tsgo.is_none() || self.background.complete {
            return;
        }
        let started = Instant::now();
        self.wait_for_background();
        let Some(runtime) = &mut self.tsgo else {
            return;
        };
        if !runtime.keep_all {
            runtime.keep_all = true;
            for index in 0..runtime.overlays.len() {
                runtime.keep_project_alive(index);
            }
        }
        let queue = self.background.queue.take().unwrap_or_else(|| {
            runtime
                .overlays
                .iter()
                .filter(|overlay| overlay.workspace() == overlay.mirror_root())
                .flat_map(|overlay| tsgo_overlay::component_projects(overlay.workspace()))
                .collect()
        });
        let mut loaded = 0;
        for spec in queue {
            if self.tsgo.as_ref().is_some_and(|runtime| {
                runtime
                    .overlays
                    .iter()
                    .any(|overlay| overlay.is_project(&spec) && overlay.is_loaded())
            }) {
                continue;
            }
            match background::load(&spec, tsgo_overlay::MAX_PROJECTION_THREADS) {
                Ok(project) => {
                    if self
                        .install_project(&spec, project.overlay, project.events)
                        .is_some()
                    {
                        loaded += 1;
                    }
                }
                Err(error) => log::warn(format_args!(
                    "could not load tsgo project {}: {error}",
                    spec.config
                        .as_deref()
                        .unwrap_or(&spec.mirror_root)
                        .display()
                )),
            }
        }
        self.background.queue = Some(VecDeque::new());
        self.background.complete = true;
        log::info(format_args!(
            "loaded {loaded} more TypeScript projects for a workspace-wide request in {} ms",
            started.elapsed().as_millis()
        ));
    }

    /// Re-project, from their buffers, the editor's open components that
    /// `overlays[index]` holds.
    fn mirror_editor_buffers(&mut self, index: usize) {
        let Some(runtime) = &mut self.tsgo else {
            return;
        };
        for document in self.documents.iter() {
            let path = uri_to_path(document.uri.as_str());
            let path = crate::uri::canonicalize(&path).unwrap_or(path);
            let overlay = &mut runtime.overlays[index];
            if !is_svelte_document(&document.language_id, &path)
                || overlay.shadow_for_source(&path).is_none()
            {
                continue;
            }
            match overlay.open_or_update(&path, document.text(), document.version) {
                Ok(shadow) => open_shadow(&runtime.client, &mut runtime.open_shadows, &shadow),
                Err(error) => log::warn(format_args!(
                    "could not update tsgo shadow for {}: {error}",
                    path.display()
                )),
            }
        }
    }

    /// A project that loaded after the sidecar started still needs its
    /// members preprocessed, as the members of every loaded project are when
    /// the sidecar starts.
    fn preprocess_loaded_project(&mut self, index: usize) {
        self.preprocess_known_empty = false;
        if self.preprocess.is_none() {
            self.ensure_preprocess_runtime();
            return;
        }
        let members = self
            .tsgo
            .as_ref()
            .and_then(|runtime| runtime.overlays.get(index))
            .map(|overlay| {
                overlay
                    .eager_shadows()
                    .into_iter()
                    .filter_map(|shadow| {
                        let filename = uri_to_path(shadow.source_uri.as_str());
                        let text = overlay.source_text(&filename)?.to_string();
                        Some((filename, shadow.version, text))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for (filename, version, text) in members {
            if !self.is_editor_document(&filename) {
                let _ = self.queue_preprocess_path(filename, version, text);
            }
        }
    }

    fn close_tsgo_document(&mut self, uri: &Uri) {
        let Some(document) = self.documents.get(uri) else {
            return;
        };
        let language_id = document.language_id.clone();
        let path = uri_to_path(uri.as_str());
        let path = crate::uri::canonicalize(&path).unwrap_or(path);
        if let Some(runtime) = &mut self.tsgo {
            runtime.document_imports.remove(&path);
        }
        if let Some(shadow_uri) = self.probed_shadow_of(&path) {
            self.end_probe_in(&shadow_uri);
        }
        if is_svelte_document(&language_id, &path) {
            let mut closed_source = None;
            if let Some(runtime) = &mut self.tsgo
                && let Some(owner) = runtime.overlay_index_for_source(&path)
            {
                let copies = runtime.copies_of(owner, &path);
                let mut events = Vec::new();
                for index in std::iter::once(owner).chain(copies) {
                    let overlay = &mut runtime.overlays[index];
                    let shadow_uri = overlay
                        .shadow_for_source(&path)
                        .map(|shadow| shadow.shadow_uri.clone());
                    match overlay.close(&path) {
                        Ok(Some(shadow)) => {
                            if index == owner {
                                closed_source = overlay
                                    .source_text(&path)
                                    .map(|text| (shadow.version, text.to_string()));
                            }
                            // The disk projection reaches tsgo as the body on
                            // disk, written before the buffer closes so the
                            // close never exposes a stale one. A JavaScript
                            // component stays a buffer (see
                            // `JAVASCRIPT_SHADOW`).
                            runtime.open_shadows.remove(shadow.shadow_uri.as_str());
                            runtime.deliver_disk_shadow(index, &shadow, &mut events);
                            if !runtime
                                .open_shadows
                                .contains_key(shadow.shadow_uri.as_str())
                            {
                                close_shadow(
                                    &runtime.client,
                                    &mut runtime.open_shadows,
                                    shadow.shadow_uri,
                                );
                            }
                        }
                        Ok(None) => {
                            if let Some(shadow_uri) = shadow_uri {
                                close_shadow(
                                    &runtime.client,
                                    &mut runtime.open_shadows,
                                    shadow_uri.clone(),
                                );
                                events.push((shadow_uri, lsp_types::FileChangeType::DELETED));
                            }
                        }
                        Err(error) => log::warn(format_args!(
                            "could not close tsgo shadow for {}: {error}",
                            path.display()
                        )),
                    }
                }
                let _ = runtime.client.notify_watched_files(events);
            }
            self.preprocess_failures.remove(&path);
            self.preprocess_documents.remove(&path);
            self.preprocess_pending.remove(&path);
            self.preprocess_dependencies.remove(&path);
            if let Some((version, text)) = closed_source {
                let _ = self.queue_preprocess_path(path, version, text);
            } else if let Some(runtime) = &self.preprocess {
                let _ = runtime.client.remove(path);
            }
        } else if is_typescript_or_javascript(&language_id, &path) {
            let Some(runtime) = &mut self.tsgo else {
                return;
            };
            let Some(overlay) = runtime.overlay_for_source_mut(&path) else {
                return;
            };
            match overlay.close_plain(&path) {
                Ok(Some(shadow_uri)) => {
                    close_shadow(&runtime.client, &mut runtime.open_shadows, shadow_uri);
                }
                Ok(None) => {}
                Err(error) => log::warn(format_args!(
                    "could not close TypeScript route {}: {error}",
                    path.display()
                )),
            }
        }
    }

    /// The editor's open documents, as the overlays must see them.
    fn editor_buffers(&self) -> EditorBuffers {
        editor_buffers(&self.documents)
    }

    fn refresh_tsgo_overlays(&mut self, changes: &[(PathBuf, lsp_types::FileChangeType)]) {
        let editor = self.editor_buffers();
        let Some(runtime) = &mut self.tsgo else {
            return;
        };
        let mut updates = Vec::new();
        // A deferred project has no members to reconcile, and refreshing one
        // would load it.
        for (index, overlay) in runtime
            .overlays
            .iter_mut()
            .enumerate()
            .filter(|(_, overlay)| overlay.is_loaded())
        {
            let update = match overlay.apply_file_changes(changes, &editor) {
                Ok(Some(update)) => Ok(update),
                Ok(None) => overlay.refresh_except(&editor),
                Err(error) => Err(error),
            };
            match update {
                Ok(update) => updates.push((index, update)),
                Err(error) => log::warn(format_args!(
                    "could not refresh tsgo overlay for {}: {error}",
                    overlay.workspace().display()
                )),
            }
        }
        if updates
            .iter()
            .any(|(_, update)| !update.opened_or_changed.is_empty() || !update.buffers.is_empty())
        {
            self.preprocess_known_empty = false;
        }
        let Some(runtime) = &mut self.tsgo else {
            return;
        };
        let mut events = Vec::new();
        let mut javascript_arrived = Vec::new();
        for (index, update) in updates {
            runtime.deliver_update(index, &update, &mut events);
            if !update.closed.is_empty() {
                // The component that kept the project may be gone.
                runtime.keep_project_alive(index);
            }
            if update
                .opened_or_changed
                .iter()
                .chain(&update.buffers)
                .any(|shadow| shadow.language_id == JAVASCRIPT_SHADOW)
                && runtime
                    .activated
                    .contains(runtime.overlays[index].cache_dir())
            {
                javascript_arrived.push(index);
            }
        }
        let _ = runtime.client.notify_watched_files(events);
        // A JavaScript component that joined may be one an open document
        // reaches (an import that resolves now).
        if !javascript_arrived.is_empty() {
            let roots = editor.paths().cloned().collect::<Vec<_>>();
            for index in javascript_arrived {
                runtime.open_reachable_javascript(index, roots.iter().cloned());
            }
        }
    }

    /// A `package.json` changed: its `imports` are part of the overlay
    /// configs' `paths`, so tsgo rereads a config whose text changed.
    fn refresh_package_imports(&mut self) {
        let Some(runtime) = &self.tsgo else {
            return;
        };
        let mut events = Vec::new();
        for overlay in runtime
            .overlays
            .iter()
            .filter(|overlay| overlay.is_loaded())
        {
            match overlay.refresh_package_imports() {
                Ok(true) => {
                    if let Some(uri) = path_to_uri(overlay.tsconfig_path()) {
                        events.push((uri, lsp_types::FileChangeType::CHANGED));
                    }
                }
                Ok(false) => {}
                Err(error) => log::warn(format_args!(
                    "could not rewrite the tsgo config for {}: {error}",
                    overlay.workspace().display()
                )),
            }
        }
        let _ = runtime.client.notify_watched_files(events);
    }

    fn rebuild_tsgo_overlays(&mut self) {
        if self.tsgo.is_none() {
            return;
        }
        self.wait_for_background();
        self.background.reset();
        let Some(runtime) = &mut self.tsgo else {
            return;
        };
        let roots = runtime
            .overlays
            .iter()
            .filter(|overlay| overlay.mirror_root() == overlay.workspace())
            .map(|overlay| overlay.workspace().to_path_buf())
            .collect::<Vec<_>>();
        let mut rebuilt = Vec::with_capacity(roots.len());
        for root in roots {
            match TsgoOverlay::build_deferred(&root, None) {
                Ok(overlay) => rebuilt.push(overlay),
                Err(error) => log::warn(format_args!(
                    "could not rebuild tsgo overlay for {}: {error}",
                    root.display()
                )),
            }
        }
        if rebuilt.is_empty() {
            return;
        }
        runtime.resolved_dirs.clear();
        for uri in std::mem::take(&mut runtime.open_shadows).into_values() {
            let _ = runtime.client.close_buffer(uri);
        }
        runtime.activated.clear();
        runtime.javascript_buffers.clear();
        runtime.document_imports.clear();
        runtime.keep_alive.clear();
        // Every project is reloaded on demand: re-syncing the open documents
        // below loads theirs, and tsgo restarts afterwards.
        runtime.overlays = rebuilt;
        let open = self
            .documents
            .iter()
            .map(|document| document.uri.as_str().to_string())
            .collect::<Vec<_>>();
        for key in open {
            self.sync_tsgo_document(&key);
        }
        self.abort_tsgo_requests("TypeScript project configuration changed");
        if let Some(runtime) = &mut self.tsgo {
            runtime.generation = None;
            let _ = runtime.client.restart();
        }
    }

    fn update_tsgo_workspace_folders(
        &mut self,
        added: &[lsp_types::WorkspaceFolder],
        removed: &[lsp_types::WorkspaceFolder],
    ) {
        if self.tsgo.is_none() {
            return;
        }
        self.wait_for_background();
        self.background.reset();
        let Some(runtime) = &mut self.tsgo else {
            return;
        };
        for folder in removed {
            let path = uri_to_path(folder.uri.as_str());
            let path = crate::uri::canonicalize(&path).unwrap_or(path);
            // The folder's own project and every project nested in it.
            let (gone, kept) = std::mem::take(&mut runtime.overlays)
                .into_iter()
                .partition::<Vec<_>, _>(|overlay| overlay.mirror_root() == path);
            runtime.overlays = kept;
            if gone.is_empty() {
                continue;
            }
            runtime.resolved_dirs.clear();
            for overlay in gone {
                runtime.activated.remove(overlay.cache_dir());
                runtime.keep_alive.remove(overlay.cache_dir());
                for shadow in overlay.open_shadows() {
                    if runtime
                        .open_shadows
                        .contains_key(shadow.shadow_uri.as_str())
                    {
                        close_shadow(
                            &runtime.client,
                            &mut runtime.open_shadows,
                            shadow.shadow_uri.clone(),
                        );
                    }
                }
            }
        }
        for folder in added {
            let path = uri_to_path(folder.uri.as_str());
            let path = crate::uri::canonicalize(&path).unwrap_or(path);
            if runtime
                .overlays
                .iter()
                .any(|overlay| overlay.workspace() == path)
            {
                continue;
            }
            match TsgoOverlay::build_deferred(&path, None) {
                Ok(overlay) => {
                    runtime.resolved_dirs.clear();
                    runtime.overlays.push(overlay);
                }
                Err(error) => log::warn(format_args!(
                    "could not prepare tsgo overlay for {}: {error}",
                    path.display()
                )),
            }
        }
    }

    fn ensure_tsgo_runtime(&mut self) {
        if self.tsgo.is_some() {
            return;
        }
        let Some(runtime) = TsgoRuntime::start(&self.client, &self.initialize_params) else {
            return;
        };
        let _ = runtime.client.update_configuration(
            self.js_ts_settings.clone(),
            self.editor_settings.clone(),
            self.typescript_settings.clone(),
            self.javascript_settings.clone(),
        );
        self.tsgo = Some(runtime);
        let open = self
            .documents
            .iter()
            .map(|document| document.uri.as_str().to_string())
            .collect::<Vec<_>>();
        for key in open {
            self.sync_tsgo_document(&key);
        }
    }

    fn ensure_preprocess_runtime(&mut self) {
        if !self.client.is_trusted || !self.settings.preprocess_enable {
            self.preprocess = None;
            self.preprocess_failures.clear();
            self.preprocess_documents.clear();
            self.preprocess_pending.clear();
            self.preprocess_dependencies.clear();
            return;
        }
        if self.preprocess.is_some() || self.preprocess_known_empty {
            return;
        }
        let inputs = self.preprocess_inputs();
        if inputs.is_empty() {
            self.preprocess_known_empty = true;
            return;
        }
        let client = match PreprocessSidecar::spawn(
            PreprocessSidecarConfig::default(),
            self.preprocess_event_sender.clone(),
        ) {
            Ok(client) => client,
            Err(error) => {
                log::warn(format_args!(
                    "could not start preprocess supervisor: {error}"
                ));
                return;
            }
        };
        for input in inputs {
            let filename = crate::uri::canonicalize(&input.filename)
                .unwrap_or_else(|_| input.filename.clone());
            let version = input.version;
            if client.preprocess(input).is_ok() {
                self.preprocess_pending.insert(filename, version);
            }
        }
        self.preprocess = Some(PreprocessRuntime {
            client,
            generation: None,
        });
    }

    fn preprocess_inputs(&mut self) -> Vec<PreprocessInput> {
        let mut inputs = HashMap::<PathBuf, PreprocessInput>::new();
        if let Some(runtime) = &self.tsgo {
            // A project loaded only for workspace-wide requests is not
            // preprocessed until a document of it opens.
            for overlay in runtime
                .overlays
                .iter()
                .filter(|overlay| runtime.activated.contains(overlay.cache_dir()))
            {
                for shadow in overlay.eager_shadows() {
                    let filename = uri_to_path(shadow.source_uri.as_str());
                    if self
                        .preprocess_configs
                        .find(&filename, overlay.workspace())
                        .is_none()
                    {
                        continue;
                    }
                    let Some(text) = overlay.source_text(&filename) else {
                        continue;
                    };
                    inputs.insert(
                        filename.clone(),
                        PreprocessInput {
                            workspace: overlay.workspace().to_path_buf(),
                            filename,
                            version: shadow.version,
                            text: text.to_string(),
                        },
                    );
                }
            }
        }
        for document in self.documents.iter() {
            let filename = uri_to_path(document.uri.as_str());
            let Some(workspace) = self.preprocess_workspace_for_path(&filename) else {
                continue;
            };
            if self
                .preprocess_configs
                .find(&filename, &workspace)
                .is_none()
            {
                continue;
            }
            inputs.insert(
                filename.clone(),
                PreprocessInput {
                    workspace,
                    filename,
                    version: document.version,
                    text: document.text().to_string(),
                },
            );
        }
        inputs.into_values().collect()
    }

    fn queue_preprocess_document(&mut self, key: &str) -> bool {
        if self.preprocess_known_empty
            && let Some(document) = self.documents.get_by_key(key)
        {
            let filename = uri_to_path(document.uri.as_str());
            if let Some(workspace) = self.preprocess_workspace_for_path(&filename)
                && self
                    .preprocess_configs
                    .find(&filename, &workspace)
                    .is_some()
            {
                self.preprocess_known_empty = false;
            }
        }
        self.ensure_preprocess_runtime();
        let Some(document) = self.documents.get_by_key(key) else {
            return false;
        };
        let filename = uri_to_path(document.uri.as_str());
        let filename = crate::uri::canonicalize(&filename).unwrap_or(filename);
        self.preprocess_failures.remove(&filename);
        self.preprocess_documents.remove(&filename);
        self.queue_preprocess_path(filename, document.version, document.text().to_string())
    }

    fn queue_preprocess_path(&mut self, filename: PathBuf, version: i32, text: String) -> bool {
        let Some(workspace) = self.preprocess_workspace_for_path(&filename) else {
            return false;
        };
        if self
            .preprocess_configs
            .find(&filename, &workspace)
            .is_none()
        {
            self.preprocess_pending.remove(&filename);
            if let Some(runtime) = &self.preprocess {
                let _ = runtime.client.remove(filename);
            }
            return false;
        }
        if let Some(runtime) = &self.preprocess {
            let queued = runtime
                .client
                .preprocess(PreprocessInput {
                    workspace,
                    filename: filename.clone(),
                    version,
                    text,
                })
                .is_ok();
            if queued {
                self.preprocess_pending.insert(filename, version);
            }
            return queued;
        }
        false
    }

    /// Whether the sidecar has yet to answer this version of the document.
    /// Upstream compiles only the preprocessed text (`SvelteDocument`), so the
    /// compiler has nothing to report until then: the text as written would
    /// report what the preprocessor removes, such as a script's `enum`.
    fn awaiting_preprocess(&self, path: &Path, version: i32) -> bool {
        if self.preprocess.is_none() {
            return false;
        }
        let path = crate::uri::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        self.preprocess_pending.get(&path) == Some(&version)
    }

    fn preprocess_workspace_for_path(&self, path: &Path) -> Option<PathBuf> {
        if let Some(runtime) = &self.tsgo
            && let Some(overlay) = runtime
                .overlays
                .iter()
                .filter(|overlay| path.starts_with(overlay.workspace()))
                .max_by_key(|overlay| overlay.workspace().components().count())
        {
            return Some(overlay.workspace().to_path_buf());
        }
        self.client
            .workspace_folders
            .iter()
            .map(|folder| uri_to_path(folder.uri.as_str()))
            .chain(
                self.client
                    .root_uri
                    .iter()
                    .map(|root| uri_to_path(root.as_str())),
            )
            .filter(|workspace| path.starts_with(workspace))
            .max_by_key(|workspace| workspace.components().count())
    }

    fn register_preprocess_dependencies(
        &mut self,
        filename: &Path,
        config_path: Option<&Path>,
        dependencies: &[PathBuf],
    ) {
        let base = config_path
            .and_then(Path::parent)
            .or_else(|| filename.parent())
            .unwrap_or(Path::new("."));
        let dependencies = dependencies
            .iter()
            .map(|dependency| {
                let dependency = if dependency.is_absolute() {
                    dependency.clone()
                } else {
                    base.join(dependency)
                };
                crate::uri::canonicalize(&dependency).unwrap_or(dependency)
            })
            .collect::<HashSet<_>>();
        self.preprocess_dependencies
            .insert(filename.to_path_buf(), dependencies.clone());
        if !self.client.dynamic_watched_files {
            return;
        }
        let candidate_directories = dependencies
            .into_iter()
            .filter_map(|dependency| {
                self.preprocess_workspace_for_path(&dependency)?;
                dependency.parent().map(Path::to_path_buf)
            })
            .collect::<Vec<_>>();
        let watch_directories = candidate_directories
            .into_iter()
            .filter(|directory| {
                self.watched_preprocess_directories
                    .insert(directory.clone())
            })
            .collect::<Vec<_>>();
        let watchers = watch_directories
            .into_iter()
            .filter_map(|directory| {
                let base_uri = path_to_uri(&directory)?;
                Some(serde_json::json!({
                    "globPattern": {
                        "baseUri": base_uri,
                        "pattern": "*",
                    }
                }))
            })
            .collect::<Vec<_>>();
        if watchers.is_empty() {
            return;
        }
        self.next_request_id += 1;
        let id = RequestId::from(format!(
            "rsvelte-preprocess-dependencies-{}",
            self.next_request_id
        ));
        self.outgoing
            .insert(id.clone(), Outgoing::WatchedFilesRegistration);
        self.send(Request::new(
            id,
            "client/registerCapability".to_string(),
            serde_json::json!({
                "registrations": [{
                    "id": format!("rsvelte-preprocess-dependencies-{}", self.next_request_id),
                    "method": "workspace/didChangeWatchedFiles",
                    "registerOptions": { "watchers": watchers },
                }],
            }),
        ));
    }

    fn restart_preprocessing(&mut self) {
        self.preprocess_configs.clear();
        self.preprocess_known_empty = false;
        self.preprocess_failures.clear();
        self.preprocess_documents.clear();
        self.preprocess_pending.clear();
        self.preprocess_dependencies.clear();
        self.preprocess = None;
        self.ensure_preprocess_runtime();
    }

    fn on_preprocess_event(&mut self, event: PreprocessEvent) {
        match event {
            PreprocessEvent::Ready { generation } => {
                if let Some(runtime) = &mut self.preprocess {
                    runtime.generation = Some(generation);
                }
            }
            PreprocessEvent::Result(output) => {
                if self
                    .preprocess
                    .as_ref()
                    .and_then(|runtime| runtime.generation)
                    != Some(output.generation)
                {
                    return;
                }
                let filename =
                    crate::uri::canonicalize(&output.filename).unwrap_or(output.filename);
                if self.preprocess_pending.get(&filename) == Some(&output.version) {
                    self.preprocess_pending.remove(&filename);
                }
                let original = self
                    .documents
                    .iter()
                    .find(|document| {
                        let path = uri_to_path(document.uri.as_str());
                        crate::uri::canonicalize(&path).unwrap_or(path) == filename
                    })
                    .filter(|document| document.version == output.version)
                    .map(|document| document.text().to_string())
                    .or_else(|| {
                        self.tsgo.as_ref().and_then(|runtime| {
                            let overlay = runtime.overlay_for_source(&filename)?;
                            overlay
                                .shadow_for_source(&filename)
                                .filter(|shadow| shadow.version == output.version)
                                .and_then(|_| overlay.source_text(&filename).map(str::to_string))
                        })
                    });
                let Some(original) = original else {
                    return;
                };
                self.preprocess_failures.remove(&filename);
                if output.has_preprocessor {
                    self.preprocess_documents.insert(
                        filename.clone(),
                        PreprocessDocumentState {
                            version: output.version,
                            text: Arc::new(output.code.clone()),
                            map: output.map.clone().map(Arc::new),
                            identity: original == output.code,
                        },
                    );
                } else {
                    self.preprocess_documents.remove(&filename);
                }
                self.register_preprocess_dependencies(
                    &filename,
                    output.config_path.as_deref(),
                    &output.dependencies,
                );
                // The preprocessed text is the compiler's; tsgo keeps checking the
                // component as written (`DocumentSnapshot.ts`), which a script
                // preprocessor would have stripped of its types.
                let open_key = self
                    .documents
                    .iter()
                    .find(|document| {
                        let path = uri_to_path(document.uri.as_str());
                        crate::uri::canonicalize(&path).unwrap_or(path) == filename
                            && document.version == output.version
                    })
                    .map(|document| document.uri.as_str().to_string());
                if let Some(key) = open_key {
                    self.refresh_document_diagnostics(key);
                }
            }
            PreprocessEvent::Failed {
                generation,
                filename,
                version,
                message,
            } => {
                if self
                    .preprocess
                    .as_ref()
                    .and_then(|runtime| runtime.generation)
                    != Some(generation)
                {
                    return;
                }
                log::warn(format_args!("preprocessing failed: {message}"));
                if let (Some(filename), Some(version)) = (filename, version) {
                    let filename = crate::uri::canonicalize(&filename).unwrap_or(filename);
                    if self.preprocess_pending.get(&filename) == Some(&version) {
                        self.preprocess_pending.remove(&filename);
                    }
                    self.preprocess_failures
                        .insert(filename.clone(), (version, message));
                    self.preprocess_documents.remove(&filename);
                    self.preprocess_dependencies.remove(&filename);
                    let open_key = self
                        .documents
                        .iter()
                        .find(|document| {
                            let path = uri_to_path(document.uri.as_str());
                            crate::uri::canonicalize(&path).unwrap_or(path) == filename
                                && document.version == version
                        })
                        .map(|document| document.uri.as_str().to_string());
                    if let Some(key) = open_key {
                        self.refresh_document_diagnostics(key);
                    }
                }
            }
            PreprocessEvent::Crashed {
                generation,
                status,
                error,
            } => {
                if let Some(runtime) = &mut self.preprocess
                    && runtime.generation == Some(generation)
                {
                    runtime.generation = None;
                }
                log::warn(format_args!(
                    "preprocess sidecar crashed ({status:?}): {error}"
                ));
            }
            PreprocessEvent::CircuitOpen {
                generation,
                crashes,
                error,
            } => {
                if let Some(runtime) = &mut self.preprocess
                    && runtime.generation == Some(generation)
                {
                    runtime.generation = None;
                }
                log::warn(format_args!(
                    "preprocess sidecar restart circuit opened after {crashes} crashes: {error}"
                ));
                self.preprocess_pending.clear();
                for input in self.preprocess_inputs() {
                    self.preprocess_documents.remove(&input.filename);
                    self.preprocess_failures
                        .insert(input.filename.clone(), (input.version, error.clone()));
                    let open_key = self
                        .documents
                        .iter()
                        .find(|document| {
                            let path = uri_to_path(document.uri.as_str());
                            crate::uri::canonicalize(&path).unwrap_or(path) == input.filename
                                && document.version == input.version
                        })
                        .map(|document| document.uri.as_str().to_string());
                    if let Some(key) = open_key {
                        self.refresh_document_diagnostics(key);
                    }
                }
            }
        }
    }

    fn on_response(&mut self, mut response: Response) {
        let Some(outgoing) = self.outgoing.remove(&response.id) else {
            log::warn(format_args!("response to unknown request {}", response.id));
            return;
        };
        match outgoing {
            Outgoing::Configuration => {
                let preprocessing_was_enabled = self.settings.preprocess_enable;
                self.settings = match response.response_result {
                    Ok(value) => {
                        let items = value.as_array();
                        self.js_ts_settings = items
                            .and_then(|items| items.get(2))
                            .cloned()
                            .unwrap_or(serde_json::Value::Null);
                        self.typescript_settings = items
                            .and_then(|items| items.get(3))
                            .cloned()
                            .unwrap_or(serde_json::Value::Null);
                        self.javascript_settings = items
                            .and_then(|items| items.get(4))
                            .cloned()
                            .unwrap_or(serde_json::Value::Null);
                        self.editor_settings = items
                            .and_then(|items| items.get(5))
                            .cloned()
                            .unwrap_or(serde_json::Value::Null);
                        Settings::from_sections(
                            items
                                .and_then(|items| items.first())
                                .unwrap_or(&serde_json::Value::Null),
                            items
                                .and_then(|items| items.get(1))
                                .unwrap_or(&serde_json::Value::Null),
                        )
                        .with_emmet(
                            items
                                .and_then(|items| items.get(6))
                                .unwrap_or(&serde_json::Value::Null),
                        )
                    }
                    Err(err) => {
                        log::warn(format_args!(
                            "workspace/configuration failed: {}",
                            err.message
                        ));
                        Settings::default()
                    }
                };
                if let Some(runtime) = &self.tsgo
                    && let Err(error) = runtime.client.update_configuration(
                        self.js_ts_settings.clone(),
                        self.editor_settings.clone(),
                        self.typescript_settings.clone(),
                        self.javascript_settings.clone(),
                    )
                {
                    log::warn(format_args!("could not update tsgo settings: {error}"));
                }
                if preprocessing_was_enabled && !self.settings.preprocess_enable {
                    self.preprocess = None;
                    self.preprocess_failures.clear();
                    self.preprocess_documents.clear();
                    self.preprocess_pending.clear();
                    self.preprocess_dependencies.clear();
                    self.rebuild_tsgo_overlays();
                } else {
                    self.ensure_preprocess_runtime();
                }
                if !self.client.pull_diagnostics {
                    self.relint_open_documents();
                }
            }
            Outgoing::WatchedFilesRegistration => {}
            Outgoing::DiagnosticRefresh => {}
            Outgoing::ApplyEdit { command_id } => {
                self.respond(Response::new_ok(command_id, serde_json::Value::Null));
            }
            Outgoing::Tsgo { child_id } => {
                response.id = child_id;
                if let Some(runtime) = &self.tsgo {
                    let _ = runtime.forward(response.into());
                }
            }
        }
    }

    fn on_tsgo_event(&mut self, event: TsgoEvent) {
        match event {
            TsgoEvent::Ready {
                generation,
                capabilities: _,
            } => {
                if let Some(runtime) = &mut self.tsgo {
                    runtime.generation = Some(generation);
                    // It has answered nothing yet, and need not have.
                    runtime.last_query_answer.set(Instant::now());
                }
                // A restarted tsgo registers its watchers again.
                if let Some(params) = self.tsgo_watchers.reset() {
                    self.send_watcher_change("client/unregisterCapability", params);
                }
                // The diagnostics the previous one was asked for were
                // answered without it.
                if generation > 1 {
                    self.refresh_all_diagnostics();
                }
            }
            TsgoEvent::Message {
                generation,
                message,
            } => {
                if self
                    .tsgo
                    .as_ref()
                    .is_some_and(|runtime| runtime.generation == Some(generation))
                {
                    self.on_tsgo_message(message);
                }
            }
            TsgoEvent::Crashed {
                generation,
                status,
                error,
            } => {
                if let Some(runtime) = &mut self.tsgo {
                    runtime.generation = None;
                }
                log::warn(format_args!(
                    "tsgo generation {generation} crashed ({status:?}): {error}"
                ));
                self.abort_tsgo_requests("TypeScript service restarted before answering");
            }
        }
    }

    fn abort_tsgo_requests(&mut self, message: &str) {
        self.background.warming = None;
        if let Some(runtime) = &self.tsgo {
            runtime.forget_requests();
        }
        let stale_child_requests = self
            .outgoing
            .iter()
            .filter_map(|(id, outgoing)| {
                matches!(outgoing, Outgoing::Tsgo { .. }).then_some(id.clone())
            })
            .collect::<Vec<_>>();
        for id in stale_child_requests {
            self.outgoing.remove(&id);
            self.send(Notification::new(
                "$/cancelRequest".to_string(),
                serde_json::json!({ "id": id }),
            ));
        }
        for (id, pending) in std::mem::take(&mut self.pending_tsgo) {
            if let Some((uri, version)) = pending.push_diagnostics {
                let diagnostics = pending
                    .fallback_result
                    .and_then(|report| report.get("items").cloned())
                    .and_then(|items| serde_json::from_value(items).ok())
                    .unwrap_or_default();
                self.publish(uri, version, diagnostics);
                continue;
            }
            if matches!(pending.rename, Some(PendingRename::Followup { .. })) {
                continue;
            }
            if let Some(fallback) = pending.fallback_result {
                self.respond(Response::new_ok(id, fallback));
            } else {
                self.respond(Response::new_err(
                    id,
                    ErrorCode::InternalError as i32,
                    message.to_string(),
                ));
            }
        }
        for (id, _) in std::mem::take(&mut self.rename_aggregates) {
            self.respond(Response::new_err(
                id,
                ErrorCode::InternalError as i32,
                message.to_string(),
            ));
        }
        let store_references = std::mem::take(&mut self.store_references);
        for (id, _) in store_references.gathering {
            self.respond(Response::new_err(
                id,
                ErrorCode::InternalError as i32,
                message.to_string(),
            ));
        }
        let component_imports = std::mem::take(&mut self.component_imports);
        for (id, _) in component_imports.gathering {
            self.respond(Response::new_err(
                id,
                ErrorCode::InternalError as i32,
                message.to_string(),
            ));
        }
        self.component_query_requests.clear();
        for (id, pending) in std::mem::take(&mut self.component_queries) {
            self.run_component_action(pending.query.restore());
            self.respond(Response::new_err(
                id,
                ErrorCode::InternalError as i32,
                message.to_string(),
            ));
        }
        self.replay_deferred();
    }

    fn on_tsgo_message(&mut self, message: Message) {
        match message {
            Message::Response(mut response) => {
                let late = self.tsgo.as_ref().is_some_and(|runtime| {
                    runtime
                        .answered(&response.id)
                        .is_some_and(|request| request.overdue)
                });
                if self.background.warming.as_ref() == Some(&response.id) {
                    self.background.warming = None;
                    return;
                }
                if self.on_component_query_response(response.clone()) {
                    return;
                }
                if let Some(editor_id) = self.store_references.followups.remove(&response.id) {
                    let locations = match response.response_result {
                        Ok(serde_json::Value::Array(locations)) => locations,
                        _ => Vec::new(),
                    };
                    self.continue_store_references(&editor_id, locations, true);
                    return;
                }
                if let Some((editor_id, followup)) =
                    self.component_imports.followups.remove(&response.id)
                {
                    self.continue_component_imports(
                        &editor_id,
                        followup,
                        response.response_result.ok(),
                    );
                    return;
                }
                let Some(pending) = self.pending_tsgo.remove(&response.id) else {
                    log::warn(format_args!(
                        "response to unknown tsgo request {}{}",
                        response.id,
                        if late {
                            " (answered late: it was overdue)"
                        } else {
                            ""
                        }
                    ));
                    return;
                };
                if let Some(rename) = pending.rename {
                    self.on_tsgo_rename_response(response, rename);
                    return;
                }
                let pending = match (pending.include_declaration, &response.response_result) {
                    (Some(include_declaration), Ok(serde_json::Value::Array(locations)))
                        if !self.store_references.gathered.remove(&response.id) =>
                    {
                        let locations = locations.clone();
                        let id = response.id.clone();
                        self.store_references.gathering.insert(
                            id.clone(),
                            StoreReferencesGathering {
                                pending,
                                include_declaration,
                                locations: Vec::new(),
                                covered: HashSet::new(),
                                remaining: 0,
                            },
                        );
                        self.continue_store_references(&id, locations, false);
                        return;
                    }
                    _ => pending,
                };
                let pending = match &response.response_result {
                    Ok(result)
                        if !pending.component_imports.is_empty()
                            && !self.component_imports.gathered.remove(&response.id) =>
                    {
                        let result = result.clone();
                        self.start_component_imports(response.id.clone(), pending, result);
                        return;
                    }
                    _ => pending,
                };
                let PendingTsgoRequest {
                    method,
                    document,
                    fallback_result,
                    completion_site,
                    rename: _,
                    code_action_diagnostic_codes,
                    push_diagnostics,
                    component_site,
                    component_references,
                    file_rename,
                    code_lens_resolve,
                    completion_site_data,
                    include_declaration: _,
                    component_imports: _,
                } = pending;
                let source_path = document
                    .as_ref()
                    .map(|document| uri_to_path(document.source_uri().as_str()));
                let source_uri = document
                    .as_ref()
                    .map(|document| document.source_uri().clone());
                let completion_context =
                    CompletionRewriteContext::new(source_path.as_deref(), true);
                let mut adopted_completion_data = None;
                if let Ok(result) = &mut response.response_result {
                    // Upstream never carries tsgo's enclosing-declaration span:
                    // `LocationLink.create(uri, defLocation.range, defLocation.range, …)`.
                    // Collapsing it before the mapping also keeps a link whose
                    // enclosing statement merely touches an `Ωignore` region.
                    if method == "textDocument/definition" {
                        normalize_definition_result(result);
                    }
                    // Upstream filters inlay hints BEFORE mapping, on generated
                    // positions (`InlayHintProvider.ts:66-75`); after mapping the
                    // offsets it tests no longer exist.
                    if method == "textDocument/inlayHint"
                        && let Some(runtime) = &self.tsgo
                        && let Some(source) = source_path.as_deref()
                        && let Some(overlay) = runtime.overlay_for_source(source)
                        && let Some(shadow) = overlay.shadow_for_source(source)
                    {
                        let shadow_path = uri_to_path(shadow.shadow_uri.as_str());
                        filter_generated_inlay_hints(result, overlay, &shadow_path);
                    }
                    // New imports are placed before mapping, from the generated
                    // line before them (`CompletionProvider.ts:1135-1143`).
                    // Code actions' import fixes are placed the same way
                    // (`CodeActionsProvider.ts:790-807`).
                    let mut new_imports = Vec::new();
                    let mut code_action_imports = Vec::new();
                    if matches!(
                        method.as_str(),
                        "completionItem/resolve" | "textDocument/codeAction"
                    ) && let Some(runtime) = &self.tsgo
                        && let Some(document) = document.as_ref()
                        && let Some((overlay, shadow_path)) =
                            shadow_overlay(&runtime.overlays, document)
                        && let Some(text) = self
                            .documents
                            .get(document.source_uri())
                            .map(Document::text)
                    {
                        let placement = ImportPlacement {
                            source: text,
                            trigger: completion_site_data.as_ref().and_then(|(_, position)| {
                                serde_json::from_value(position.clone()).ok()
                            }),
                            script_language: Some(
                                self.settings.svelte.default_script_language.as_str(),
                            ),
                        };
                        if method == "completionItem/resolve" {
                            new_imports =
                                take_new_import_edits(result, overlay, &shadow_path, &placement);
                        } else {
                            code_action_imports = take_new_imports(
                                result,
                                overlay,
                                &shadow_path,
                                document.shadow_uri().as_str(),
                                &placement,
                            );
                        }
                    }
                    if let Some(runtime) = &self.tsgo {
                        let mut mapper = TsgoResponseMapper::for_overlays_with_default_document(
                            &runtime.overlays,
                            document,
                        );
                        if let Some(rename) = &file_rename {
                            let _ = mapper.add_uri_alias(
                                rename.new_source.clone(),
                                rename.new_shadow.clone(),
                                &rename.old_source,
                            );
                        }
                        if method.starts_with("textDocument/semanticTokens/") {
                            mapper.set_semantic_token_types(semantic_token_types(&self.client));
                        }
                        mapper.map_response(&method, result);
                    }
                    if !code_action_imports.is_empty()
                        && let Some(uri) = source_uri.as_ref()
                    {
                        restore_new_imports(result, uri.as_str(), code_action_imports);
                    }
                    if !new_imports.is_empty()
                        && let Some(item) = result.as_object_mut()
                    {
                        let edits = item
                            .entry("additionalTextEdits")
                            .or_insert_with(|| serde_json::Value::Array(Vec::new()));
                        if let Some(edits) = edits.as_array_mut() {
                            edits.extend(new_imports);
                        }
                    }
                    // The same component can sit in several projects (its
                    // owner and the copies others keep for their imports),
                    // and their answers map to the same source location.
                    if matches!(
                        method.as_str(),
                        "workspace/symbol"
                            | "textDocument/references"
                            | "textDocument/implementation"
                            | "textDocument/definition"
                            | "textDocument/typeDefinition"
                    ) {
                        dedupe_array(result);
                    }
                    if matches!(
                        method.as_str(),
                        "textDocument/references"
                            | "textDocument/implementation"
                            | "textDocument/definition"
                            | "textDocument/typeDefinition"
                            | "textDocument/documentHighlight"
                    ) {
                        self.drop_locations_in_script_tags(
                            result,
                            source_uri.as_ref(),
                            matches!(
                                method.as_str(),
                                "textDocument/definition"
                                    | "textDocument/typeDefinition"
                                    | "textDocument/implementation"
                            ),
                        );
                    }
                    // The second half of upstream's inlay-hint filtering runs
                    // AFTER mapping (`InlayHintProvider.ts:84-88`), because the
                    // question it asks — which attribute is this hint on — only
                    // the source document can answer.
                    if method == "textDocument/inlayHint"
                        && let Some(text) = source_uri
                            .as_ref()
                            .and_then(|uri| self.documents.get(uri))
                            .map(Document::text)
                    {
                        filter_source_inlay_hints(result, text);
                    }
                    match method.as_str() {
                        "textDocument/documentSymbol" => {
                            if let Some(text) = source_uri
                                .as_ref()
                                .and_then(|uri| self.documents.get(uri))
                                .map(Document::text)
                            {
                                rewrite_document_symbols(result, text);
                            }
                        }
                        "textDocument/completion" => {
                            let mut strip_commits = false;
                            rewrite_completion_response_for_context(result, completion_context);
                            // `CompletionProvider.ts:451`: a document svelte2tsx
                            // rejected is answered from a fragment, so the list
                            // is incomplete and `PluginHost.ts:285-296` narrows
                            // it here rather than leaving it to the editor.
                            if let Some(path) = source_path.as_deref()
                                && self
                                    .tsgo
                                    .as_ref()
                                    .and_then(|runtime| runtime.overlay_for_source(path))
                                    .is_some_and(|overlay| overlay.parser_error(path).is_some())
                            {
                                // `CompletionProvider.ts:785-789`: outside a
                                // `<script>` such a response carries no commit
                                // characters at all — applied after the adopt
                                // below, which installs the default set.
                                strip_commits =
                                    completion_site_data.as_ref().is_some_and(|(_, position)| {
                                        !position_is_in_script(
                                            source_uri
                                                .as_ref()
                                                .and_then(|uri| self.documents.get(uri))
                                                .map_or("", Document::text),
                                            position,
                                        )
                                    });
                                let prefix = self
                                    .client
                                    .filter_incomplete_completions
                                    .then(|| {
                                        source_uri
                                            .as_ref()
                                            .and_then(|uri| self.documents.get(uri))
                                            .zip(completion_site_data.as_ref())
                                            .map(|(document, (_, position))| {
                                                incomplete_completion_filter(
                                                    document.text(),
                                                    position,
                                                )
                                            })
                                    })
                                    .flatten();
                                mark_completion_list_incomplete(result, prefix.as_deref());
                            }
                            if let Some((uri, position)) = &completion_site_data {
                                adopted_completion_data = Some((
                                    completion_data_key(uri, position),
                                    adopt_upstream_completion_data(result, uri, position),
                                ));
                            }
                            if let Some(site) = completion_site {
                                let (count, first_is_member) = completion_result_shape(result);
                                if !matches!(
                                    completion_action(site, count, first_is_member),
                                    CompletionAction::Forward
                                ) {
                                    *result = empty_completion_list();
                                }
                            }
                            if strip_commits {
                                strip_commit_characters(result);
                            }
                        }
                        "completionItem/resolve" => {
                            rewrite_completion_item_for_context(result, completion_context);
                            if let Some((uri, position)) = &completion_site_data {
                                adopt_upstream_item_data(result, uri, position);
                            }
                            normalize_completion_documentation(result);
                        }
                        "textDocument/codeAction" => {
                            if let Some(uri) = source_uri.as_ref()
                                && let Some(source) = self.documents.get(uri)
                                && is_svelte_document(
                                    &source.language_id,
                                    &uri_to_path(uri.as_str()),
                                )
                            {
                                let parser_error = document_has_parser_error(source.text());
                                let context = TsgoCodeActionContext::new(uri, source.text())
                                    .with_parser_error(parser_error)
                                    .with_default_script_language(
                                        (self.settings.svelte.default_script_language != "none")
                                            .then_some(
                                                self.settings
                                                    .svelte
                                                    .default_script_language
                                                    .as_str(),
                                            ),
                                    )
                                    .with_diagnostic_codes(&code_action_diagnostic_codes);
                                rewrite_code_action_response(result, &context);
                            }
                        }
                        "textDocument/codeLens" => {
                            if let Some(uri) = source_uri.as_ref() {
                                prepare_code_lenses(result, uri);
                                if self.component_reference_code_lens_enabled(uri)
                                    && let Some(lenses) = result.as_array_mut()
                                {
                                    lenses.push(component_reference_code_lens(uri));
                                }
                            }
                        }
                        "textDocument/hover" => {
                            normalize_hover_result(result);
                            if let Some(document) =
                                source_uri.as_ref().and_then(|uri| self.documents.get(uri))
                            {
                                widen_hover_range_over_string_quotes(result, document.text());
                            }
                        }
                        _ => {}
                    }
                    rewrite_visible_tsgo_response(result);
                    if component_references.is_some() {
                        let locations =
                            serde_json::from_value::<Vec<lsp_types::Location>>(result.clone())
                                .unwrap_or_default();
                        let texts = locations
                            .iter()
                            .map(|location| {
                                self.documents
                                    .get(&location.uri)
                                    .map(|document| document.text().to_string())
                                    .or_else(|| {
                                        fs::read_to_string(uri_to_path(location.uri.as_str())).ok()
                                    })
                            })
                            .collect::<Vec<_>>();
                        let references =
                            locations.into_iter().zip(&texts).map(|(location, text)| {
                                ComponentReference {
                                    location,
                                    source_text: text.as_deref(),
                                    is_definition: false,
                                    is_generated: false,
                                }
                            });
                        *result = serde_json::to_value(filter_component_references(references))
                            .unwrap_or_else(|_| serde_json::Value::Array(Vec::new()));
                    }
                    if let Some(file_rename) = &file_rename {
                        let mapping = WillRenameMapping {
                            old: ShadowUriPair {
                                source_uri: &file_rename.old_source,
                                shadow_uri: &file_rename.old_shadow,
                            },
                            new: ShadowUriPair {
                                source_uri: &file_rename.new_source,
                                shadow_uri: &file_rename.new_shadow,
                            },
                        };
                        let owned_pairs = self
                            .tsgo
                            .as_ref()
                            .map(|runtime| {
                                runtime
                                    .overlays
                                    .iter()
                                    .flat_map(TsgoOverlay::eager_shadows)
                                    .map(|shadow| {
                                        (shadow.source_uri.clone(), shadow.shadow_uri.clone())
                                    })
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default();
                        let pairs = owned_pairs
                            .iter()
                            .map(|(source, shadow)| ShadowUriPair {
                                source_uri: source,
                                shadow_uri: shadow,
                            })
                            .collect::<Vec<_>>();
                        rewrite_will_rename_result(result, &[mapping], &pairs);
                    }
                    if let Some(mut resolve) = code_lens_resolve {
                        let locations = locations_from_tsgo_result(result);
                        resolve_code_lens(
                            &mut resolve.lens,
                            resolve.kind,
                            &resolve.source_uri,
                            locations,
                        );
                        *result = resolve.lens;
                    }
                    if method == "textDocument/foldingRange" {
                        drop_degenerate_folding_ranges(result, self.client.line_folding_only);
                    }
                    if method == "textDocument/diagnostic" {
                        fill_diagnostic_tags(result);
                        adjust_diagnostic_messages(result);
                        if let Some(document) =
                            source_uri.as_ref().and_then(|uri| self.documents.get(uri))
                        {
                            drop_syntax_errors_in_script_tags(result, document.text());
                        }
                        let javascript = source_path.as_deref().is_some_and(|source| {
                            self.tsgo
                                .as_ref()
                                .and_then(|runtime| runtime.overlay_for_source(source))
                                .and_then(|overlay| overlay.shadow_for_source(source))
                                .is_some_and(|shadow| shadow.language_id == "javascriptreact")
                        });
                        if javascript {
                            label_javascript_diagnostics(result);
                        }
                    }
                    if let Some(fallback) = fallback_result {
                        merge_tsgo_result(&method, result, fallback);
                    }
                } else if let Some(fallback) = fallback_result {
                    response.response_result = Ok(fallback);
                }
                // tsgo answers `null` where it has nothing; upstream's plugins do
                // too, but `PluginHost.getCompletions` still returns the
                // `CompletionList.create([], false)` its signature promises.
                if method == "textDocument/completion"
                    && let Ok(result) = &mut response.response_result
                    && result.is_null()
                {
                    *result = empty_completion_list();
                }
                if let Some((key, stash)) = adopted_completion_data {
                    self.remember_completion_data(key, stash);
                }
                if let Some((uri, version)) = push_diagnostics {
                    let diagnostics = response
                        .response_result
                        .as_ref()
                        .ok()
                        .and_then(|result| result.get("items"))
                        .cloned()
                        .and_then(|items| serde_json::from_value(items).ok())
                        .unwrap_or_default();
                    self.publish(uri, version, diagnostics);
                    return;
                }
                if method == "textDocument/completion"
                    && let (Some(site), Some(uri), Ok(result)) = (
                        component_site,
                        source_uri.as_ref(),
                        response.response_result.as_ref(),
                    )
                    && self.start_component_query(response.id.clone(), uri, site, result.clone())
                {
                    return;
                }
                self.respond(response);
            }
            Message::Notification(mut notification) => {
                // A shadow open only to keep its project is no editor
                // document: a client that takes pushed diagnostics must not
                // get them for it.
                if notification.method == "textDocument/publishDiagnostics"
                    && let Some(runtime) = &self.tsgo
                    && let Some((source, _)) = notification
                        .params
                        .get("uri")
                        .and_then(serde_json::Value::as_str)
                        .and_then(|uri| {
                            runtime
                                .keep_alive
                                .values()
                                .find(|(_, shadow)| shadow.as_str() == uri)
                        })
                    && !self.is_editor_document(source)
                {
                    return;
                }
                if let Some(runtime) = &self.tsgo {
                    let mapper = TsgoResponseMapper::for_overlays(&runtime.overlays);
                    if !mapper.map_child_params(&notification.method, &mut notification.params) {
                        return;
                    }
                }
                self.send(notification);
            }
            Message::Request(mut request) => {
                // A watcher the editor already has covers what tsgo asks
                // for: answer here rather than register it again.
                let watchers = match request.method.as_str() {
                    "client/registerCapability" => {
                        Some((self.tsgo_watchers.register(&request.params), None))
                    }
                    "client/unregisterCapability" => {
                        Some(self.tsgo_watchers.unregister(&request.params))
                    }
                    _ => None,
                };
                if let Some((forward, register)) = watchers {
                    if let Some(params) = register {
                        self.send_watcher_change("client/registerCapability", params);
                    }
                    match forward {
                        Some(params) => request.params = params,
                        None => {
                            if let Some(runtime) = &self.tsgo {
                                let _ = runtime.forward(
                                    Response::new_ok(request.id, serde_json::Value::Null).into(),
                                );
                            }
                            return;
                        }
                    }
                }
                if let Some(runtime) = &self.tsgo {
                    let mapper = TsgoResponseMapper::for_overlays_request(
                        &runtime.overlays,
                        &request.params,
                    );
                    if !mapper.map_child_params(&request.method, &mut request.params) {
                        let _ = runtime.forward(
                            Response::new_err(
                                request.id,
                                ErrorCode::InvalidParams as i32,
                                "could not map generated request coordinates".to_string(),
                            )
                            .into(),
                        );
                        return;
                    }
                }
                let child_id = request.id.clone();
                self.next_request_id += 1;
                let editor_id =
                    RequestId::from(format!("rsvelte-tsgo-child-{}", self.next_request_id));
                request.id = editor_id.clone();
                self.outgoing.insert(editor_id, Outgoing::Tsgo { child_id });
                self.send(request);
            }
        }
    }

    /// Register or unregister, with the editor, tsgo watchers this server
    /// holds for tsgo ([`TsgoWatchers`]); the editor's answer goes nowhere.
    fn send_watcher_change(&mut self, method: &str, params: serde_json::Value) {
        if !self.client.dynamic_watched_files {
            return;
        }
        self.next_request_id += 1;
        let id = RequestId::from(format!("rsvelte-tsgo-watchers-{}", self.next_request_id));
        self.outgoing
            .insert(id.clone(), Outgoing::WatchedFilesRegistration);
        self.send(Request::new(id, method.to_string(), params));
    }

    fn start_component_query(
        &mut self,
        editor_id: RequestId,
        source_uri: &Uri,
        site: ComponentCompletionSite,
        result: serde_json::Value,
    ) -> bool {
        let Some(runtime) = self.tsgo.as_ref() else {
            return false;
        };
        let source_path = uri_to_path(source_uri.as_str());
        // The project that owns the document answers about it, as every
        // other request is answered, not a copy another project keeps.
        let Some((shadow, generated_range)) =
            runtime
                .overlay_for_source(&source_path)
                .and_then(|overlay| {
                    let shadow = overlay.shadow_for_source(&source_path)?;
                    let ranges = generated_component_ranges(
                        overlay.projection_map(&source_path)?,
                        &site,
                        &shadow.text,
                    );
                    Some((shadow.clone(), ranges.into_iter().next_back()?))
                })
        else {
            return false;
        };
        // The probe has the shadow's imports, so in the shadow's own buffer
        // it is a content change; a new document beside it would be a new
        // root file for tsgo, which rebuilds the program to open it and again
        // to close it. A JavaScript shadow keeps the separate TypeScript
        // probe document, since the probe is TypeScript.
        let in_place = shadow.language_id == "typescriptreact"
            && runtime
                .open_shadows
                .contains_key(shadow.shadow_uri.as_str());
        let query = if in_place {
            ComponentInfoQuery::in_place(
                &shadow.shadow_uri,
                shadow.text,
                generated_range,
                site.component_expression(),
                shadow.version,
            )
        } else {
            ComponentInfoQuery::new(
                &shadow.shadow_uri,
                shadow.text,
                generated_range,
                site.component_expression(),
                shadow.version.saturating_add(10_000),
            )
        };
        let Ok(query) = query else {
            return false;
        };
        self.component_queries.insert(
            editor_id.clone(),
            PendingComponentQuery {
                query,
                site,
                result,
            },
        );
        self.drive_component_query(&editor_id);
        true
    }

    fn on_inlay_hint(&mut self, request: Request) {
        let forward = request.clone();
        let enabled = request
            .params
            .get("textDocument")
            .and_then(|document| document.get("uri"))
            .and_then(serde_json::Value::as_str)
            .and_then(|uri| uri.parse::<Uri>().ok())
            .is_none_or(|uri| self.inlay_hints_enabled(&uri));
        if enabled {
            self.forward_tsgo_request(forward);
            return;
        }
        self.respond(Response::new_ok(request.id, serde_json::Value::Null));
    }

    /// `getInlayHints` returns `null` before asking TypeScript for anything when
    /// no category is on (`InlayHintProvider.ts:38-43`), and the config it reads
    /// is the one for the document's own script kind, not the shadow's extension
    /// (`LSAndTSDocResolver.ts:339-342`). `ls-config.ts:515-522` maps each
    /// category with no fallback, so a client that sends no `inlayHints` has
    /// every one of them undefined and gets `null`.
    fn inlay_hints_enabled(&self, uri: &Uri) -> bool {
        let Some(document) = self.documents.get(uri) else {
            return true;
        };
        if !is_svelte_document(&document.language_id, &uri_to_path(uri.as_str())) {
            return true;
        }
        let language = if is_typescript_component(document.text()) {
            &self.typescript_settings
        } else {
            &self.javascript_settings
        };
        inlay_hints_enabled_in([&self.editor_settings, language, &self.js_ts_settings])
    }

    fn component_reference_code_lens_enabled(&self, uri: &Uri) -> bool {
        let Some(document) = self.documents.get(uri) else {
            return false;
        };
        if !is_svelte_document(&document.language_id, &uri_to_path(uri.as_str()))
            || document_has_parser_error(document.text())
        {
            return false;
        }
        let language = if is_typescript_component(document.text()) {
            &self.typescript_settings
        } else {
            &self.javascript_settings
        };
        let mut enabled = true;
        for layer in [&self.editor_settings, language, &self.js_ts_settings] {
            if let Some(value) = layer
                .pointer("/referencesCodeLens/enabled")
                .and_then(serde_json::Value::as_bool)
            {
                enabled = value;
            }
        }
        enabled
    }

    fn on_component_query_response(&mut self, response: Response) -> bool {
        let Some((editor_id, query_id)) = self.component_query_requests.remove(&response.id) else {
            return false;
        };
        let Some(mut pending) = self.component_queries.remove(&editor_id) else {
            return true;
        };
        match response.response_result {
            Ok(result) => {
                let _ = pending.query.accept_response(query_id, result);
            }
            Err(_) => {
                let _ = pending.query.accept_error(query_id);
            }
        }
        self.component_queries.insert(editor_id.clone(), pending);
        self.drive_component_query(&editor_id);
        true
    }

    fn drive_component_query(&mut self, editor_id: &RequestId) {
        let Some(mut pending) = self.component_queries.remove(editor_id) else {
            return;
        };
        loop {
            let Some(action) = pending.query.next_action() else {
                self.component_queries.insert(editor_id.clone(), pending);
                return;
            };
            match action {
                action @ (ComponentInfoAction::Open { .. }
                | ComponentInfoAction::Change { .. }
                | ComponentInfoAction::Close { .. }) => self.run_component_action(action),
                ComponentInfoAction::Request { id, method, params } => {
                    self.next_request_id += 1;
                    let child_id = RequestId::from(format!(
                        "rsvelte-tsgo-component-info-{}",
                        self.next_request_id
                    ));
                    let request = Request::new(child_id.clone(), method.to_string(), params);
                    let sent = self
                        .tsgo
                        .as_ref()
                        .is_some_and(|runtime| runtime.forward(request.into()).is_ok());
                    if sent {
                        self.component_query_requests
                            .insert(child_id, (editor_id.clone(), id));
                        self.component_queries.insert(editor_id.clone(), pending);
                        return;
                    }
                    let _ = pending.query.accept_error(id);
                }
                ComponentInfoAction::Complete(info) => {
                    let manual = info.completion_items(&pending.site);
                    append_component_completions(
                        &mut pending.result,
                        manual,
                        pending.site.was_colon_triggered(),
                    );
                    self.respond(Response::new_ok(editor_id.clone(), pending.result));
                    self.replay_deferred();
                    return;
                }
            }
        }
    }

    /// Hand tsgo a probe document operation of a component query.
    fn run_component_action(&self, action: ComponentInfoAction) {
        let Some(runtime) = &self.tsgo else {
            return;
        };
        match action {
            ComponentInfoAction::Open {
                uri,
                language_id,
                version,
                text,
            } => {
                let _ =
                    runtime
                        .client
                        .open_buffer(OpenBuffer::new(uri, language_id, version, text));
            }
            ComponentInfoAction::Change { uri, version, text } => {
                let _ = runtime.client.change_buffer(OpenBuffer::new(
                    uri,
                    "typescriptreact",
                    version,
                    text,
                ));
            }
            ComponentInfoAction::Close { uri } => {
                let _ = runtime.client.close_buffer(uri);
            }
            ComponentInfoAction::Request { .. } | ComponentInfoAction::Complete(_) => {}
        }
    }

    /// The shadow of `path` a component query is probing in, if any.
    fn probed_shadow_of(&self, path: &Path) -> Option<Uri> {
        if !self.probe_in_flight() {
            return None;
        }
        self.tsgo
            .as_ref()?
            .owner_shadow(path)
            .map(|shadow| shadow.shadow_uri.clone())
    }

    /// Whether a component query is probing in a shadow's own buffer, which
    /// no other request may read until the probe is swapped back out.
    fn probe_in_flight(&self) -> bool {
        self.component_queries
            .values()
            .any(|pending| pending.query.is_in_place())
    }

    /// The editor changed or closed a document whose shadow holds a probe:
    /// the change replaces the probe, so the query ends, answered with what
    /// tsgo offered without the component's own members.
    fn end_probe_in(&mut self, shadow_uri: &Uri) {
        self.end_probes_where(|query| query.query_uri() == shadow_uri);
    }

    /// End every in-place probe — before a document changes, so the requests
    /// held while one was in are replayed against the state they were sent
    /// for (the editor applies an answer's edits to the version it asked
    /// about). A replayed completion may start a probe of its own, which
    /// would hold the rest again: that one ends too, until none is left.
    fn end_probes(&mut self) {
        while self.probe_in_flight() {
            self.end_probes_where(|_| true);
        }
    }

    /// Let the requests that arrived before a document change send their
    /// tsgo half before the change applies. Most requests take the native
    /// worker's answer first and ask tsgo only when it has none (a hover in
    /// the script), and a tsgo half sent after the change is answered for the
    /// next version: a hover sent right before a line was inserted above it
    /// described the symbol on the line below. The worker answers in about a
    /// millisecond; this waits at most [`NATIVE_HALF_WAIT`].
    fn finish_native_halves(&mut self) {
        let waiting = self
            .pending
            .iter()
            .filter(|(_, pending)| pending.asks_tsgo_next())
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        if waiting.is_empty() {
            return;
        }
        let deadline = Instant::now() + NATIVE_HALF_WAIT;
        while waiting.iter().any(|id| self.pending.contains_key(id)) {
            let Ok(outcome) = self.outcomes.recv_deadline(deadline) else {
                break;
            };
            self.on_outcome(outcome);
        }
    }

    /// End the in-place probes `ends` picks: put each shadow's own text and
    /// version back in tsgo, drop its outstanding tsgo requests, answer it
    /// with what tsgo offered without the component's own members, then hand
    /// on the held requests.
    fn end_probes_where(&mut self, ends: impl Fn(&ComponentInfoQuery) -> bool) {
        let probing = self
            .component_queries
            .iter()
            .filter(|(_, pending)| pending.query.is_in_place() && ends(&pending.query))
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        if probing.is_empty() {
            return;
        }
        for id in probing {
            if let Some(pending) = self.component_queries.remove(&id) {
                self.run_component_action(pending.query.restore());
                self.cancel_component_requests(&id);
                self.respond(Response::new_ok(id, pending.result));
            }
        }
        self.replay_deferred();
    }

    /// Forget the tsgo requests a component query has outstanding and ask
    /// tsgo to cancel them.
    fn cancel_component_requests(&mut self, editor_id: &RequestId) {
        let child_ids = self
            .component_query_requests
            .iter()
            .filter(|(_, (owner, _))| owner == editor_id)
            .map(|(child_id, _)| child_id.clone())
            .collect::<Vec<_>>();
        for child_id in child_ids {
            self.component_query_requests.remove(&child_id);
            if let Some(runtime) = &self.tsgo {
                runtime.cancel(&child_id);
            }
        }
    }

    /// Hand on the requests held while a probe was in a shadow's buffer.
    fn replay_deferred(&mut self) {
        if self.probe_in_flight() {
            return;
        }
        for deferred in std::mem::take(&mut self.deferred) {
            match deferred {
                Deferred::Request(request) => self.on_request(request),
                Deferred::Forward(request, fallback) => {
                    self.forward_tsgo_request_with_fallback(request, fallback);
                }
                Deferred::PushDiagnostics(uri, version, native) => {
                    self.pull_and_publish_tsgo_diagnostics(uri, version, native);
                }
            }
        }
    }

    fn on_tsgo_rename_response(&mut self, mut response: Response, pending: PendingRename) {
        match pending {
            PendingRename::Prepare(plan) => {
                if let Ok(result) = response.response_result {
                    let rewritten = self
                        .tsgo
                        .as_ref()
                        .and_then(|runtime| {
                            let documents = rename_documents(&runtime.overlays);
                            rewrite_prepare_response(&plan, result, &documents)
                        })
                        .unwrap_or(serde_json::Value::Null);
                    response.response_result = Ok(rewritten);
                }
                self.respond(response);
            }
            PendingRename::Primary { plan, new_name } => {
                let Ok(result) = response.response_result else {
                    self.respond(response);
                    return;
                };
                let Some(runtime) = self.tsgo.as_ref() else {
                    self.respond_nothing(response.id);
                    return;
                };
                let documents = rename_documents(&runtime.overlays);
                let rewritten = rewrite_workspace_edit(&plan, &result, &documents, &new_name, true);
                if rewritten.followups.is_empty() {
                    self.respond(Response::new_ok(response.id, rewritten.edit));
                    return;
                }
                let editor_id = response.id;
                let remaining = rewritten.followups.len();
                let mut requests = Vec::with_capacity(remaining);
                for followup in rewritten.followups {
                    self.next_request_id += 1;
                    let child_id = RequestId::from(format!(
                        "rsvelte-tsgo-rename-followup-{}",
                        self.next_request_id
                    ));
                    let request = Request::new(
                        child_id.clone(),
                        "textDocument/rename".to_string(),
                        serde_json::json!({
                            "textDocument": { "uri": followup.uri },
                            "position": followup.position,
                            "newName": followup.new_name,
                        }),
                    );
                    let child_pending = PendingTsgoRequest {
                        method: "textDocument/rename".to_string(),
                        document: None,
                        fallback_result: None,
                        completion_site: None,
                        rename: Some(PendingRename::Followup {
                            editor_id: editor_id.clone(),
                        }),
                        code_action_diagnostic_codes: Vec::new(),
                        push_diagnostics: None,
                        component_site: None,
                        component_references: None,
                        file_rename: None,
                        code_lens_resolve: None,
                        completion_site_data: None,
                        include_declaration: None,
                        component_imports: Vec::new(),
                    };
                    requests.push((child_id, request, child_pending));
                }
                self.rename_aggregates.insert(
                    editor_id.clone(),
                    RenameAggregate {
                        plan,
                        new_name,
                        edits: vec![rewritten.edit],
                        remaining,
                    },
                );
                for (child_id, request, child_pending) in requests {
                    if let Some(runtime) = &self.tsgo
                        && runtime.forward(request.into()).is_ok()
                    {
                        self.pending_tsgo.insert(child_id, child_pending);
                    } else {
                        self.finish_rename_followup(&editor_id, None);
                    }
                }
            }
            PendingRename::Followup { editor_id } => {
                let edit = response.response_result.ok().and_then(|result| {
                    let aggregate = self.rename_aggregates.get(&editor_id)?;
                    let runtime = self.tsgo.as_ref()?;
                    let documents = rename_documents(&runtime.overlays);
                    Some(
                        rewrite_workspace_edit(
                            &aggregate.plan,
                            &result,
                            &documents,
                            &aggregate.new_name,
                            false,
                        )
                        .edit,
                    )
                });
                self.finish_rename_followup(&editor_id, edit);
            }
        }
    }

    /// Gather the references of a store and of its `$store` value together
    /// ([`store_counterpart_position`]): add tsgo's `locations` (an answer to the
    /// editor's request, or to a follow-up), ask for the references of the
    /// other half of each store declaration among them that no answer covers
    /// yet (in each component that declares it), and once every follow-up is
    /// answered, map the gathered locations as tsgo's answer to the editor's
    /// request (review 6: from `$count`, official lists `const count` and
    /// the three `$count` uses; this server listed the uses only, and from
    /// the declaration only the declaration).
    fn continue_store_references(
        &mut self,
        editor_id: &RequestId,
        locations: Vec<serde_json::Value>,
        followup: bool,
    ) {
        let start_of = |location: &serde_json::Value| {
            let uri = location.get("uri")?.as_str()?;
            let start = location.pointer("/range/start")?;
            let line = u32::try_from(start.get("line")?.as_u64()?).ok()?;
            let character = u32::try_from(start.get("character")?.as_u64()?).ok()?;
            Some((uri.to_string(), line, character))
        };
        let counterparts = self.tsgo.as_ref().map_or_else(Vec::new, |runtime| {
            locations
                .iter()
                .filter_map(|location| {
                    let (uri, line, character) = start_of(location)?;
                    let text = runtime
                        .overlays
                        .iter()
                        .find_map(|overlay| overlay.shadow_text(&uri_to_path(&uri)))?;
                    let position =
                        store_counterpart_position(text, Position::new(line, character))?;
                    Some((uri, position.line, position.character))
                })
                .collect::<Vec<_>>()
        });
        let Some(gathering) = self.store_references.gathering.get_mut(editor_id) else {
            return;
        };
        if followup {
            gathering.remaining = gathering.remaining.saturating_sub(1);
        }
        gathering
            .covered
            .extend(locations.iter().filter_map(start_of));
        gathering.locations.extend(locations);
        let include_declaration = gathering.include_declaration;
        let mut asks = Vec::new();
        for counterpart in counterparts {
            if gathering.covered.insert(counterpart.clone()) {
                asks.push(counterpart);
            }
        }
        for (uri, line, character) in asks {
            self.next_request_id += 1;
            let child_id = RequestId::from(format!(
                "rsvelte-tsgo-store-references-{}",
                self.next_request_id
            ));
            let request = Request::new(
                child_id.clone(),
                "textDocument/references".to_string(),
                serde_json::json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": line, "character": character },
                    "context": { "includeDeclaration": include_declaration },
                }),
            );
            if self
                .tsgo
                .as_ref()
                .is_some_and(|runtime| runtime.forward(request.into()).is_ok())
            {
                self.store_references
                    .followups
                    .insert(child_id, editor_id.clone());
                if let Some(gathering) = self.store_references.gathering.get_mut(editor_id) {
                    gathering.remaining += 1;
                }
            }
        }
        if self
            .store_references
            .gathering
            .get(editor_id)
            .is_none_or(|gathering| gathering.remaining > 0)
        {
            return;
        }
        let Some(gathering) = self.store_references.gathering.remove(editor_id) else {
            return;
        };
        let mut locations = Vec::with_capacity(gathering.locations.len());
        for location in gathering.locations {
            if !locations.contains(&location) {
                locations.push(location);
            }
        }
        self.pending_tsgo
            .insert(editor_id.clone(), gathering.pending);
        self.store_references.gathered.insert(editor_id.clone());
        self.on_tsgo_message(Message::Response(Response::new_ok(
            editor_id.clone(),
            serde_json::Value::Array(locations),
        )));
    }

    /// Hold tsgo's answer to a code-action request whose `Cannot find name`
    /// diagnostics are on component tags, and ask for each tag's completions
    /// (`CodeActionsProvider.getComponentImportQuickFix`, `:1016-1112`):
    /// TypeScript's own import fix cannot find a component, whose default
    /// export carries svelte2tsx's suffix, so the fix is made from the
    /// auto-import completion that imports it.
    fn start_component_imports(
        &mut self,
        editor_id: RequestId,
        pending: PendingTsgoRequest,
        result: serde_json::Value,
    ) {
        let Some(shadow_uri) = pending
            .document
            .as_ref()
            .map(|document| document.shadow_uri().clone())
        else {
            return;
        };
        let sites = pending.component_imports.clone();
        let actions = match result {
            serde_json::Value::Array(actions) => actions,
            _ => Vec::new(),
        };
        self.component_imports.gathering.insert(
            editor_id.clone(),
            ComponentImportsGathering {
                pending,
                actions,
                fixes: Vec::new(),
                shadow_uri: shadow_uri.clone(),
                remaining: 0,
            },
        );
        for site in sites {
            self.send_component_import_followup(
                &editor_id,
                "textDocument/completion",
                serde_json::json!({
                    "textDocument": { "uri": shadow_uri.as_str() },
                    "position": site.position,
                    "context": { "triggerKind": 1 },
                }),
                ComponentImportFollowup::Completion { name: site.name },
            );
        }
        self.finish_component_imports(&editor_id);
    }

    fn send_component_import_followup(
        &mut self,
        editor_id: &RequestId,
        method: &str,
        params: serde_json::Value,
        followup: ComponentImportFollowup,
    ) {
        self.next_request_id += 1;
        let child_id = RequestId::from(format!(
            "rsvelte-tsgo-component-import-{}",
            self.next_request_id
        ));
        let request = Request::new(child_id.clone(), method.to_string(), params);
        if self
            .tsgo
            .as_ref()
            .is_some_and(|runtime| runtime.forward(request.into()).is_ok())
        {
            self.component_imports
                .followups
                .insert(child_id, (editor_id.clone(), followup));
            if let Some(gathering) = self.component_imports.gathering.get_mut(editor_id) {
                gathering.remaining += 1;
            }
        }
    }

    /// A follow-up's answer: resolve the completions that import the
    /// component, or keep the quick fix a resolved one makes.
    fn continue_component_imports(
        &mut self,
        editor_id: &RequestId,
        followup: ComponentImportFollowup,
        result: Option<serde_json::Value>,
    ) {
        let Some(gathering) = self.component_imports.gathering.get_mut(editor_id) else {
            return;
        };
        gathering.remaining = gathering.remaining.saturating_sub(1);
        match followup {
            ComponentImportFollowup::Completion { name } => {
                let candidates = result.as_ref().map_or_else(Vec::new, |result| {
                    component_import_candidates(result, &name)
                });
                for item in candidates {
                    self.send_component_import_followup(
                        editor_id,
                        "completionItem/resolve",
                        item,
                        ComponentImportFollowup::Resolve,
                    );
                }
            }
            ComponentImportFollowup::Resolve => {
                if let Some(action) = result.as_ref().and_then(|resolved| {
                    component_import_action(resolved, gathering.shadow_uri.as_str())
                }) {
                    gathering.fixes.push(action);
                }
            }
        }
        self.finish_component_imports(editor_id);
    }

    /// Once every follow-up is answered, answer the editor's request with the
    /// component imports, mapped as tsgo's answer to it. Upstream offers them
    /// instead of TypeScript's own quick fixes, and both for a "did you mean"
    /// diagnostic (`CodeActionsProvider.ts:676-710`); other kinds stay.
    fn finish_component_imports(&mut self, editor_id: &RequestId) {
        if self
            .component_imports
            .gathering
            .get(editor_id)
            .is_none_or(|gathering| gathering.remaining > 0)
        {
            return;
        }
        let Some(gathering) = self.component_imports.gathering.remove(editor_id) else {
            return;
        };
        let did_you_mean = gathering
            .pending
            .code_action_diagnostic_codes
            .contains(&2552);
        let mut actions = gathering.fixes;
        sort_component_import_actions(&mut actions);
        actions.extend(gathering.actions.into_iter().filter(|action| {
            did_you_mean
                || action.get("kind").and_then(serde_json::Value::as_str) != Some("quickfix")
        }));
        self.pending_tsgo
            .insert(editor_id.clone(), gathering.pending);
        self.component_imports.gathered.insert(editor_id.clone());
        self.on_tsgo_message(Message::Response(Response::new_ok(
            editor_id.clone(),
            serde_json::Value::Array(actions),
        )));
    }

    fn finish_rename_followup(&mut self, editor_id: &RequestId, edit: Option<serde_json::Value>) {
        let Some(aggregate) = self.rename_aggregates.get_mut(editor_id) else {
            return;
        };
        if let Some(edit) = edit {
            aggregate.edits.push(edit);
        }
        aggregate.remaining = aggregate.remaining.saturating_sub(1);
        if aggregate.remaining != 0 {
            return;
        }
        let aggregate = self
            .rename_aggregates
            .remove(editor_id)
            .expect("aggregate existed above");
        self.respond(Response::new_ok(
            editor_id.clone(),
            merge_workspace_edits(aggregate.edits),
        ));
    }

    /// Drop the locations of a references, definition or highlight answer
    /// that land in a component's `<script …>` opening tag
    /// ([`drop_locations_in_script_tags`]).
    fn drop_locations_in_script_tags(
        &self,
        result: &mut serde_json::Value,
        request_document: Option<&Uri>,
        definitions: bool,
    ) {
        drop_locations_in_script_tags(result, request_document, definitions, |uri| {
            self.component_text(uri)
        });
    }

    /// A Svelte component's text, for checking where an answer points into
    /// it: the editor's buffer, else the copy a project holds, else the file.
    fn component_text(&self, uri: &Uri) -> Option<std::borrow::Cow<'_, str>> {
        let path = uri_to_path(uri.as_str());
        if path
            .extension()
            .is_none_or(|extension| extension != "svelte")
        {
            return None;
        }
        if let Some(document) = self.documents.get(uri) {
            return Some(std::borrow::Cow::Borrowed(document.text()));
        }
        if let Some(text) = self.tsgo.as_ref().and_then(|runtime| {
            runtime
                .overlays
                .iter()
                .find_map(|overlay| overlay.source_text(&path))
        }) {
            return Some(std::borrow::Cow::Borrowed(text));
        }
        fs::read_to_string(&path).ok().map(std::borrow::Cow::Owned)
    }

    fn on_outcome(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::Formatted { id, edits } => {
                // A request that is no longer pending was cancelled or already
                // answered; its result is simply dropped.
                if self.pending.remove(&id).is_some() {
                    self.respond(Response::new_ok(id, edits));
                }
            }
            Outcome::Compiled { id, result } => {
                if matches!(self.pending.remove(&id), Some(Pending::CompiledCode)) {
                    self.respond(Response::new_ok(id, result));
                }
            }
            Outcome::Completed { id, list } => {
                if let Some(Pending::Completion {
                    tsgo_fallback,
                    filter,
                }) = self.pending.remove(&id)
                {
                    let mut fallback = list.and_then(|list| serde_json::to_value(list).ok());
                    // `PluginHost.ts:285-296`: an incomplete list — Emmet's —
                    // keeps only the items whose label holds the word at the
                    // cursor, since not every editor filters client-side.
                    if let Some(result) = fallback.as_mut()
                        && result["isIncomplete"] == true
                    {
                        mark_completion_list_incomplete(result, filter.as_deref());
                    }
                    self.forward_tsgo_request_with_fallback(tsgo_fallback, fallback);
                }
            }
            Outcome::Hovered { id, hover } => {
                if let Some(Pending::Hover { tsgo_fallback }) = self.pending.remove(&id) {
                    if hover.is_some() {
                        self.respond(Response::new_ok(id, hover));
                    } else {
                        self.forward_tsgo_request(tsgo_fallback);
                    }
                }
            }
            Outcome::CodeActions { id, actions } => {
                self.join_native(id, serde_json::to_value(actions).ok());
            }
            Outcome::CodeLenses { id, lenses } => {
                self.join_native(id, serde_json::to_value(lenses).ok());
            }
            Outcome::ExtractedComponent { id, result } => {
                if !matches!(self.pending.remove(&id), Some(Pending::ExtractComponent)) {
                    return;
                }
                if let Some(message) = result.as_str() {
                    self.send(Notification::new(
                        "window/showMessage".to_string(),
                        serde_json::json!({ "type": 1, "message": message }),
                    ));
                    self.respond(Response::new_ok(id, serde_json::Value::Null));
                } else if self.client.apply_edit {
                    self.next_request_id += 1;
                    let apply_id =
                        RequestId::from(format!("rsvelte-apply-edit-{}", self.next_request_id));
                    self.outgoing
                        .insert(apply_id.clone(), Outgoing::ApplyEdit { command_id: id });
                    self.send(Request::new(
                        apply_id,
                        "workspace/applyEdit".to_string(),
                        serde_json::json!({ "edit": result }),
                    ));
                } else {
                    self.respond(Response::new_ok(id, serde_json::Value::Null));
                }
            }
            Outcome::FoldingRanges { id, ranges } => {
                self.join_native(id, serde_json::to_value(ranges).ok());
            }
            Outcome::SelectionRanges { id, ranges } => {
                if self.pending.remove(&id).is_some() {
                    self.respond(Response::new_ok(id, ranges));
                }
            }
            Outcome::DocumentSymbols { id, symbols } => {
                self.join_native(id, serde_json::to_value(symbols).ok());
            }
            Outcome::PulledDiagnostics {
                id,
                path,
                mut diagnostics,
            } => {
                if !self.joins.contains_key(&id) {
                    return;
                }
                if let Some((_, message)) = {
                    let path = crate::uri::canonicalize(&path).unwrap_or(path);
                    self.preprocess_failures.get(&path)
                } {
                    diagnostics.push(crate::diagnostics::preprocess_failure(message));
                }
                self.join_native(
                    id,
                    serde_json::to_value(diagnostic_report(diagnostics)).ok(),
                );
            }
            Outcome::FileReferences { id, locations } => {
                if matches!(self.pending.remove(&id), Some(Pending::FileReferences)) {
                    self.respond(Response::new_ok(id, locations));
                }
            }
            Outcome::Diagnostics {
                key,
                uri,
                version,
                mut diagnostics,
            } => {
                if self.documents.get_by_key(&key).is_some() {
                    if let Some((failed_version, message)) = {
                        let path = uri_to_path(uri.as_str());
                        let path = crate::uri::canonicalize(&path).unwrap_or(path);
                        self.preprocess_failures.get(&path)
                    } && *failed_version == version
                    {
                        diagnostics.push(crate::diagnostics::preprocess_failure(message));
                    }
                    self.pull_and_publish_tsgo_diagnostics(uri, version, diagnostics);
                }
            }
        }
    }

    fn request_configuration(&mut self) {
        self.next_request_id += 1;
        let id = RequestId::from(format!("rsvelte-configuration-{}", self.next_request_id));
        self.outgoing.insert(id.clone(), Outgoing::Configuration);
        self.send(Request::new(
            id,
            "workspace/configuration".to_string(),
            ConfigurationParams {
                items: [
                    CONFIG_SECTION,
                    SVELTE_CONFIG_SECTION,
                    JS_TS_CONFIG_SECTION,
                    TYPESCRIPT_CONFIG_SECTION,
                    JAVASCRIPT_CONFIG_SECTION,
                    EDITOR_CONFIG_SECTION,
                    EMMET_CONFIG_SECTION,
                ]
                .into_iter()
                .map(|section| ConfigurationItem {
                    scope_uri: None,
                    section: Some(section.to_string()),
                })
                .collect(),
            },
        ));
    }

    fn register_watched_files(&mut self) {
        self.next_request_id += 1;
        let id = RequestId::from(format!("rsvelte-watch-files-{}", self.next_request_id));
        self.outgoing
            .insert(id.clone(), Outgoing::WatchedFilesRegistration);
        self.send(Request::new(
            id,
            "client/registerCapability".to_string(),
            serde_json::json!({
                "registrations": [{
                    "id": "rsvelte-project-configs",
                    "method": "workspace/didChangeWatchedFiles",
                    "registerOptions": {
                        "watchers": project_config_names().iter().map(|name| serde_json::json!({
                            "globPattern": format!("**/{name}"),
                        })).collect::<Vec<_>>(),
                    },
                }],
            }),
        ));
    }

    fn schedule_lint(&mut self, key: String, delay: Duration) {
        self.scheduled.insert(key, Instant::now() + delay);
    }

    fn refresh_document_diagnostics(&mut self, key: String) {
        self.linted.remove(&key);
        if !self.client.pull_diagnostics {
            self.schedule_lint(key, Duration::ZERO);
            return;
        }
        self.request_diagnostic_refresh();
    }

    /// Have the editor pull every document's diagnostics again, or push
    /// them again to an editor that does not pull.
    fn refresh_all_diagnostics(&mut self) {
        if self.client.pull_diagnostics {
            self.request_diagnostic_refresh();
        } else {
            self.relint_open_documents();
        }
    }

    fn request_diagnostic_refresh(&mut self) {
        if !self.client.diagnostic_refresh
            || self
                .outgoing
                .values()
                .any(|outgoing| matches!(outgoing, Outgoing::DiagnosticRefresh))
        {
            return;
        }
        self.next_request_id += 1;
        let id = RequestId::from(format!(
            "rsvelte-diagnostic-refresh-{}",
            self.next_request_id
        ));
        self.outgoing
            .insert(id.clone(), Outgoing::DiagnosticRefresh);
        self.send(Request::new(
            id,
            "workspace/diagnostic/refresh".to_string(),
            serde_json::Value::Null,
        ));
    }

    /// Re-lint everything after the settings changed — the results may differ
    /// even though no document did, so the content-hash guard is reset.
    fn relint_open_documents(&mut self) {
        self.linted.clear();
        let keys: Vec<String> = self
            .documents
            .iter()
            .map(|d| d.uri.as_str().to_string())
            .collect();
        for key in keys {
            self.schedule_lint(key, Duration::ZERO);
        }
    }

    fn run_scheduled_lints(&mut self) {
        let now = Instant::now();
        let due: Vec<String> = self
            .scheduled
            .iter()
            .filter(|&(_, &deadline)| deadline <= now)
            .map(|(key, _)| key.clone())
            .collect();
        for key in due {
            self.scheduled.remove(&key);
            self.lint(&key);
        }
    }

    fn lint(&mut self, key: &str) {
        let Some(document) = self.documents.get_by_key(key) else {
            return;
        };
        let uri = document.uri.clone();
        let language_id = document.language_id.clone();
        let version = document.version;
        let hash = document.content_hash();
        // A burst of edits that cancel out leaves the text — and therefore the
        // diagnostics already on screen — unchanged.
        if self.linted.get(key) == Some(&hash) {
            return;
        }
        self.linted.insert(key.to_string(), hash);

        if !self.settings.lint_enable || !is_lint_target(document) {
            let path = uri_to_path(key);
            if is_svelte_document(&language_id, &path)
                || is_typescript_or_javascript(&language_id, &path)
            {
                self.pull_and_publish_tsgo_diagnostics(uri, version, Vec::new());
            } else {
                self.publish(uri, version, Vec::new());
            }
            return;
        }
        if self.awaiting_preprocess(&uri_to_path(key), version) {
            // The sidecar's answer lints it again.
            self.linted.remove(key);
            return;
        }
        self.worker.submit(Job::Lint {
            key: key.to_string(),
            uri,
            version,
            path: uri_to_path(key),
            text: document.shared_text(),
            preprocessed: self.preprocessed_analysis(&uri_to_path(key), version),
            warnings: self.settings.compiler_warnings.clone(),
            svelte_diagnostics: self.settings.svelte.enable && self.settings.svelte.diagnostics,
            css_diagnostics: self.settings.css.enable && self.settings.css.diagnostics,
        });
    }

    fn preprocessed_analysis(&self, path: &Path, version: i32) -> Option<PreprocessedAnalysis> {
        let path = crate::uri::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let state = self.preprocess_documents.get(&path)?;
        if state.version != version {
            return None;
        }
        Some(PreprocessedAnalysis {
            text: Arc::clone(&state.text),
            map: state.map.as_ref().map(Arc::clone),
            identity: state.identity,
        })
    }

    fn publish(&self, uri: Uri, version: i32, diagnostics: Vec<lsp_types::Diagnostic>) {
        self.send(Notification::new(
            "textDocument/publishDiagnostics".to_string(),
            PublishDiagnosticsParams {
                uri,
                diagnostics,
                version: Some(version),
            },
        ));
    }

    fn respond_diagnostic_report(
        &mut self,
        id: RequestId,
        diagnostics: Vec<lsp_types::Diagnostic>,
    ) {
        self.respond(Response::new_ok(id, diagnostic_report(diagnostics)));
    }

    fn respond_no_edits(&mut self, id: RequestId) {
        self.respond(Response::new_ok(id, Vec::<TextEdit>::new()));
    }

    /// A request with nothing to offer is answered with `null`, not an error.
    fn respond_nothing(&mut self, id: RequestId) {
        self.respond(Response::new_ok(id, serde_json::Value::Null));
    }

    fn respond_no_actions(&mut self, id: RequestId) {
        self.respond(Response::new_ok(id, Vec::<CodeActionOrCommand>::new()));
    }

    fn respond_no_lenses(&mut self, id: RequestId) {
        self.respond(Response::new_ok(id, Vec::<CodeLens>::new()));
    }

    fn respond_no_ranges(&mut self, id: RequestId) {
        self.respond(Response::new_ok(id, Vec::<FoldingRange>::new()));
    }

    fn respond_no_symbols(&mut self, id: RequestId) {
        self.respond(Response::new_ok(
            id,
            DocumentSymbolResponse::Nested(Vec::new()),
        ));
    }

    /// Answer the editor. tsgo's answer to a request whose native half is
    /// still being computed waits for it ([`Join`]).
    fn respond(&mut self, response: Response) {
        let response = match self.joins.get_mut(&response.id) {
            None => response,
            Some(join) => match join.first.take() {
                None | Some(JoinHalf::Tsgo(_)) => {
                    join.first = Some(JoinHalf::Tsgo(response));
                    return;
                }
                Some(JoinHalf::Native(native)) => {
                    let method = join.method;
                    self.joins.remove(&response.id);
                    joined(method, response, native)
                }
            },
        };
        self.send(response);
    }

    /// Send a joined request's tsgo half, as the editor sent it, while the
    /// worker computes its native half ([`Join`]).
    fn join(&mut self, id: RequestId, request: Request) {
        let method = match request.method.as_str() {
            "textDocument/diagnostic" => "textDocument/diagnostic",
            "textDocument/codeAction" => "textDocument/codeAction",
            "textDocument/codeLens" => "textDocument/codeLens",
            "textDocument/foldingRange" => "textDocument/foldingRange",
            "textDocument/documentSymbol" => "textDocument/documentSymbol",
            other => unreachable!("{other} is not a joined request"),
        };
        self.joins.insert(id, Join::new(method));
        self.forward_tsgo_request(request);
    }

    /// The worker's half of a joined request: the answer goes out if tsgo's
    /// is in, and otherwise waits for it.
    fn join_native(&mut self, id: RequestId, native: Option<serde_json::Value>) {
        let Some(join) = self.joins.get_mut(&id) else {
            // Cancelled.
            return;
        };
        match join.first.take() {
            None | Some(JoinHalf::Native(_)) => join.first = Some(JoinHalf::Native(native)),
            Some(JoinHalf::Tsgo(response)) => {
                let method = join.method;
                self.joins.remove(&id);
                self.send(joined(method, response, native));
            }
        }
    }

    fn send(&self, message: impl Into<Message>) {
        let _ = self.sender.send(message.into());
    }
}

fn diagnostic_report(diagnostics: Vec<lsp_types::Diagnostic>) -> DocumentDiagnosticReport {
    DocumentDiagnosticReport::Full(RelatedFullDocumentDiagnosticReport {
        related_documents: None,
        full_document_diagnostic_report: FullDocumentDiagnosticReport {
            result_id: None,
            items: diagnostics,
        },
    })
}

fn inside_element_body(text: &str, offset: usize, tag: &str) -> bool {
    let before = text.get(..offset).unwrap_or(text);
    let open = before.rfind(&format!("<{tag}"));
    let close = before.rfind(&format!("</{tag}"));
    open > close && open.is_some_and(|start| before[start..].contains('>'))
}

/// Keep the locations of a references, definition or highlight answer out of
/// a component's `<script …>` opening tag ([`ScriptOpeningTags`]): no
/// author's text is there, only the generated code svelte2tsx wrote over it,
/// whose unmapped copies (the props type the render header hoists, review 5)
/// map to the tag. A reference or highlight there is dropped. A definition
/// there is right about its file (the component's own declaration is that
/// header: go to definition on a component's tag lands there, e2e 4c), so it
/// is moved to the start of the file, where upstream puts a definition in
/// generated code. `request_document` is the document a highlight is about;
/// `text_of` reads a component's text (`None` for any other document).
fn drop_locations_in_script_tags<'t>(
    result: &mut serde_json::Value,
    request_document: Option<&Uri>,
    definitions: bool,
    text_of: impl Fn(&Uri) -> Option<std::borrow::Cow<'t, str>>,
) {
    let single = result.is_object();
    let items = if single {
        std::slice::from_ref(&*result)
    } else if let Some(items) = result.as_array() {
        items.as_slice()
    } else {
        return;
    };
    let mut texts = HashMap::new();
    for (uri, _) in items.iter().filter_map(location_target) {
        let Some(uri) = uri
            .and_then(|uri| uri.parse::<Uri>().ok())
            .or_else(|| request_document.cloned())
        else {
            continue;
        };
        if !texts.contains_key(uri.as_str()) {
            let text = text_of(&uri);
            texts.insert(uri.as_str().to_string(), text);
        }
    }
    let documents = texts
        .iter()
        .filter_map(|(uri, text)| {
            let text = text.as_deref()?;
            Some((
                uri.as_str(),
                (text, LineIndex::new(text), ScriptOpeningTags::new(text)),
            ))
        })
        .collect::<HashMap<_, _>>();
    if documents.is_empty() {
        return;
    }
    let request_document = request_document.map(|uri| uri.as_str());
    let in_tag = |item: &serde_json::Value| {
        let Some((uri, start)) = location_target(item) else {
            return false;
        };
        uri.or(request_document)
            .and_then(|uri| documents.get(uri))
            .is_some_and(|(text, index, tags)| tags.covers(index.offset(text, start)))
    };
    let start_of_file = serde_json::json!({
        "start": { "line": 0, "character": 0 },
        "end": { "line": 0, "character": 0 },
    });
    let to_start_of_file = |item: &mut serde_json::Value| {
        for field in ["range", "targetRange", "targetSelectionRange"] {
            if let Some(range) = item.get_mut(field) {
                *range = start_of_file.clone();
            }
        }
    };
    if single {
        if in_tag(result) {
            if definitions {
                to_start_of_file(result);
            } else {
                *result = serde_json::Value::Array(Vec::new());
            }
        }
    } else if let Some(items) = result.as_array_mut() {
        if definitions {
            for item in items.iter_mut() {
                if in_tag(item) {
                    to_start_of_file(item);
                }
            }
        } else {
            items.retain(|item| !in_tag(item));
        }
    }
    if definitions {
        dedupe_array(result);
    }
}

/// Where a location in a tsgo answer points: its document (`None` for a
/// document highlight, which is about the request's own document) and the
/// start of its range.
fn location_target(item: &serde_json::Value) -> Option<(Option<&str>, Position)> {
    let (uri, range) = match item.get("targetUri") {
        Some(uri) => (
            uri.as_str(),
            item.get("targetSelectionRange")
                .or_else(|| item.get("targetRange"))?,
        ),
        None => (
            item.get("uri").and_then(serde_json::Value::as_str),
            item.get("range")?,
        ),
    };
    let start = serde_json::from_value::<Position>(range.get("start")?.clone()).ok()?;
    Some((uri, start))
}

/// Drop repeated elements of an array result, keeping the first of each.
fn dedupe_array(result: &mut serde_json::Value) {
    let Some(items) = result.as_array_mut() else {
        return;
    };
    let mut seen = HashSet::new();
    items.retain(|item| seen.insert(item.to_string()));
}

/// The editor's open documents, by canonical path.
fn editor_buffers(documents: &DocumentStore) -> EditorBuffers {
    let mut buffers = EditorBuffers::default();
    for document in documents.iter() {
        let path = uri_to_path(document.uri.as_str());
        buffers.insert(
            crate::uri::canonicalize(&path).unwrap_or(path),
            document.shared_text(),
            document.version,
        );
    }
    buffers
}

fn rename_documents(overlays: &[TsgoOverlay]) -> Vec<RenameDocument<'_>> {
    let mut documents = Vec::new();
    for overlay in overlays {
        for shadow in overlay.eager_shadows() {
            let source_path = uri_to_path(shadow.source_uri.as_str());
            let Some(source_text) = overlay.source_text(&source_path) else {
                continue;
            };
            let Some(projection_map) = overlay.projection_map(&source_path) else {
                continue;
            };
            documents.push(RenameDocument {
                source_uri: &shadow.source_uri,
                shadow_uri: &shadow.shadow_uri,
                source_text,
                generated_text: &shadow.text,
                projection_map,
                source_map: overlay.source_map(&source_path),
                parser_error: false,
            });
        }
    }
    documents
}

fn custom_request_uri(params: &serde_json::Value) -> Option<Uri> {
    params
        .as_str()
        .or_else(|| params.get("uri").and_then(serde_json::Value::as_str))?
        .parse()
        .ok()
}

fn text_document_request_uri(request: &Request) -> Option<Uri> {
    request
        .params
        .pointer("/textDocument/uri")
        .cloned()
        .and_then(|uri| serde_json::from_value(uri).ok())
}

/// Svelte components plus the `.svelte.js` / `.svelte.ts` module dialect.
fn is_lint_target(document: &Document) -> bool {
    if document.language_id == "svelte" {
        return true;
    }
    let uri = document.uri.as_str();
    uri.ends_with(".svelte.js") || uri.ends_with(".svelte.ts")
}

/// `areInlayHintsEnabled` (`InlayHintProvider.ts:92-102`) over the categories
/// `ls-config.ts:515-522` maps, each of which is read with no fallback — so an
/// absent key is off, and `parameterNames` is the one category whose enabled
/// values are strings rather than a boolean.
///
/// Layers are ordered least- to most-specific, matching the sibling rule in
/// `component_reference_code_lens_enabled`.
fn inlay_hints_enabled_in(layers: [&serde_json::Value; 3]) -> bool {
    let resolve = |category: &str| {
        let pointer = format!("/inlayHints/{category}/enabled");
        layers
            .iter()
            .filter_map(|layer| layer.pointer(&pointer))
            .next_back()
    };
    matches!(
        resolve("parameterNames").and_then(serde_json::Value::as_str),
        Some("literals" | "all")
    ) || [
        "enumMemberValues",
        "functionLikeReturnTypes",
        "parameterTypes",
        "propertyDeclarationTypes",
        "variableTypes",
    ]
    .iter()
    .any(|category| resolve(category).and_then(serde_json::Value::as_bool) == Some(true))
}

fn is_svelte_document(language_id: &str, path: &Path) -> bool {
    language_id == "svelte"
        || path
            .extension()
            .is_some_and(|extension| extension == "svelte")
}

fn is_typescript_or_javascript(language_id: &str, path: &Path) -> bool {
    if matches!(
        language_id,
        "typescript" | "typescriptreact" | "javascript" | "javascriptreact"
    ) {
        return true;
    }
    path.extension().is_some_and(|extension| {
        matches!(
            extension.to_str(),
            Some("ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs")
        )
    })
}

fn is_project_config(uri: &Uri) -> bool {
    project_config_names()
        .iter()
        .any(|name| uri.as_str().rsplit('/').next() == Some(name))
}

/// `FoldingRangeProvider.ts:54-56`: a client that folds by line has no use for a
/// range inside one line, and an inverted range is never emitted.
/// TypeScript codes whose diagnostic the API marks `reportsUnnecessary`, read out
/// of `ts.Diagnostics` in the pinned typescript 6.0.3.
const UNNECESSARY_CODES: [i64; 9] = [2695, 6133, 6138, 6192, 6196, 6198, 6199, 7027, 7028];
/// The `reportsDeprecated` half of the same table.
const DEPRECATED_CODES: [i64; 2] = [6385, 6387];

/// tsgo's LSP never writes `tags`: upstream reads `reportsUnnecessary` /
/// `reportsDeprecated` off the TypeScript API instead, and its `getDiagnosticTag`
/// returns `[]` rather than `undefined`, so every diagnostic it emits carries the
/// key. The code decides both flags, so the LSP surface can reproduce them.
/// Upstream rewrites two diagnostics' message text before returning them
/// (`typescript-go/features/DiagnosticsProvider.ts:946-981`). Svelte 5 is the
/// only target here, so the `isSvelte5Plus` arm's `SvelteComponentTyped`
/// paragraph is never appended -- which is why the first suffix ends in a space.
const COMPONENT_CONSTRUCTOR_HINT: &str = "\n\nPossible causes:\n\
    - You use the instance type of a component where you should use the constructor type\n\
    - Type definitions are missing for this Svelte Component. ";

const DECLARE_STATEMENT_HINT: &str =
    "\nIf this is a declare statement, move it into <script context=\"module\">..</script>";

/// `DiagnosticsProvider` names a diagnostic's source after the document's
/// script kind — `ts` for TS/TSX, `js` otherwise — where tsgo always says `ts`.
fn label_javascript_diagnostics(result: &mut serde_json::Value) {
    for item in result
        .get_mut("items")
        .and_then(serde_json::Value::as_array_mut)
        .into_iter()
        .flatten()
    {
        if item.get("source").and_then(serde_json::Value::as_str) == Some("ts") {
            item["source"] = serde_json::Value::String("js".to_string());
        }
    }
}

fn adjust_diagnostic_messages(result: &mut serde_json::Value) {
    fn adjust_items(value: &mut serde_json::Value) {
        let Some(items) = value
            .get_mut("items")
            .and_then(serde_json::Value::as_array_mut)
        else {
            return;
        };
        for item in items {
            let Some(object) = item.as_object_mut() else {
                continue;
            };
            let code = object.get("code").and_then(serde_json::Value::as_i64);
            let Some(message) = object.get("message").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let suffix = match code {
                Some(2345) if message.contains("ConstructorOfATypedSvelteComponent") => {
                    COMPONENT_CONSTRUCTOR_HINT
                }
                Some(1184) => DECLARE_STATEMENT_HINT,
                _ => continue,
            };
            if message.ends_with(suffix) {
                continue;
            }
            let adjusted = format!("{message}{suffix}");
            object.insert("message".to_string(), serde_json::Value::String(adjusted));
        }
    }

    adjust_items(result);
    if let Some(related) = result
        .get_mut("relatedDocuments")
        .and_then(serde_json::Value::as_object_mut)
    {
        for report in related.values_mut() {
            adjust_items(report);
        }
    }
}

/// Drop TypeScript's syntax errors that map into a `<script …>` opening tag.
/// No script text is there: the shadow overwrites the tag with the render
/// function's header (`;type $$ComponentProps = …;function $$render() {`),
/// which maps to the tag as a whole, and TypeScript reports a syntax error in
/// that generated header only when a half-typed statement hoisted above it
/// (`import {`, `type T = {`) left a bracket open and swallowed it. The
/// author's own error is reported where it was typed. A syntax error in a
/// `generics="…"` value, which the header copies as the render function's
/// type parameters, is the author's and stays (review 5: with it dropped, a
/// malformed value showed only a "Cannot find name 'T'" it caused), as do
/// semantic errors: one in the props type the header copies is the author's.
fn drop_syntax_errors_in_script_tags(result: &mut serde_json::Value, text: &str) {
    let Some(items) = result
        .get_mut("items")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return;
    };
    let syntax = |item: &serde_json::Value| {
        item.get("code")
            .and_then(serde_json::Value::as_i64)
            .is_some_and(|code| (1000..2000).contains(&code))
    };
    if !items.iter().any(syntax) {
        return;
    }
    let tags = ScriptOpeningTags::new(text);
    let index = LineIndex::new(text);
    items.retain(|item| {
        let Some(start) = item
            .pointer("/range/start")
            .and_then(|start| serde_json::from_value::<Position>(start.clone()).ok())
        else {
            return true;
        };
        !(syntax(item) && tags.covers(index.offset(text, start)))
    });
}

fn fill_diagnostic_tags(result: &mut serde_json::Value) {
    fn tag_items(value: &mut serde_json::Value) {
        let Some(items) = value
            .get_mut("items")
            .and_then(serde_json::Value::as_array_mut)
        else {
            return;
        };
        for item in items {
            let Some(object) = item.as_object_mut() else {
                continue;
            };
            if object.contains_key("tags") {
                continue;
            }
            let code = object.get("code").and_then(serde_json::Value::as_i64);
            let mut tags = Vec::new();
            if let Some(code) = code {
                if UNNECESSARY_CODES.contains(&code) {
                    tags.push(serde_json::json!(1));
                }
                if DEPRECATED_CODES.contains(&code) {
                    tags.push(serde_json::json!(2));
                }
            }
            object.insert("tags".to_string(), serde_json::Value::Array(tags));
        }
    }

    tag_items(result);
    if let Some(related) = result
        .get_mut("relatedDocuments")
        .and_then(serde_json::Value::as_object_mut)
    {
        for report in related.values_mut() {
            tag_items(report);
        }
    }
}

fn drop_degenerate_folding_ranges(result: &mut serde_json::Value, line_folding_only: bool) {
    let Some(ranges) = result.as_array_mut() else {
        return;
    };
    ranges.retain(|range| {
        let line = |key| range.get(key).and_then(serde_json::Value::as_u64);
        match (line("startLine"), line("endLine")) {
            (Some(start), Some(end)) if line_folding_only => start < end,
            (Some(start), Some(end)) => start <= end,
            _ => true,
        }
    });
}

/// `PluginHost.ts:287-293`: the word the cursor sits in, looking back at most
/// 20 UTF-16 units, lowercased. Nobody types an import name longer than that
/// and still expects perfect autocompletion.
fn incomplete_completion_filter(text: &str, position: &serde_json::Value) -> String {
    let Ok(position) = serde_json::from_value::<Position>(position.clone()) else {
        return String::new();
    };
    incomplete_completion_filter_at(text, LineIndex::new(text).offset(text, position))
}

/// [`incomplete_completion_filter`] at a byte offset.
fn incomplete_completion_filter_at(text: &str, offset: usize) -> String {
    let before = text.get(..offset).unwrap_or(text);
    let units = before.encode_utf16().collect::<Vec<_>>();
    let window = String::from_utf16_lossy(&units[units.len().saturating_sub(20)..]);
    let start = window
        .char_indices()
        .filter(|(_, character)| !(character.is_ascii_alphanumeric() || *character == '_'))
        .map(|(index, character)| index + character.len_utf8())
        .next_back()
        .unwrap_or(0);
    window[start..].to_lowercase()
}

/// The project whose shadow a request was mapped to, and that shadow's path.
fn shadow_overlay<'a>(
    overlays: &'a [TsgoOverlay],
    document: &RequestDocumentContext,
) -> Option<(&'a TsgoOverlay, PathBuf)> {
    let source_path = uri_to_path(document.source_uri().as_str());
    let overlay = overlays.iter().find(|overlay| {
        overlay
            .shadow_for_source(&source_path)
            .is_some_and(|shadow| &shadow.shadow_uri == document.shadow_uri())
    })?;
    Some((overlay, uri_to_path(document.shadow_uri().as_str())))
}

/// `isInScript` (`plugins/typescript/utils.ts`): whether the cursor sits in an
/// instance or module `<script>` body.
fn position_is_in_script(text: &str, position: &serde_json::Value) -> bool {
    let Ok(position) = serde_json::from_value::<Position>(position.clone()) else {
        return false;
    };
    let offset = LineIndex::new(text).offset(text, position);
    crate::context::EmbeddedRegions::new(text).in_script(offset)
}

/// `CompletionProvider.ts:828-830`: with `checkCommitCharacters` off every item
/// answers `undefined`, so the field is absent rather than empty.
fn strip_commit_characters(result: &mut serde_json::Value) {
    let items = match result {
        serde_json::Value::Array(items) => items,
        other => {
            let Some(items) = other
                .get_mut("items")
                .and_then(serde_json::Value::as_array_mut)
            else {
                return;
            };
            items
        }
    };
    for item in items {
        if let Some(object) = item.as_object_mut() {
            object.remove("commitCharacters");
        }
    }
}

/// `CompletionProvider.ts:451` and `PluginHost.ts:286-297`: the list a rejected
/// document produces is incomplete, and the server narrows it itself because
/// not every editor filters client-side.
fn mark_completion_list_incomplete(result: &mut serde_json::Value, filter: Option<&str>) {
    if let Some(items) = result.as_array() {
        *result = serde_json::json!({ "isIncomplete": true, "items": items });
    }
    if let Some(object) = result.as_object_mut() {
        object.insert("isIncomplete".to_string(), serde_json::Value::Bool(true));
    }
    let Some(filter) = filter.filter(|filter| !filter.is_empty()) else {
        return;
    };
    if let Some(items) = result
        .get_mut("items")
        .and_then(serde_json::Value::as_array_mut)
    {
        items.retain(|item| {
            item.get("label")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|label| label.to_lowercase().contains(filter))
        });
    }
}

fn merge_tsgo_result(method: &str, result: &mut serde_json::Value, fallback: serde_json::Value) {
    if result.is_null() {
        *result = fallback;
        return;
    }
    match method {
        "textDocument/completion" => {
            let mut fallback = fallback;
            // `PluginHost.ts:278-281` ORs the flag over every contributing
            // plugin; hardcoding it says the list is exhaustive when tsgo has
            // just said it is not.
            let incomplete = [&*result, &fallback].into_iter().any(|list| {
                list.get("isIncomplete")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
            });
            let fallback_items = fallback
                .get_mut("items")
                .and_then(serde_json::Value::as_array_mut);
            let result_items = result
                .get_mut("items")
                .and_then(serde_json::Value::as_array_mut);
            if let (Some(fallback_items), Some(result_items)) = (fallback_items, result_items) {
                fallback_items.append(result_items);
                if let Some(object) = fallback.as_object_mut() {
                    object.insert(
                        "isIncomplete".to_string(),
                        serde_json::Value::Bool(incomplete),
                    );
                }
                *result = fallback;
            }
        }
        "textDocument/codeAction"
        | "textDocument/codeLens"
        | "textDocument/foldingRange"
        | "textDocument/documentSymbol" => {
            if let (Some(result_items), Some(mut fallback_items)) =
                (result.as_array_mut(), fallback.as_array().cloned())
            {
                fallback_items.append(result_items);
                *result = serde_json::Value::Array(fallback_items);
            }
        }
        "textDocument/diagnostic" => {
            let mut fallback = fallback;
            let fallback_items = fallback
                .get_mut("items")
                .and_then(serde_json::Value::as_array_mut);
            let result_items = result
                .get_mut("items")
                .and_then(serde_json::Value::as_array_mut);
            if let (Some(fallback_items), Some(result_items)) = (fallback_items, result_items) {
                fallback_items.append(result_items);
                *result = fallback;
            }
        }
        _ => {}
    }
}

/// The cache key for one completion site.
/// JavaScript's `!completionItem.data` is false.
fn has_resolve_data(item: &serde_json::Value) -> bool {
    match item.get("data") {
        None | Some(serde_json::Value::Null) => false,
        Some(serde_json::Value::Bool(value)) => *value,
        Some(serde_json::Value::Number(number)) => number.as_f64().is_some_and(|n| n != 0.0),
        Some(serde_json::Value::String(text)) => !text.is_empty(),
        Some(_) => true,
    }
}

fn completion_data_key(uri: &str, position: &serde_json::Value) -> String {
    format!("{uri}|{position}")
}

fn completion_result_shape(result: &serde_json::Value) -> (usize, bool) {
    let items = result
        .get("items")
        .and_then(serde_json::Value::as_array)
        .or_else(|| result.as_array());
    let count = items.map_or(0, Vec::len);
    let first_is_member = items
        .and_then(|items| items.first())
        .and_then(|item| item.get("kind"))
        .and_then(serde_json::Value::as_u64)
        .is_some_and(|kind| matches!(kind, 5 | 10));
    (count, first_is_member)
}

fn locations_from_tsgo_result(result: &serde_json::Value) -> Vec<lsp_types::Location> {
    let Some(items) = result.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            serde_json::from_value::<lsp_types::Location>(item.clone())
                .ok()
                .or_else(|| {
                    let uri = item
                        .get("targetUri")
                        .cloned()
                        .and_then(|uri| serde_json::from_value(uri).ok())?;
                    let range = item
                        .get("targetSelectionRange")
                        .or_else(|| item.get("targetRange"))
                        .cloned()
                        .and_then(|range| serde_json::from_value(range).ok())?;
                    Some(lsp_types::Location { uri, range })
                })
        })
        .collect()
}

fn append_component_completions(
    result: &mut serde_json::Value,
    manual: Vec<lsp_types::CompletionItem>,
    replace: bool,
) {
    let manual = manual
        .into_iter()
        .filter_map(|item| serde_json::to_value(item).ok())
        .collect::<Vec<_>>();
    if replace {
        *result = serde_json::json!({ "isIncomplete": false, "items": manual });
        return;
    }
    if let Some(items) = result
        .get_mut("items")
        .and_then(serde_json::Value::as_array_mut)
    {
        items.splice(0..0, manual);
    } else if let Some(items) = result.as_array_mut() {
        items.splice(0..0, manual);
    } else {
        *result = serde_json::json!({ "isIncomplete": false, "items": manual });
    }
}

fn code_action_diagnostic_codes(request: &Request) -> Vec<u32> {
    if request.method != "textDocument/codeAction" {
        return Vec::new();
    }
    request
        .params
        .pointer("/context/diagnostics")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|diagnostic| {
            let code = diagnostic.get("code")?;
            code.as_u64()
                .and_then(|code| u32::try_from(code).ok())
                .or_else(|| code.as_str()?.trim_start_matches("TS").parse::<u32>().ok())
        })
        .collect()
}

const fn project_config_names() -> &'static [&'static str] {
    &[
        "rsvelte-lint.json",
        ".rsvelte-lintrc.json",
        ".oxfmtrc.json",
        ".oxfmtrc.jsonc",
        "oxfmt.config.ts",
        "oxfmt.config.mts",
        "tsconfig.json",
        "jsconfig.json",
        "svelte.config.js",
        "svelte.config.mjs",
        "svelte.config.cjs",
        "svelte.config.ts",
        "svelte.config.mts",
        "vite.config.js",
        "vite.config.mjs",
        "vite.config.cjs",
        "vite.config.ts",
        "vite.config.mts",
    ]
}

/// The completion site of a cursor inside a start tag, if it is in one.
fn start_tag_completion_site(text: &str, offset: usize) -> Option<CompletionSite> {
    // A `<script>` / `<style>` start tag is not an `Element` in the Svelte
    // AST — the block is hoisted to `instance` / `module` / `css` — so
    // upstream's `svelteNode?.type === 'Element'` guard never fires there.
    let element_tag = match crate::context::start_tag_context(text, offset) {
        // An attribute value's `Text` has an `Attribute` parent, which is
        // not in upstream's raw-text bail list, and `svelteNodeAt` answers
        // `Text` rather than the element, so neither guard fires.
        crate::context::StartTag::Attribute(attribute) if attribute.in_value => {
            return Some(CompletionSite::Unguarded);
        }
        crate::context::StartTag::Attribute(attribute) => attribute.element_tag,
        crate::context::StartTag::Bare { element_tag }
        | crate::context::StartTag::TagName { element_tag } => element_tag,
        crate::context::StartTag::None => return None,
    };
    if is_embedded_tag(element_tag) {
        return Some(CompletionSite::Unguarded);
    }
    // The Svelte parser makes a capitalised or dotted tag an `InlineComponent`.
    Some(
        if element_tag.starts_with(|character: char| character.is_ascii_uppercase())
            || element_tag.contains('.')
        {
            CompletionSite::ComponentStartTag {
                // `CompletionProvider.ts:252-283` narrows only in the
                // whitespace after the name; in the name itself (`<MyCom`) it
                // forwards to TypeScript, which completes the component's
                // identifier and offers auto-imports.
                at_whitespace: might_be_at_start_tag_whitespace(text, offset),
            }
        } else {
            CompletionSite::ElementStartTag
        },
    )
}

/// Whether a start tag opens a block the Svelte parser hoists out of the
/// template rather than keeping as an element.
fn is_embedded_tag(tag: &str) -> bool {
    tag.eq_ignore_ascii_case("script") || tag.eq_ignore_ascii_case("style")
}

/// `CompletionProvider.ts:497-501`, which tests `/\s[\s>/]/` against the two
/// characters straddling the cursor — narrower than "nothing typed yet".
fn might_be_at_start_tag_whitespace(text: &str, offset: usize) -> bool {
    let before = text.get(..offset).and_then(|text| text.chars().next_back());
    let at = text.get(offset..).and_then(|text| text.chars().next());
    before.is_some_and(char::is_whitespace)
        && at.is_some_and(|character| character.is_whitespace() || matches!(character, '>' | '/'))
}

#[cfg(test)]
mod start_tag_completion_site_tests {
    use super::{CompletionSite, start_tag_completion_site};

    fn site(text_with_cursor: &str) -> Option<CompletionSite> {
        let offset = text_with_cursor.find('|').expect("cursor");
        let text = text_with_cursor.replacen('|', "", 1);
        start_tag_completion_site(&text, offset)
    }

    /// Typing a component's name is answered by TypeScript, as upstream
    /// does: only the whitespace after the name narrows to props.
    #[test]
    fn a_component_name_being_typed_is_not_narrowed() {
        for typing in ["<MyCom|", "<MyCom|\n", "<p>x</p>\n<Other|", "<ui.But|"] {
            assert_eq!(
                site(typing),
                Some(CompletionSite::ComponentStartTag {
                    at_whitespace: false
                }),
                "{typing:?}"
            );
        }
        for after_name in [
            "<MyComponent | />",
            "<ui.Button | />",
            "<MyComponent | x={1} />",
        ] {
            assert_eq!(
                site(after_name),
                Some(CompletionSite::ComponentStartTag {
                    at_whitespace: true
                }),
                "{after_name:?}"
            );
        }
        assert_eq!(site("<inp|"), Some(CompletionSite::ElementStartTag));
        assert_eq!(
            site("<input value=\"|\" />"),
            Some(CompletionSite::Unguarded)
        );
    }
}

#[cfg(test)]
mod restart_backoff_tests {
    use std::time::{Duration, Instant};

    use super::{RestartBackoff, TSGO_RESTART_QUIET_TIMEOUTS};

    /// Restarts for a state the editor keeps asking about double the next
    /// one's wait, up to eight timeouts; a request sent after eight timeouts
    /// without one waits the plain timeout again (review 5), and other
    /// requests are not affected.
    #[test]
    fn the_restart_backoff_doubles_and_resets_after_a_quiet_period() {
        let timeout = Duration::from_secs(30);
        let key = (
            "textDocument/signatureHelp".to_string(),
            Some("file:///a.ts".to_string()),
        );
        let other = (
            "textDocument/completion".to_string(),
            Some("file:///a.ts".to_string()),
        );
        let start = Instant::now();
        let at = |seconds: u64| start + Duration::from_secs(seconds);
        let mut backoff = RestartBackoff::default();
        assert_eq!(backoff.doublings(&key, at(0), timeout), 0);
        // The editor asks again a second after each restart.
        let (mut sent, mut now) = (0, 30);
        for expected in [1, 2, 3, 3, 3] {
            backoff.restarted(key.clone(), at(sent), at(now), timeout);
            sent = now + 1;
            assert_eq!(backoff.doublings(&key, at(sent), timeout), expected);
            now = sent + 30 * (1 << expected);
        }
        assert_eq!(backoff.doublings(&other, at(sent), timeout), 0);
        // Quiet since the last restart (at `now - 241`): the plain timeout.
        let quiet = 30 * u64::from(TSGO_RESTART_QUIET_TIMEOUTS);
        let last = now - 241;
        assert_eq!(backoff.doublings(&key, at(last + quiet - 1), timeout), 3);
        assert_eq!(backoff.doublings(&key, at(last + quiet), timeout), 0);
        backoff.restarted(
            key.clone(),
            at(last + quiet),
            at(last + quiet + 30),
            timeout,
        );
        assert_eq!(backoff.doublings(&key, at(last + quiet + 31), timeout), 1);
    }
}

#[cfg(test)]
mod script_tag_location_tests {
    use std::borrow::Cow;

    use lsp_types::Uri;
    use serde_json::json;

    use super::drop_locations_in_script_tags;

    const COMPONENT: &str = "<script lang=\"ts\" generics=\"T\">\n\ttype Size = 'sm';\n\tlet s: Size = 'sm';\n</script>\n";

    fn range(line: u32, start: u32, end: u32) -> serde_json::Value {
        json!({
            "start": { "line": line, "character": start },
            "end": { "line": line, "character": end },
        })
    }

    fn filtered(mut result: serde_json::Value, definitions: bool) -> serde_json::Value {
        let request: Uri = "file:///ws/Card.svelte".parse().unwrap();
        drop_locations_in_script_tags(&mut result, Some(&request), definitions, |uri| {
            uri.as_str()
                .ends_with(".svelte")
                .then_some(Cow::Borrowed(COMPONENT))
        });
        result
    }

    fn starts(result: &serde_json::Value, field: &str) -> Vec<String> {
        result
            .as_array()
            .unwrap()
            .iter()
            .map(|item| {
                let start = &item[field]["start"];
                let end = &item[field]["end"];
                format!(
                    "{}:{}-{}:{}",
                    start["line"], start["character"], end["line"], end["character"]
                )
            })
            .collect()
    }

    /// A copy the render header makes of the props type maps to the
    /// `<script …>` tag (0:1, review 5): such a reference or highlight is
    /// dropped, while one in the script, in the `generics` value, at the `<`
    /// or in another kind of file stays.
    #[test]
    fn references_and_highlights_in_a_script_opening_tag_are_dropped() {
        let card = "file:///ws/Card.svelte";
        let references = filtered(
            json!([
                { "uri": card, "range": range(1, 6, 10) },
                { "uri": card, "range": range(0, 1, 1) },
                { "uri": card, "range": range(0, 28, 29) },
                { "uri": card, "range": range(2, 8, 12) },
                { "uri": card, "range": range(0, 0, 0) },
                { "uri": "file:///ws/util.ts", "range": range(0, 1, 5) },
            ]),
            false,
        );
        assert_eq!(
            starts(&references, "range"),
            ["1:6-1:10", "0:28-0:29", "2:8-2:12", "0:0-0:0", "0:1-0:5"]
        );
        let highlights = filtered(
            json!([
                { "range": range(0, 1, 5), "kind": 1 },
                { "range": range(1, 6, 10), "kind": 3 },
            ]),
            false,
        );
        assert_eq!(starts(&highlights, "range"), ["1:6-1:10"]);
        assert_eq!(
            filtered(json!({ "uri": card, "range": range(0, 1, 5) }), false),
            json!([])
        );
    }

    /// A definition in the tag is right about its file: the component's own
    /// declaration is the header (go to definition on a component's tag,
    /// e2e 4c). It moves to the start of the file, once.
    #[test]
    fn definitions_in_a_script_opening_tag_move_to_the_start_of_the_file() {
        let card = "file:///ws/Card.svelte";
        let links = filtered(
            json!([
                { "targetUri": card, "targetRange": range(0, 1, 5), "targetSelectionRange": range(0, 1, 5), "originSelectionRange": range(3, 1, 5) },
                { "targetUri": card, "targetRange": range(0, 1, 1), "targetSelectionRange": range(0, 1, 1), "originSelectionRange": range(3, 1, 5) },
                { "targetUri": card, "targetRange": range(1, 6, 10), "targetSelectionRange": range(1, 6, 10) },
            ]),
            true,
        );
        assert_eq!(
            starts(&links, "targetSelectionRange"),
            ["0:0-0:0", "1:6-1:10"]
        );
        assert_eq!(starts(&links, "targetRange"), ["0:0-0:0", "1:6-1:10"]);
        assert_eq!(links[0]["originSelectionRange"], range(3, 1, 5));
        assert_eq!(
            filtered(json!({ "uri": card, "range": range(0, 1, 5) }), true),
            json!({ "uri": card, "range": range(0, 0, 0) })
        );
    }
}

#[cfg(test)]
mod diagnostic_message_tests {
    use super::{
        COMPONENT_CONSTRUCTOR_HINT, DECLARE_STATEMENT_HINT, adjust_diagnostic_messages,
        drop_syntax_errors_in_script_tags,
    };

    /// `import {` typed above the props leaves the hoisted import open, so
    /// TypeScript reads the render header after it as an import clause and
    /// reports syntax errors there, which map to the `<script>` tag (0:1,
    /// on `script`). Those go; the error where the author typed, and a
    /// semantic error that maps to the tag (an unknown type in the props type
    /// the header copies), stay.
    #[test]
    fn syntax_errors_in_a_script_opening_tag_are_dropped() {
        let text = "<script lang=\"ts\">\n\timport {\n\tlet { a }: { a: Foo } = $props();\n</script>\n<p>{a}</p>\n";
        let item = |code: i64, line: u32, character: u32| {
            serde_json::json!({
                "code": code,
                "message": format!("{code}"),
                "range": {
                    "start": { "line": line, "character": character },
                    "end": { "line": line, "character": character },
                },
            })
        };
        let mut result = serde_json::json!({
            "kind": "full",
            "items": [item(1003, 1, 9), item(1003, 0, 1), item(1005, 0, 1), item(1141, 0, 17), item(2304, 0, 1), item(1005, 4, 3)],
        });
        drop_syntax_errors_in_script_tags(&mut result, text);
        let kept = result["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| {
                format!(
                    "{}@{}:{}",
                    item["code"],
                    item["range"]["start"]["line"],
                    item["range"]["start"]["character"]
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(kept, ["1003@1:9", "2304@0:1", "1005@4:3"]);
    }

    /// The header copies a `generics="…"` value as the render function's type
    /// parameters, so a syntax error there is the author's and stays (review
    /// 5); one elsewhere in the tag still goes.
    #[test]
    fn syntax_errors_in_a_generics_value_are_kept() {
        let text = "<script lang=\"ts\" generics=\"T extends \">\n\tlet { item }: { item: T } = $props();\n</script>\n<p>{String(item)}</p>\n";
        let value = u32::try_from(text.find("T extends").unwrap()).unwrap();
        let end = value + u32::try_from("T extends ".len()).unwrap();
        let item = |code: i64, character: u32| {
            serde_json::json!({
                "code": code,
                "message": format!("{code}"),
                "range": {
                    "start": { "line": 0, "character": character },
                    "end": { "line": 0, "character": character },
                },
            })
        };
        let mut result = serde_json::json!({
            "kind": "full",
            "items": [item(1110, end), item(1005, value), item(1005, 1), item(2304, 1)],
        });
        drop_syntax_errors_in_script_tags(&mut result, text);
        let kept = result["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| format!("{}@{}", item["code"], item["range"]["start"]["character"]))
            .collect::<Vec<_>>();
        assert_eq!(
            kept,
            [
                format!("1110@{end}"),
                format!("1005@{value}"),
                "2304@1".to_string()
            ]
        );
    }

    fn messages(items: serde_json::Value) -> Vec<String> {
        let mut result = serde_json::json!({ "kind": "full", "items": items });
        adjust_diagnostic_messages(&mut result);
        result["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["message"].as_str().unwrap().to_string())
            .collect()
    }

    /// The expected bytes are official's own, read out of the LSP gate artifact
    /// rather than retyped: the line-continuations above must rebuild exactly
    /// this, trailing space included -- it is the `isSvelte5Plus` ternary's empty
    /// arm, and a version that drops it still looks right.
    #[test]
    fn the_constructor_hint_is_byte_identical_to_the_official_server() {
        assert_eq!(
            COMPONENT_CONSTRUCTOR_HINT,
            "\n\nPossible causes:\n- You use the instance type of a component where you should use the constructor type\n- Type definitions are missing for this Svelte Component. "
        );
        assert!(COMPONENT_CONSTRUCTOR_HINT.ends_with(' '));
        assert!(!COMPONENT_CONSTRUCTOR_HINT.contains("SvelteComponentTyped"));
    }

    #[test]
    fn only_a_component_constructor_2345_is_adjusted() {
        let out = messages(serde_json::json!([
            { "code": 2345, "message": "x ConstructorOfATypedSvelteComponent y" },
            { "code": 2345, "message": "an ordinary argument-type error" },
            { "code": 1184, "message": "Modifiers cannot appear here." },
            { "code": 2322, "message": "untouched" },
            { "message": "no code at all" }
        ]));
        assert_eq!(
            out[0],
            format!("x ConstructorOfATypedSvelteComponent y{COMPONENT_CONSTRUCTOR_HINT}")
        );
        assert_eq!(out[1], "an ordinary argument-type error");
        assert_eq!(
            out[2],
            format!("Modifiers cannot appear here.{DECLARE_STATEMENT_HINT}")
        );
        assert_eq!(out[3], "untouched");
        assert_eq!(out[4], "no code at all");
    }

    /// The response passes this once, but a merged fallback or a re-entered
    /// pipeline must not append a second copy.
    #[test]
    fn adjusting_twice_appends_once() {
        let mut result = serde_json::json!({
            "kind": "full",
            "items": [{ "code": 1184, "message": "Modifiers cannot appear here." }]
        });
        adjust_diagnostic_messages(&mut result);
        adjust_diagnostic_messages(&mut result);
        assert_eq!(
            result["items"][0]["message"].as_str().unwrap(),
            format!("Modifiers cannot appear here.{DECLARE_STATEMENT_HINT}")
        );
    }

    #[test]
    fn related_documents_are_adjusted_too() {
        let mut result = serde_json::json!({
            "kind": "full",
            "items": [],
            "relatedDocuments": {
                "file:///a.ts": {
                    "kind": "full",
                    "items": [{ "code": 1184, "message": "Modifiers cannot appear here." }]
                }
            }
        });
        adjust_diagnostic_messages(&mut result);
        assert_eq!(
            result["relatedDocuments"]["file:///a.ts"]["items"][0]["message"]
                .as_str()
                .unwrap(),
            format!("Modifiers cannot appear here.{DECLARE_STATEMENT_HINT}")
        );
    }
}

#[cfg(test)]
mod diagnostic_tag_tests {
    use super::fill_diagnostic_tags;

    fn tagged(items: serde_json::Value) -> Vec<serde_json::Value> {
        let mut result = serde_json::json!({ "kind": "full", "items": items });
        fill_diagnostic_tags(&mut result);
        result["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["tags"].clone())
            .collect()
    }

    #[test]
    fn a_javascript_document_labels_its_type_diagnostics_js() {
        let mut result = serde_json::json!({
            "kind": "full",
            "items": [
                { "source": "ts", "code": 2304, "message": "Cannot find name 'hi'." },
                { "source": "svelte", "code": "a11y", "message": "kept" }
            ]
        });
        super::label_javascript_diagnostics(&mut result);
        assert_eq!(result["items"][0]["source"], "js");
        assert_eq!(result["items"][1]["source"], "svelte");
    }

    /// tsgo sends no `tags` at all, and upstream's `getDiagnosticTag` returns `[]`
    /// rather than `undefined` — so an untagged diagnostic still carries the key.
    #[test]
    fn a_code_decides_the_tag_and_every_diagnostic_carries_the_key() {
        assert_eq!(
            tagged(serde_json::json!([
                { "code": 6133, "message": "unused" },
                { "code": 6387, "message": "deprecated" },
                { "code": 2322, "message": "type error" },
                { "message": "no code at all" }
            ])),
            [
                serde_json::json!([1]),
                serde_json::json!([2]),
                serde_json::json!([]),
                serde_json::json!([])
            ]
        );
    }

    /// A server that does send `tags` keeps them: this fills a gap, it does not
    /// overwrite an answer.
    #[test]
    fn an_existing_tags_field_is_left_alone() {
        assert_eq!(
            tagged(serde_json::json!([{ "code": 6133, "tags": [] }])),
            [serde_json::json!([])]
        );
    }

    /// A related document's report is a second `items` array, and an
    /// inter-file-dependency provider fills it.
    #[test]
    fn related_documents_are_tagged_too() {
        let mut result = serde_json::json!({
            "kind": "full",
            "items": [],
            "relatedDocuments": { "file:///a.ts": { "kind": "full", "items": [{ "code": 6385 }] } }
        });
        fill_diagnostic_tags(&mut result);
        assert_eq!(
            result["relatedDocuments"]["file:///a.ts"]["items"][0]["tags"],
            serde_json::json!([2])
        );
    }
}

#[cfg(test)]
mod folding_tests {
    use super::drop_degenerate_folding_ranges;

    fn kept(line_folding_only: bool) -> Vec<u64> {
        let mut result = serde_json::json!([
            { "startLine": 0, "endLine": 3 },
            { "startLine": 1, "endLine": 1 },
            { "startLine": 5, "endLine": 4 },
            { "startLine": 6 }
        ]);
        drop_degenerate_folding_ranges(&mut result, line_folding_only);
        result
            .as_array()
            .unwrap()
            .iter()
            .map(|range| range["startLine"].as_u64().unwrap())
            .collect()
    }

    /// `FoldingRangeProvider.ts:54-56` keeps a single-line range only when the
    /// client folds by character; an inverted range is dropped either way, and a
    /// range with no `endLine` is not this filter's business.
    #[test]
    fn a_single_line_range_survives_only_a_character_folding_client() {
        assert_eq!(kept(true), [0, 6]);
        assert_eq!(kept(false), [0, 1, 6]);
    }
}

#[cfg(test)]
mod start_tag_tests {
    use super::might_be_at_start_tag_whitespace;

    fn at(text: &str, needle: &str) -> bool {
        might_be_at_start_tag_whitespace(text, text.find(needle).unwrap() + needle.len())
    }

    #[test]
    fn only_whitespace_before_an_empty_slot_counts() {
        assert!(at("<Comp >", "<Comp "));
        assert!(at("<Comp />", "<Comp "));
        assert!(at("<Comp  a>", "<Comp "));
        // A name is being typed, so the slot is not empty.
        assert!(!at("<Comp a>", "<Comp "));
        // Nothing before the cursor is whitespace.
        assert!(!at("<Comp a >", "<Comp a"));
    }
}

#[cfg(test)]
mod merge_tsgo_result_tests {
    use super::merge_tsgo_result;
    use serde_json::json;

    fn merged_is_incomplete(tsgo: bool, ours: bool) -> bool {
        let mut result = json!({ "isIncomplete": tsgo, "items": [{ "label": "a" }] });
        merge_tsgo_result(
            "textDocument/completion",
            &mut result,
            json!({ "isIncomplete": ours, "items": [{ "label": "b" }] }),
        );
        assert_eq!(result["items"].as_array().unwrap().len(), 2);
        result["isIncomplete"].as_bool().unwrap()
    }

    // `PluginHost.ts:278-281` ORs the flag over the contributing plugins, so a
    // constant — in either direction — is wrong on one of these four rows.
    #[test]
    fn is_incomplete_is_ored_over_both_contributors() {
        assert!(!merged_is_incomplete(false, false));
        assert!(merged_is_incomplete(true, false));
        assert!(merged_is_incomplete(false, true));
        assert!(merged_is_incomplete(true, true));
    }
}

#[cfg(test)]
mod inlay_hint_preference_tests {
    use super::inlay_hints_enabled_in;
    use serde_json::{Value, json};

    fn enabled(language: Value) -> bool {
        inlay_hints_enabled_in([&Value::Null, &language, &Value::Null])
    }

    /// Every answer is upstream's, taken by running the two servers rather than
    /// transcribed: with the LSP gate's own configuration -- which gives
    /// `typescript` all six categories and `javascript` none -- official answers
    /// a list for a `lang="ts"` component and `null` for a plain `<script>` one.
    #[test]
    fn an_absent_category_is_off_and_a_present_one_decides() {
        // The gate's `javascript` section: a sibling key, and no `inlayHints`.
        assert!(!enabled(json!({ "suggest": { "autoImports": false } })));
        // The gate's `typescript` section.
        assert!(enabled(json!({
            "inlayHints": {
                "enumMemberValues": { "enabled": true },
                "functionLikeReturnTypes": { "enabled": true },
                "parameterNames": { "enabled": "all", "suppressWhenArgumentMatchesName": false },
                "parameterTypes": { "enabled": true },
                "propertyDeclarationTypes": { "enabled": true },
                "variableTypes": { "enabled": true, "suppressWhenTypeMatchesName": false }
            }
        })));
        assert!(!enabled(json!({ "inlayHints": {} })));
    }

    /// `parameterNames` is a string enum and the other five are booleans, so a
    /// rule that reads all six the same way is wrong in both directions.
    #[test]
    fn parameter_names_is_a_string_enum_and_none_is_off() {
        assert!(enabled(
            json!({ "inlayHints": { "parameterNames": { "enabled": "all" } } })
        ));
        assert!(enabled(
            json!({ "inlayHints": { "parameterNames": { "enabled": "literals" } } })
        ));
        assert!(!enabled(
            json!({ "inlayHints": { "parameterNames": { "enabled": "none" } } })
        ));
        assert!(!enabled(
            json!({ "inlayHints": { "parameterNames": { "enabled": true } } })
        ));
        assert!(enabled(
            json!({ "inlayHints": { "variableTypes": { "enabled": true } } })
        ));
        assert!(!enabled(
            json!({ "inlayHints": { "variableTypes": { "enabled": false } } })
        ));
        assert!(!enabled(
            json!({ "inlayHints": { "variableTypes": { "enabled": "all" } } })
        ));
    }

    /// One enabled category is enough, which is what makes this a disjunction
    /// rather than a check of any single setting.
    #[test]
    fn any_single_category_turns_it_on() {
        for category in [
            "enumMemberValues",
            "functionLikeReturnTypes",
            "parameterTypes",
            "propertyDeclarationTypes",
            "variableTypes",
        ] {
            assert!(
                enabled(json!({ "inlayHints": { category: { "enabled": true } } })),
                "{category} alone should enable"
            );
        }
    }

    /// A later layer overrides an earlier one, so a workspace that turns a
    /// category off is not overruled by the editor default that turned it on.
    #[test]
    fn a_later_layer_wins() {
        let on = json!({ "inlayHints": { "variableTypes": { "enabled": true } } });
        let off = json!({ "inlayHints": { "variableTypes": { "enabled": false } } });
        assert!(!inlay_hints_enabled_in([&on, &off, &Value::Null]));
        assert!(inlay_hints_enabled_in([&off, &on, &Value::Null]));
        assert!(!inlay_hints_enabled_in([&Value::Null, &on, &off]));
    }
}

#[cfg(test)]
mod nested_project_tests {
    use super::{ProjectLocation, locate_project, persist_bodies, serving_index};
    use crate::tsgo_overlay::{CACHE_DIRECTORY, TsgoOverlay};
    use std::fs;
    use std::path::{Path, PathBuf};

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn workspace(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rsvelte-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        crate::uri::canonicalize(&dir).unwrap()
    }

    #[test]
    fn a_document_belongs_to_its_nearest_configs_project() {
        let root = workspace("locate-nearest");
        write(&root.join("src/App.svelte"), "<p />");
        write(&root.join("docs/tsconfig.json"), "{}");
        write(&root.join("docs/src/Page.svelte"), "<p />");
        let mut overlays = vec![TsgoOverlay::build_deferred(&root, None).unwrap()];
        assert!(!overlays[0].is_loaded(), "nothing is projected up front");
        assert!(overlays[0].eager_shadows().is_empty());

        assert_eq!(
            locate_project(&overlays, &root.join("src/App.svelte")),
            Some(ProjectLocation::Existing(0))
        );
        assert_eq!(
            locate_project(&overlays, &root.join("docs/src/Page.svelte")),
            Some(ProjectLocation::Nested {
                config: root.join("docs/tsconfig.json"),
                mirror_root: root.clone(),
            })
        );
        assert_eq!(
            locate_project(&overlays, Path::new("/elsewhere/App.svelte")),
            None
        );

        overlays.push(
            TsgoOverlay::build_nested(&root.join("docs"), &root.join("docs/tsconfig.json"), &root)
                .unwrap(),
        );
        assert_eq!(
            locate_project(&overlays, &root.join("docs/src/Page.svelte")),
            Some(ProjectLocation::Existing(1))
        );
        assert!(overlays[1].owns(&root.join("docs/src/Page.svelte")));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_dependencys_component_belongs_to_the_folders_project() {
        // A library component opened from `node_modules`, where the config
        // search stops, in a folder that has a config of its own.
        let root = workspace("locate-node-modules");
        write(&root.join("tsconfig.json"), "{}");
        write(&root.join("node_modules/pkg/dist/Comp.svelte"), "<p />");
        write(&root.join("app/tsconfig.json"), "{}");
        write(&root.join("app/node_modules/dep/Dep.svelte"), "<p />");
        let overlays = vec![TsgoOverlay::build_deferred(&root, None).unwrap()];
        for library in [
            "node_modules/pkg/dist/Comp.svelte",
            "app/node_modules/dep/Dep.svelte",
        ] {
            assert_eq!(
                locate_project(&overlays, &root.join(library)),
                Some(ProjectLocation::Existing(0)),
                "{library}"
            );
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_root_config_never_takes_a_nested_projects_documents() {
        // monorepo/admin: a Node-only root config, the components under `src`.
        let root = workspace("locate-admin");
        write(
            &root.join("tsconfig.json"),
            r#"{ "include": ["vite.config.ts"] }"#,
        );
        write(&root.join("vite.config.ts"), "export default {};");
        write(&root.join("src/tsconfig.json"), "{}");
        write(&root.join("src/App.svelte"), "<p />");
        let mut overlays = vec![TsgoOverlay::build_deferred(&root, None).unwrap()];
        assert_eq!(
            locate_project(&overlays, &root.join("vite.config.ts")),
            Some(ProjectLocation::Existing(0))
        );
        assert_eq!(
            locate_project(&overlays, &root.join("src/App.svelte")),
            Some(ProjectLocation::Nested {
                config: root.join("src/tsconfig.json"),
                mirror_root: root.clone(),
            })
        );
        assert!(overlays[0].load().unwrap());
        assert!(
            overlays[0].eager_shadows().is_empty(),
            "the root project owns no component"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_serving_project_is_the_owner_not_a_copy() {
        let root = workspace("serving-owner");
        write(&root.join("a/tsconfig.json"), "{}");
        write(&root.join("a/X.svelte"), "<p />");
        write(&root.join("b/tsconfig.json"), "{}");
        write(
            &root.join("b/Y.svelte"),
            "<script>import X from '../a/X.svelte';</script>",
        );
        let copy = TsgoOverlay::build_nested(&root.join("b"), &root.join("b/tsconfig.json"), &root)
            .unwrap();
        let owner =
            TsgoOverlay::build_nested(&root.join("a"), &root.join("a/tsconfig.json"), &root)
                .unwrap();
        let overlays = [copy, owner];
        assert!(
            overlays[0]
                .shadow_for_source(&root.join("a/X.svelte"))
                .is_some()
        );
        assert_eq!(serving_index(&overlays, &root.join("a/X.svelte")), Some(1));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_loaded_project_writes_its_bodies_and_sweeps_stale_ones() {
        let root = workspace("nested-bodies");
        write(&root.join("src/App.svelte"), "<p />");
        write(&root.join("docs/tsconfig.json"), "{}");
        write(&root.join("docs/src/Page.svelte"), "<p />");
        let cache = root.join(CACHE_DIRECTORY);
        // A body the root project wrote before `docs` had a config, and a
        // torn write.
        let stale = cache.join("tsgo/svelte/docs/src/Page.svelte.tsx");
        write(&stale, "stale");
        write(&cache.join("tsgo/svelte/src/.App.svelte.tsx.1.tmp"), "torn");

        let mut overlay = TsgoOverlay::build_with(&root, None, None).unwrap();
        assert!(!overlay.load().unwrap(), "built loaded");
        let events = persist_bodies(&overlay);
        let body = cache.join("tsgo/svelte/src/App.svelte.tsx");
        assert!(body.is_file());
        assert_eq!(
            fs::read_to_string(&body).unwrap(),
            overlay
                .shadow_for_source(&root.join("src/App.svelte"))
                .unwrap()
                .text
        );
        assert!(!stale.exists(), "a component the project no longer has");
        assert!(!cache.join("tsgo/svelte/src/.App.svelte.tsx.1.tmp").exists());
        assert!(events.iter().any(|(uri, change)| {
            crate::uri::uri_to_path(uri.as_str()) == stale
                && *change == lsp_types::FileChangeType::DELETED
        }));
        assert!(persist_bodies(&overlay).is_empty(), "nothing changed");
        assert!(overlay.unresolved_shadow_routes().is_empty());
        let _ = fs::remove_dir_all(&root);
    }
}
