//! Svelte projections for the tsgo LSP child process.
//!
//! Every project gets an overlay tsconfig, the svelte2tsx shims and a shadow
//! tree mirroring the workspace. The generated `.svelte.tsx` body of every
//! component the project serves is written into that tree, where the config's
//! `svelte/**/*` include finds it, so tsgo builds the program from disk exactly
//! once, and only for a project a request actually needs. Only documents open
//! in the editor (and JavaScript components, whose script kind the `.tsx` name
//! would lose) are handed to tsgo as in-memory buffers, and an open buffer
//! always wins over the body on disk.
//!
//! Keeping the bodies in memory instead — `didOpen` for every component — makes
//! each `didOpen` a file *creation* for tsgo, so it rebuilds the whole program
//! once per component before it can answer the first request.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::fs;
use std::io;
use std::ops::Bound;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, OnceLock};

use lsp_types::{Position, Range, Uri};
use rsvelte_check::overlay::{
    CACHE_GITIGNORE, FallbackSvelteTypes, SHIM_FILES, SHIM_JSX_V4_NAME, fallback_svelte_types,
    global_type_files,
};
use rsvelte_projection::{
    ProjectionEngine, ProjectionMap, RewriteExternalImportsOptions, Svelte2TsxMode,
    Svelte2TsxNamespace, Svelte2TsxOptions, SvelteVersion, is_typescript_component,
};
use serde_json::json;
use sourcemap::{SourceMap, SourceMapBuilder};

use crate::text::LineIndex;

/// Distinct from upstream rsvelte's diskless `.rsvelte-language-server`, which
/// refuses to run over a shadow body that exists on disk.
pub(crate) const CACHE_DIRECTORY: &str = ".svelte-rs";
const NESTED_PROJECTS_DIRECTORY: &str = "projects";
const TSGO_DIRECTORY: &str = "tsgo";
const SHADOW_DIRECTORY: &str = "svelte";
const OVERLAY_TSCONFIG: &str = "tsconfig.json";
pub(crate) const IGNORE_START: &str = "/*Ωignore_startΩ*/";
pub(crate) const IGNORE_END: &str = "/*Ωignore_endΩ*/";
/// `svelte2tsx/src/utils/ignore.ts:4`. rsvelte emits this inline rather than
/// through a constant, so this is the first place it is named.
pub(crate) const IGNORE_POSITION: &str = "/*Ωignore_positionΩ*/";

/// One virtual document to open or update in the tsgo child.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowDocument {
    /// URI of the user's `.svelte` document.
    pub source_uri: Uri,
    /// URI of `<cache>/svelte/<relative>.svelte.tsx`.
    pub shadow_uri: Uri,
    /// LSP language id sent to tsgo.
    pub language_id: String,
    /// Generated svelte2tsx body. The overlay persists it for a closed document
    /// ([`TsgoOverlay::persist_body`]); an editor buffer's text is never written.
    pub text: String,
    /// Document version sent to tsgo.
    pub version: i32,
}

/// Observable invariants that prevent an unresolved component from silently
/// falling through to ambient `declare module '*.svelte'` and becoming `any`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowResolutionInfo {
    /// Source component routed by this entry.
    pub source_path: PathBuf,
    /// Virtual TSX path tsgo must have open.
    pub shadow_path: PathBuf,
    /// Whether the source is confined to the workspace.
    pub source_in_workspace: bool,
    /// Whether the shadow is confined to the cache's `svelte` root.
    pub shadow_in_cache: bool,
    /// Whether tsgo's required on-disk parent directory exists.
    pub parent_directory_exists: bool,
    /// Whether a shadow body is registered for this route.
    pub shadow_registered: bool,
    /// Whether the overlay tsconfig's include pattern admits the shadow.
    pub included_by_overlay_config: bool,
    /// Whether the body tsgo reads for a closed document is on disk.
    pub body_exists_on_disk: bool,
}

impl ShadowResolutionInfo {
    /// Whether the route has every invariant needed to avoid ambient-any
    /// fallback: tsgo finds the body through the overlay config's include.
    #[must_use]
    pub const fn is_resolvable(&self) -> bool {
        self.source_in_workspace
            && self.shadow_in_cache
            && self.parent_directory_exists
            && self.shadow_registered
            && self.included_by_overlay_config
            && self.body_exists_on_disk
    }
}

/// Changes found while reconciling the eager project set with disk.
#[derive(Debug, Default, Clone)]
pub struct OverlayUpdate {
    /// New or changed shadows projected from disk: their bodies are persisted.
    pub opened_or_changed: Vec<ShadowDocument>,
    /// New copies projected from an editor buffer. tsgo holds them as
    /// buffers, like the document they copy; they never reach disk.
    pub buffers: Vec<ShadowDocument>,
    /// Shadow URIs removed because their sources no longer exist.
    pub closed: Vec<Uri>,
}

/// One document the editor has open, as tsgo must see it instead of the file.
#[derive(Debug, Clone)]
pub struct EditorBuffer {
    pub text: Arc<String>,
    pub version: i32,
}

/// The documents the editor has open, by canonical path. A file the editor
/// holds is the editor's to report: a change on disk never replaces its
/// shadow, and a project that needs a copy of it projects the buffer.
#[derive(Debug, Default, Clone)]
pub struct EditorBuffers(HashMap<PathBuf, EditorBuffer>);

impl EditorBuffers {
    pub fn insert(&mut self, path: PathBuf, text: Arc<String>, version: i32) {
        self.0.insert(path, EditorBuffer { text, version });
    }

    #[must_use]
    pub fn get(&self, path: &Path) -> Option<&EditorBuffer> {
        self.0.get(path)
    }

    #[must_use]
    pub fn contains(&self, path: &Path) -> bool {
        self.0.contains_key(path)
    }

    /// The open documents' paths.
    pub fn paths(&self) -> impl Iterator<Item = &PathBuf> {
        self.0.keys()
    }
}

/// Failure to prepare or update an overlay.
#[derive(Debug)]
pub enum TsgoOverlayError {
    /// Filesystem operation failed.
    Io(io::Error),
    /// A source path does not name a confined `.svelte` file.
    InvalidSource { path: PathBuf, reason: &'static str },
    /// svelte2tsx rejected a source document.
    Projection { path: PathBuf, message: String },
    /// A filesystem path could not be represented as a file URI.
    InvalidUri(PathBuf),
    /// The generated source map was malformed.
    InvalidSourceMap { path: PathBuf, message: String },
}

impl fmt::Display for TsgoOverlayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "overlay I/O error: {error}"),
            Self::InvalidSource { path, reason } => {
                write!(f, "invalid Svelte source {}: {reason}", path.display())
            }
            Self::Projection { path, message } => {
                write!(f, "svelte2tsx failed on {}: {message}", path.display())
            }
            Self::InvalidUri(path) => write!(f, "cannot encode {} as a file URI", path.display()),
            Self::InvalidSourceMap { path, message } => {
                write!(f, "invalid source map for {}: {message}", path.display())
            }
        }
    }
}

impl std::error::Error for TsgoOverlayError {}

impl From<io::Error> for TsgoOverlayError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

#[derive(Debug, Clone, Copy)]
struct MappingToken {
    generated_line: u32,
    generated_column: u32,
    source: Option<(u32, u32)>,
}

#[derive(Debug, Clone, Copy)]
struct ReverseMappingToken {
    source_line: u32,
    source_column: u32,
    generated_line: u32,
    generated_column: u32,
}

#[derive(Debug, Default)]
struct PreprocessMappings {
    generated: Vec<MappingToken>,
    original: Vec<ReverseMappingToken>,
}

/// Whether a Svelte projection used a user-preprocessed document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreprocessStatus {
    /// Original and preprocessed texts are identical.
    Identity,
    /// Changed preprocessed text has a usable source map back to the original.
    Mapped,
    /// Changed preprocessed text has no source map and cannot be mapped safely.
    Unmapped,
}

struct ShadowState {
    document: ShadowDocument,
    source_path: PathBuf,
    shadow_path: PathBuf,
    source_text: String,
    source_index: LineIndex,
    preprocessed_text: Option<String>,
    preprocessed_index: Option<LineIndex>,
    preprocess_map: Option<String>,
    preprocess_mappings: PreprocessMappings,
    preprocess_status: PreprocessStatus,
    generated_index: LineIndex,
    exact_map: ProjectionMap,
    source_map: Option<String>,
    tokens: Vec<MappingToken>,
    generated_ranges: Vec<std::ops::Range<usize>>,
    /// The `.tsx` this overlay appends to every `.svelte` module specifier so
    /// tsgo resolves the shadow. Unlike an `Ωignore` region it sits *inside* a
    /// source-backed token, so a range that merely spans it — `'./X.svelte.tsx'`,
    /// which is what tsgo hands back for a module hover — still has a source
    /// range on both sides (#4097).
    import_suffix_ranges: Vec<std::ops::Range<usize>>,
    identity: bool,
    plain_insertions: Vec<(usize, std::ops::Range<usize>)>,
    /// Byte offset in `source_text` that generated offset 0 corresponds to,
    /// for the `FragmentMapper` upstream installs when there is no projection.
    fragment_offset: usize,
    /// svelte2tsx's message, when this shadow is the parser-error fallback.
    parser_error: Option<String>,
}

impl ShadowState {
    /// A `.svelte` component's shadow — a projection or its parser-error
    /// fallback — rather than a plain module routed through the project.
    const fn is_component(&self) -> bool {
        !self.identity || self.parser_error.is_some()
    }
}

/// Workspace-scoped overlay used by the tsgo LSP proxy.
pub struct TsgoOverlay {
    workspace: PathBuf,
    /// The directory the shadow tree mirrors. The workspace itself, except for
    /// a nested project, whose documents may import `.svelte` files beside its
    /// own directory: those need a shadow at the same relative position.
    mirror_root: PathBuf,
    cache_dir: PathBuf,
    shadow_dir: PathBuf,
    tsconfig_path: PathBuf,
    source_tsconfig: Option<PathBuf>,
    svelte_fallback: Option<FallbackSvelteTypes>,
    engine: ProjectionEngine,
    accessors: bool,
    namespace: Svelte2TsxNamespace,
    entries: BTreeMap<PathBuf, ShadowState>,
    by_shadow: BTreeMap<PathBuf, PathBuf>,
    /// Whether the members have been discovered and projected. A deferred
    /// overlay ([`Self::build_deferred`]) has written its config and shims
    /// only, and projects nothing until a document it owns needs it.
    loaded: bool,
    /// The members whose nearest config is this project's own. Every other
    /// member is a copy of a component another project owns, there only so an
    /// import of it resolves inside this project's shadow tree.
    owned: BTreeSet<PathBuf>,
    /// Components an import names that do not exist (yet), or that existed
    /// and were deleted: creating one makes it a copy again (see
    /// [`Self::apply_file_changes`]).
    missing: BTreeSet<PathBuf>,
    /// The `paths` patterns imports are followed through, read once: a
    /// config change rebuilds the overlay.
    aliases: OnceLock<Vec<AliasPattern>>,
    /// Components a plain module this project owns imports — a barrel's
    /// re-exports. Any document may reach them through the module, which
    /// [`Self::reachable_javascript`] does not follow, so they always count.
    module_imports: BTreeSet<PathBuf>,
}

impl fmt::Debug for TsgoOverlay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TsgoOverlay")
            .field("workspace", &self.workspace)
            .field("cache_dir", &self.cache_dir)
            .field("tsconfig_path", &self.tsconfig_path)
            .field("loaded", &self.loaded)
            .field("entries", &self.entries.len())
            .finish_non_exhaustive()
    }
}

impl TsgoOverlay {
    /// Build the workspace overlay and project its members (see
    /// [`Self::load`]) at once.
    ///
    /// `tsconfig` defaults to `tsconfig.json`, then `jsconfig.json`, when one
    /// exists at the workspace root.
    pub fn build(workspace: &Path, tsconfig: Option<&Path>) -> Result<Self, TsgoOverlayError> {
        Self::build_with(workspace, tsconfig, server_svelte_root().as_deref())
    }

    /// Build the overlay, saying where to resolve `svelte` from when the
    /// workspace does not provide one (`importPackage.ts:27-38`).
    pub fn build_with(
        workspace: &Path,
        tsconfig: Option<&Path>,
        svelte_fallback_root: Option<&Path>,
    ) -> Result<Self, TsgoOverlayError> {
        Self::build_mirrored(workspace, tsconfig, svelte_fallback_root, None, true)
    }

    /// Prepare the workspace overlay — cache, shims, config — without
    /// discovering or projecting a single member; [`Self::load`] does that
    /// when the first document it owns is opened. A repository with many
    /// projects then only pays for the ones the editor touches.
    pub fn build_deferred(
        workspace: &Path,
        tsconfig: Option<&Path>,
    ) -> Result<Self, TsgoOverlayError> {
        Self::build_mirrored(
            workspace,
            tsconfig,
            server_svelte_root().as_deref(),
            None,
            false,
        )
    }

    /// The project `spec` names, deferred: its workspace folder's own
    /// overlay ([`Self::build_deferred`]) or a nested config's.
    pub fn build_for(spec: &ProjectSpec) -> Result<Self, TsgoOverlayError> {
        match spec.config.as_deref() {
            Some(config) if !spec.is_folder() => Self::build_mirrored(
                config.parent().unwrap_or(&spec.mirror_root),
                Some(config),
                server_svelte_root().as_deref(),
                Some(&spec.mirror_root),
                false,
            ),
            _ => Self::build_deferred(&spec.mirror_root, None),
        }
    }

    /// Whether this is the project `spec` names.
    #[must_use]
    pub fn is_project(&self, spec: &ProjectSpec) -> bool {
        self.mirror_root == spec.mirror_root
            && if spec.is_folder() {
                self.workspace == self.mirror_root
            } else {
                self.source_tsconfig == spec.config
            }
    }

    /// Whether the members have been discovered and projected.
    #[must_use]
    pub const fn is_loaded(&self) -> bool {
        self.loaded
    }

    /// Discover and project the members of a deferred overlay: every
    /// component this project owns, plus a copy of every component they reach
    /// through an import (see [`Self::discover_sources`]). Returns whether it
    /// loaded now; an overlay loads once. A member that cannot be read or
    /// projected is skipped, so one bad file never costs the whole project.
    pub fn load(&mut self) -> Result<bool, TsgoOverlayError> {
        self.load_with_threads(MAX_PROJECTION_THREADS)
    }

    /// [`Self::load`] on at most `threads` projection threads: a load in the
    /// background leaves the editor's work most of the machine.
    pub fn load_with_threads(&mut self, threads: usize) -> Result<bool, TsgoOverlayError> {
        if self.loaded {
            return Ok(false);
        }
        self.loaded = true;
        let sources = self.discover_sources().into_iter().collect::<Vec<_>>();
        for (path, state) in self.project_sources(&sources, threads) {
            match state {
                Ok(state) => {
                    self.insert_state(state);
                }
                Err(error) => crate::log::warn(format_args!(
                    "could not project {} for tsgo: {error}",
                    path.display()
                )),
            }
        }
        self.write_tsconfig()?;
        self.link_node_modules();
        Ok(true)
    }

    /// tsgo looks a bare import up in the `node_modules` of every directory
    /// above the importing file, and a config's `types` and default
    /// `typeRoots` in those above the config. A shadow and the overlay config
    /// live in the cache, not beside their source, so a nested package's own
    /// `node_modules` (monorepo/admin/node_modules, under a workspace-root cache)
    /// is on neither path: every import of a package only it installs fails.
    /// Each such directory is mirrored as a symlink at its owner's position
    /// in the shadow tree and in the config's ancestry, so both lookups walk
    /// the same chain as from the source. `**` never matches `node_modules`,
    /// so no package joins the project's root files. The mirror root's own
    /// `node_modules` needs no link: the cache lives in the mirror root, so
    /// every lookup from it ends there anyway — and a link there would keep a
    /// dependency's component (`node_modules/pkg/Comp.svelte`, opened from a
    /// definition) out of the shadow tree.
    pub fn link_node_modules(&self) {
        let cache_root = self.mirror_root.join(CACHE_DIRECTORY);
        let mut links = BTreeMap::new();
        let mut seen = BTreeSet::new();
        let redundant = self.shadow_dir.join("node_modules");
        if fs::symlink_metadata(&redundant).is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            let _ = fs::remove_file(&redundant);
        }
        for entry in self.entries.values().filter(|entry| entry.is_component()) {
            let mut directory = entry.source_path.parent();
            while let Some(current) = directory {
                if current == self.mirror_root
                    || !current.starts_with(&self.mirror_root)
                    || !seen.insert(current.to_path_buf())
                {
                    break;
                }
                let target = current.join("node_modules");
                if let Ok(relative) = current.strip_prefix(&self.mirror_root)
                    && target.is_dir()
                {
                    links.insert(self.shadow_dir.join(relative).join("node_modules"), target);
                }
                directory = current.parent();
            }
        }
        // `<cache>/projects/<project>/tsgo/tsconfig.json`: the directories
        // above the config mirror the project's own ancestry.
        let mut source = Some(self.workspace.as_path());
        let mut mirror = self.cache_dir.parent();
        while let (Some(current), Some(position)) = (source, mirror) {
            if current == self.mirror_root || !position.starts_with(&cache_root) {
                break;
            }
            let target = current.join("node_modules");
            if target.is_dir() {
                links.insert(position.join("node_modules"), target);
            }
            source = current.parent();
            mirror = position.parent();
        }
        for (link, target) in links {
            // A member reached through `node_modules` keeps a real directory.
            if self
                .entries
                .values()
                .any(|entry| entry.shadow_path.starts_with(&link))
            {
                continue;
            }
            if let Err(error) = place_symlink(&link, &target, &cache_root) {
                crate::log::warn(format_args!(
                    "could not mirror {} for tsgo: {error}",
                    target.display()
                ));
            }
        }
    }

    /// Read and project `sources` from disk, on up to
    /// [`MAX_PROJECTION_THREADS`] threads: a project's first document waits
    /// for every member, and svelte2tsx is pure per file. The results come
    /// back in `sources` order, so the overlay built from them does not
    /// depend on scheduling. A panic is one member's error, as it is for the
    /// analysis worker.
    fn project_sources(
        &self,
        sources: &[PathBuf],
        max_threads: usize,
    ) -> Vec<(PathBuf, Result<ShadowState, String>)> {
        let project = |path: &PathBuf| {
            let state = fs::read_to_string(path)
                .map_err(|error| error.to_string())
                .and_then(|text| {
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        self.project_state(path, &text, &text, None, 0)
                    }))
                    .map_err(|_| "svelte2tsx panicked".to_string())?
                    .map_err(|error| error.to_string())
                });
            (path.clone(), state)
        };
        let threads = std::thread::available_parallelism()
            .map_or(1, std::num::NonZero::get)
            .min(max_threads)
            .min(sources.len().div_ceil(MIN_SOURCES_PER_THREAD));
        if threads <= 1 {
            return sources.iter().map(project).collect();
        }
        let chunk = sources.len().div_ceil(threads);
        std::thread::scope(|scope| {
            let parts = sources
                .chunks(chunk)
                .map(|part| {
                    let handle = std::thread::Builder::new()
                        .name("rsvelte-projection".to_string())
                        .stack_size(PROJECTION_STACK_SIZE)
                        .spawn_scoped(scope, move || part.iter().map(project).collect::<Vec<_>>())
                        .ok();
                    (part, handle)
                })
                .collect::<Vec<_>>();
            parts
                .into_iter()
                .flat_map(|(part, handle)| {
                    // A thread that could not start (or died) leaves its part
                    // to this one.
                    handle
                        .and_then(|handle| handle.join().ok())
                        .unwrap_or_else(|| part.iter().map(project).collect())
                })
                .collect()
        })
    }

    /// Whether `source_path` is one of this project's own members — its
    /// nearest config is this overlay's — rather than a copy of another
    /// project's component.
    #[must_use]
    pub fn owns(&self, source_path: &Path) -> bool {
        self.owned.contains(&self.lookup_source_path(source_path))
    }

    /// Build the project a nested `tsconfig` owns, rooted at `workspace` but
    /// with its shadow tree mirroring `mirror_root` — the enclosing workspace —
    /// so a relative import that leaves the project directory still resolves.
    pub fn build_nested(
        workspace: &Path,
        tsconfig: &Path,
        mirror_root: &Path,
    ) -> Result<Self, TsgoOverlayError> {
        Self::build_mirrored(
            workspace,
            Some(tsconfig),
            server_svelte_root().as_deref(),
            Some(mirror_root),
            true,
        )
    }

    fn build_mirrored(
        workspace: &Path,
        tsconfig: Option<&Path>,
        svelte_fallback_root: Option<&Path>,
        mirror_root: Option<&Path>,
        load: bool,
    ) -> Result<Self, TsgoOverlayError> {
        let workspace = absolute_normalized(workspace);
        let workspace = fs::canonicalize(&workspace)?;
        if !workspace.is_dir() {
            return Err(TsgoOverlayError::InvalidSource {
                path: workspace,
                reason: "workspace is not a directory",
            });
        }
        let mirror_root = match mirror_root {
            Some(root) => fs::canonicalize(absolute_normalized(root))?,
            None => workspace.clone(),
        };
        if !workspace.starts_with(&mirror_root) {
            return Err(TsgoOverlayError::InvalidSource {
                path: workspace,
                reason: "workspace is outside the directory its shadows mirror",
            });
        }

        // A nested project keeps its cache beside the enclosing workspace's, so
        // its shadows sit under a `rootDirs` entry exactly as a root project's do.
        let cache_root = mirror_root.join(CACHE_DIRECTORY);
        let cache_dir = match workspace.strip_prefix(&mirror_root) {
            Ok(relative) if !relative.as_os_str().is_empty() => cache_root
                .join(NESTED_PROJECTS_DIRECTORY)
                .join(relative)
                .join(TSGO_DIRECTORY),
            _ => cache_root.join(TSGO_DIRECTORY),
        };
        let shadow_dir = cache_dir.join(SHADOW_DIRECTORY);
        reject_symlink_components(&cache_dir, &mirror_root)?;
        fs::create_dir_all(&shadow_dir)?;
        reject_symlink_components(&shadow_dir, &cache_dir)?;
        write_cache_gitignore(&cache_root);

        let source_tsconfig = resolve_tsconfig(&workspace, tsconfig);
        let tsconfig_path = cache_dir.join(OVERLAY_TSCONFIG);
        let svelte_fallback = fallback_svelte_types(&workspace, svelte_fallback_root, &cache_dir);
        let compiler = rsvelte_check::config::load_compiler_options(&workspace);
        let mut overlay = Self {
            workspace,
            mirror_root,
            cache_dir,
            shadow_dir,
            tsconfig_path,
            source_tsconfig,
            svelte_fallback,
            engine: ProjectionEngine::new(),
            accessors: compiler.projection_accessors(),
            namespace: compiler.projection_namespace(),
            entries: BTreeMap::new(),
            by_shadow: BTreeMap::new(),
            loaded: false,
            owned: BTreeSet::new(),
            missing: BTreeSet::new(),
            aliases: OnceLock::new(),
            module_imports: BTreeSet::new(),
        };
        overlay.materialize_support_files()?;
        if load {
            overlay.load()?;
        }
        Ok(overlay)
    }

    /// Absolute workspace root.
    #[must_use]
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// The directory the shadow tree mirrors: the workspace, or for a nested
    /// project the workspace that encloses it.
    #[must_use]
    pub fn mirror_root(&self) -> &Path {
        &self.mirror_root
    }

    /// Directory containing the persisted config, shims, and skeleton.
    #[must_use]
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// Overlay tsconfig to pass to tsgo.
    #[must_use]
    pub fn tsconfig_path(&self) -> &Path {
        &self.tsconfig_path
    }

    /// The project config this overlay was built from, when the workspace has
    /// one. `None` is upstream's configless fallback project.
    #[must_use]
    pub fn source_tsconfig(&self) -> Option<&Path> {
        self.source_tsconfig.as_deref()
    }

    /// Generate or replace one virtual shadow buffer.
    pub fn open_or_update(
        &mut self,
        source_path: &Path,
        text: &str,
        version: i32,
    ) -> Result<ShadowDocument, TsgoOverlayError> {
        self.open_or_update_preprocessed(source_path, text, text, None, version)
    }

    /// Generate or replace one virtual shadow from a preprocessed Svelte document.
    ///
    /// `preprocess_map` is a standard v3 map from `preprocessed_text` back to
    /// `original_text`. Changed text without a map remains usable by tsgo, but
    /// editor positions cannot be mapped safely.
    pub fn open_or_update_preprocessed(
        &mut self,
        source_path: &Path,
        original_text: &str,
        preprocessed_text: &str,
        preprocess_map: Option<&str>,
        version: i32,
    ) -> Result<ShadowDocument, TsgoOverlayError> {
        let state = self.project_state(
            source_path,
            original_text,
            preprocessed_text,
            preprocess_map,
            version,
        )?;
        // A component that joins outside discovery and file events — the
        // editor opened it before it reached disk, or saved it as a new name —
        // is still this project's own when it lives where the project owns
        // files; a copy at the same depth must never answer for it.
        if !self.entries.contains_key(&state.source_path) && self.owns_location(&state.source_path)
        {
            self.owned.insert(state.source_path.clone());
        }
        Ok(self.insert_state(state))
    }

    /// Install a projection, replacing the source's previous one.
    fn insert_state(&mut self, state: ShadowState) -> ShadowDocument {
        let document = state.document.clone();
        let source_path = state.source_path.clone();
        let shadow_path = state.shadow_path.clone();
        if let Some(old) = self.entries.insert(source_path.clone(), state) {
            self.by_shadow.remove(&old.shadow_path);
        }
        self.by_shadow.insert(shadow_path, source_path);
        document
    }

    /// [`Self::open_or_update_preprocessed`]'s projection, without touching
    /// the overlay's routes, so members can be projected in parallel.
    fn project_state(
        &self,
        source_path: &Path,
        original_text: &str,
        preprocessed_text: &str,
        preprocess_map: Option<&str>,
        version: i32,
    ) -> Result<ShadowState, TsgoOverlayError> {
        let source_path = self.confined_source(source_path)?;
        let shadow_path = self.shadow_path_for(&source_path)?;
        self.ensure_shadow_parent(&shadow_path)?;

        let preprocess_status = if original_text == preprocessed_text {
            PreprocessStatus::Identity
        } else if preprocess_map.is_some() {
            PreprocessStatus::Mapped
        } else {
            PreprocessStatus::Unmapped
        };
        let preprocess_mappings = if preprocess_status == PreprocessStatus::Mapped {
            parse_preprocess_mappings(
                &source_path,
                preprocess_map.expect("mapped preprocessing has a map"),
            )?
        } else {
            PreprocessMappings::default()
        };

        let options = self.projection_options(&source_path, &shadow_path, preprocessed_text);
        let artifact = match self.engine.project(preprocessed_text, options) {
            Ok(artifact) => artifact,
            // `DocumentSnapshot.ts:275-291`: a rejected document still gets a
            // snapshot, over its extracted script, so TypeScript keeps answering.
            Err(error) => {
                return self.parser_error_state(
                    &source_path,
                    &shadow_path,
                    original_text,
                    version,
                    error.to_string(),
                );
            }
        };
        let mut exact_map = if preprocess_status == PreprocessStatus::Identity {
            artifact.exact_mappings.unwrap_or_default()
        } else {
            ProjectionMap::default()
        };
        let projection_source_map = artifact.source_map;
        let mut tokens = parse_mapping_tokens(&source_path, projection_source_map.as_deref())?;
        let original_generated = artifact.code;
        let original_index = LineIndex::new(&original_generated);
        let (generated_text, import_insertions) = rewrite_plain_svelte_imports(&original_generated);
        let insertion_positions = import_insertions
            .iter()
            .map(|(original_offset, generated_range)| {
                (
                    original_index.position(&original_generated, *original_offset),
                    generated_range.len() as u32,
                )
            })
            .collect::<Vec<_>>();
        for token in &mut tokens {
            token.generated_column += insertion_positions
                .iter()
                .filter(|(position, _)| {
                    token.generated_line == position.line
                        && token.generated_column >= position.character
                })
                .map(|(_, length)| *length)
                .sum::<u32>();
        }
        for (_, generated_range) in &import_insertions {
            exact_map.insert_generated(generated_range.start as u32, generated_range.len() as u32);
        }
        let generated_index = LineIndex::new(&generated_text);
        let source_map = compose_source_map(
            &source_path,
            original_text,
            projection_source_map.as_deref(),
            &tokens,
            &preprocess_mappings,
            preprocess_status,
            &generated_text,
            &generated_index,
            &import_insertions,
        )?;
        let document = ShadowDocument {
            source_uri: path_to_uri(&source_path)?,
            shadow_uri: path_to_uri(&shadow_path)?,
            language_id: shadow_language_id(preprocessed_text).to_string(),
            text: generated_text,
            version,
        };
        let generated_ranges = ignored_ranges(&document.text);
        let import_suffix_ranges = import_insertions
            .iter()
            .map(|(_, generated)| generated.clone())
            .collect::<Vec<_>>();
        let state = ShadowState {
            source_path: source_path.clone(),
            shadow_path: shadow_path.clone(),
            source_text: original_text.to_string(),
            source_index: LineIndex::new(original_text),
            preprocessed_text: (preprocess_status != PreprocessStatus::Identity)
                .then(|| preprocessed_text.to_string()),
            preprocessed_index: (preprocess_status != PreprocessStatus::Identity)
                .then(|| LineIndex::new(preprocessed_text)),
            preprocess_map: preprocess_map.map(str::to_string),
            preprocess_mappings,
            preprocess_status,
            generated_index,
            exact_map,
            source_map,
            tokens,
            generated_ranges,
            import_suffix_ranges,
            identity: false,
            plain_insertions: Vec::new(),
            fragment_offset: 0,
            parser_error: None,
            document,
        };
        Ok(state)
    }

    /// The snapshot upstream keeps when svelte2tsx rejects a document: the
    /// instance script's body (else the module script's, else nothing) mapped
    /// back by a constant shift, so TypeScript still answers and every caller
    /// can see that the projection failed.
    fn parser_error_state(
        &self,
        source_path: &Path,
        shadow_path: &Path,
        original_text: &str,
        version: i32,
        message: String,
    ) -> Result<ShadowState, TsgoOverlayError> {
        let fragment = crate::context::fallback_script_body(original_text);
        let fragment_offset = fragment.as_ref().map_or(0, |body| body.start);
        let generated_text = fragment
            .and_then(|body| original_text.get(body))
            .unwrap_or("")
            .to_string();
        let document = ShadowDocument {
            source_uri: path_to_uri(source_path)?,
            shadow_uri: path_to_uri(shadow_path)?,
            language_id: shadow_language_id(original_text).to_string(),
            text: generated_text.clone(),
            version,
        };
        let state = ShadowState {
            source_path: source_path.to_path_buf(),
            shadow_path: shadow_path.to_path_buf(),
            source_text: original_text.to_string(),
            source_index: LineIndex::new(original_text),
            preprocessed_text: None,
            preprocessed_index: None,
            preprocess_map: None,
            preprocess_mappings: PreprocessMappings::default(),
            preprocess_status: PreprocessStatus::Identity,
            generated_index: LineIndex::new(&generated_text),
            exact_map: ProjectionMap::default(),
            tokens: Vec::new(),
            source_map: None,
            generated_ranges: Vec::new(),
            import_suffix_ranges: Vec::new(),
            identity: true,
            plain_insertions: Vec::new(),
            fragment_offset,
            parser_error: Some(message),
            document,
        };
        Ok(state)
    }

    /// svelte2tsx's message for a document whose projection failed.
    #[must_use]
    pub fn parser_error(&self, source_path: &Path) -> Option<&str> {
        let path = self.lookup_source_path(source_path);
        self.entries.get(&path)?.parser_error.as_deref()
    }

    /// Route an open TypeScript or JavaScript buffer through the overlay
    /// project without persisting its body.
    pub fn open_plain(
        &mut self,
        source_path: &Path,
        text: &str,
        version: i32,
        language_id: &str,
    ) -> Result<ShadowDocument, TsgoOverlayError> {
        let source_path = self.confined_any_source(source_path)?;
        let relative = source_path.strip_prefix(&self.mirror_root).map_err(|_| {
            TsgoOverlayError::InvalidSource {
                path: source_path.clone(),
                reason: "source escapes the workspace",
            }
        })?;
        let shadow_path = self.shadow_dir.join(relative);
        self.ensure_shadow_parent(&shadow_path)?;
        let (generated_text, plain_insertions) = rewrite_plain_svelte_imports(text);
        let document = ShadowDocument {
            source_uri: path_to_uri(&source_path)?,
            shadow_uri: path_to_uri(&shadow_path)?,
            language_id: language_id.to_string(),
            text: generated_text,
            version,
        };
        let state = ShadowState {
            source_path: source_path.clone(),
            shadow_path: shadow_path.clone(),
            source_text: text.to_string(),
            source_index: LineIndex::new(text),
            preprocessed_text: None,
            preprocessed_index: None,
            preprocess_map: None,
            preprocess_mappings: PreprocessMappings::default(),
            preprocess_status: PreprocessStatus::Identity,
            generated_index: LineIndex::new(text),
            exact_map: ProjectionMap::default(),
            tokens: Vec::new(),
            source_map: None,
            generated_ranges: Vec::new(),
            import_suffix_ranges: Vec::new(),
            identity: true,
            plain_insertions,
            fragment_offset: 0,
            parser_error: None,
            document: document.clone(),
        };
        if let Some(old) = self.entries.insert(source_path.clone(), state) {
            self.by_shadow.remove(&old.shadow_path);
        }
        self.by_shadow.insert(shadow_path, source_path);
        Ok(document)
    }

    /// Remove a routed plain buffer and its empty directory skeleton.
    pub fn close_plain(&mut self, source_path: &Path) -> Result<Option<Uri>, TsgoOverlayError> {
        let source_path = self.lookup_source_path(source_path);
        let uri = self
            .entries
            .get(&source_path)
            .filter(|entry| entry.identity)
            .map(|entry| entry.document.shadow_uri.clone());
        if uri.is_some() {
            self.remove_source(&source_path)?;
        }
        Ok(uri)
    }

    /// Re-project a source using its current on-disk contents.
    pub fn update_from_disk(
        &mut self,
        source_path: &Path,
        version: i32,
    ) -> Result<ShadowDocument, TsgoOverlayError> {
        let source_path = self.confined_source(source_path)?;
        let text = fs::read_to_string(&source_path)?;
        self.open_or_update(&source_path, &text, version)
    }

    /// Close an editor buffer while keeping its component in the project.
    ///
    /// If the source remains on disk, the returned document contains a fresh
    /// disk projection: the caller persists it ([`Self::persist_body`]) before
    /// sending `didClose`, so tsgo reads the saved component from disk. If the
    /// source was deleted, its route and body are removed and `None` is
    /// returned; the caller should send `didClose` for the previously known
    /// shadow URI.
    pub fn close(
        &mut self,
        source_path: &Path,
    ) -> Result<Option<ShadowDocument>, TsgoOverlayError> {
        let source_path = self.confined_source(source_path)?;
        let next_version = self
            .entries
            .get(&source_path)
            .map_or(0, |entry| entry.document.version.saturating_add(1));
        match fs::read_to_string(&source_path) {
            Ok(text) => self
                .open_or_update(&source_path, &text, next_version)
                .map(Some),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                // As for a deletion event: the file may come back.
                if self.entries.contains_key(&source_path) {
                    self.missing.insert(source_path.clone());
                }
                self.remove_source(&source_path)?;
                Ok(None)
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Reconcile eager shadows with the current workspace tree. A removed
    /// source's body is deleted; the caller persists new and changed ones.
    pub fn refresh(&mut self) -> Result<OverlayUpdate, TsgoOverlayError> {
        self.refresh_except(&EditorBuffers::default())
    }

    /// [`Self::refresh`], leaving every source the editor has open to its
    /// buffer: a change on disk does not replace it (the editor reports its
    /// own edits), so re-projecting the file would hand tsgo stale text until
    /// the next keystroke. An open document this project did not hold yet
    /// joins it from the buffer (in [`OverlayUpdate::buffers`]).
    pub fn refresh_except(
        &mut self,
        editor: &EditorBuffers,
    ) -> Result<OverlayUpdate, TsgoOverlayError> {
        let discovered = self.discover_sources();
        let known = self
            .entries
            .iter()
            .filter(|(_, entry)| !entry.identity)
            .map(|(path, _)| path.clone())
            .collect::<BTreeSet<_>>();
        let mut update = OverlayUpdate::default();

        for removed in known.difference(&discovered) {
            if editor.contains(removed) {
                continue;
            }
            if let Some(entry) = self.entries.get(removed) {
                update.closed.push(entry.document.shadow_uri.clone());
            }
            self.remove_source(removed)?;
        }
        for path in &discovered {
            if let Some(buffer) = editor.get(path) {
                if !self.entries.contains_key(path) {
                    update
                        .buffers
                        .push(self.open_or_update(path, &buffer.text, buffer.version)?);
                }
                continue;
            }
            let text = fs::read_to_string(path)?;
            let unchanged = self
                .entries
                .get(path)
                .is_some_and(|entry| entry.source_text == text);
            if unchanged {
                continue;
            }
            let version = self
                .entries
                .get(path)
                .map_or(0, |entry| entry.document.version.saturating_add(1));
            update
                .opened_or_changed
                .push(self.open_or_update(path, &text, version)?);
        }
        // Discovery read the files; an unsaved import is only in the buffer.
        let open = self
            .entries
            .keys()
            .filter_map(|path| Some((path.clone(), editor.get(path)?.text.to_string())))
            .collect::<Vec<_>>();
        self.follow_imports(open, editor, &mut update)?;
        if !update.opened_or_changed.is_empty() || !update.buffers.is_empty() {
            self.link_node_modules();
        }
        Ok(update)
    }

    /// Apply the file events an editor reports to a loaded project, touching
    /// only the files they name: a component it holds is re-projected or
    /// removed, a new component it owns joins, and a changed component or
    /// owned module has its `.svelte` imports followed for new copies — the
    /// cost of a save no longer grows with the project. `None` asks for a
    /// [`Self::refresh_except`] instead, for an event this cannot place (a
    /// directory, say). A copy no import needs any more stays until then:
    /// one extra root file, never a wrong answer.
    pub fn apply_file_changes(
        &mut self,
        changes: &[(PathBuf, lsp_types::FileChangeType)],
        editor: &EditorBuffers,
    ) -> Result<Option<OverlayUpdate>, TsgoOverlayError> {
        let changes = changes
            .iter()
            .map(|(path, change)| (canonical_event_path(path), *change))
            .filter(|(path, _)| path.starts_with(&self.mirror_root))
            .collect::<Vec<_>>();
        // Decided before anything changes, so the fallback never drops an
        // update this pass already made.
        // A hidden or `node_modules` path holds no member (`walk_owned_files`).
        // A removed directory is gone, so the members beneath it are what tell
        // it from a file: its name can have a dot (`ui.v1`), and a client
        // reports a folder deletion as one event for the folder.
        let unplaceable = |path: &Path| {
            (path.is_dir() || path.extension().is_none() || self.holds_members_under(path))
                && !path
                    .strip_prefix(&self.mirror_root)
                    .unwrap_or(path)
                    .components()
                    .any(|component| {
                        let name = component.as_os_str().to_string_lossy();
                        name.starts_with('.') || name == "node_modules"
                    })
        };
        if changes.iter().any(|(path, _)| unplaceable(path)) {
            return Ok(None);
        }
        let mut update = OverlayUpdate::default();
        let mut scan = Vec::new();
        for (path, change) in changes {
            let Some(name) = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
            else {
                continue;
            };
            let component = name.ends_with(".svelte");
            if !component && !is_script_module(&name) {
                continue;
            }
            // The editor reports its own documents' edits, so a change on disk
            // never replaces one's shadow. Its imports and its ownership are
            // still this event's to update, from the buffer.
            let buffer = editor.get(&path);
            if change == lsp_types::FileChangeType::DELETED {
                if buffer.is_none()
                    && component
                    && self
                        .entries
                        .get(&path)
                        .is_some_and(ShadowState::is_component)
                {
                    if let Some(entry) = self.entries.get(&path) {
                        update.closed.push(entry.document.shadow_uri.clone());
                    }
                    // Still wanted: an import names it, so it is a copy again
                    // as soon as it comes back (an undo, a branch switch).
                    self.missing.insert(path.clone());
                    self.owned.remove(&path);
                    self.remove_source(&path)?;
                }
                continue;
            }
            let text = match buffer {
                Some(buffer) => buffer.text.to_string(),
                None => match fs::read_to_string(&path) {
                    Ok(text) => text,
                    Err(_) => continue,
                },
            };
            if component {
                let held = self
                    .entries
                    .get(&path)
                    .is_some_and(ShadowState::is_component);
                let owned = !held && self.owns_location(&path);
                let wanted = self.missing.remove(&path);
                if !(held || owned || wanted) {
                    continue;
                }
                if owned {
                    self.owned.insert(path.clone());
                }
                if let Some(buffer) = buffer {
                    if !held {
                        update.buffers.push(self.open_or_update(
                            &path,
                            &buffer.text,
                            buffer.version,
                        )?);
                    }
                } else if self
                    .entries
                    .get(&path)
                    .is_none_or(|entry| entry.source_text != text)
                {
                    let version = self
                        .entries
                        .get(&path)
                        .map_or(0, |entry| entry.document.version.saturating_add(1));
                    update
                        .opened_or_changed
                        .push(self.open_or_update(&path, &text, version)?);
                }
                scan.push((path, text));
            } else if self.owns_location(&path) {
                scan.push((path, text));
            }
        }
        self.follow_imports(scan, editor, &mut update)?;
        if !update.opened_or_changed.is_empty() || !update.buffers.is_empty() {
            self.link_node_modules();
        }
        Ok(Some(update))
    }

    /// Whether any member lives below `path` (not at it). Members are keyed
    /// by path, which orders a directory's descendants right after it.
    fn holds_members_under(&self, path: &Path) -> bool {
        self.entries
            .range::<Path, _>((Bound::Excluded(path), Bound::Unbounded))
            .next()
            .is_some_and(|(member, _)| member.starts_with(path))
    }

    /// Project, as copies, the components `sources` import that this
    /// project does not hold yet, transitively. A component the editor has
    /// open is copied from its buffer, which tsgo then holds as a buffer here
    /// too ([`OverlayUpdate::buffers`]); the rest are read from disk.
    fn follow_imports(
        &mut self,
        mut pending: Vec<(PathBuf, String)>,
        editor: &EditorBuffers,
        update: &mut OverlayUpdate,
    ) -> Result<(), TsgoOverlayError> {
        if pending.is_empty() {
            return Ok(());
        }
        let aliases = self.alias_patterns().to_vec();
        while let Some((path, text)) = pending.pop() {
            let Some(directory) = path.parent() else {
                continue;
            };
            let module = !path.to_string_lossy().ends_with(".svelte");
            for specifier in svelte_specifiers(&text) {
                for target in resolve_svelte_specifier(directory, specifier, &aliases) {
                    let target = fs::canonicalize(&target).unwrap_or(target);
                    if module && target.starts_with(&self.mirror_root) && target.is_file() {
                        self.module_imports.insert(target.clone());
                    }
                    if !target.starts_with(&self.mirror_root) || self.entries.contains_key(&target)
                    {
                        continue;
                    }
                    if let Some(buffer) = editor.get(&target) {
                        self.missing.remove(&target);
                        update.buffers.push(self.open_or_update(
                            &target,
                            &buffer.text,
                            buffer.version,
                        )?);
                        pending.push((target, buffer.text.to_string()));
                        continue;
                    }
                    if !target.is_file() {
                        self.missing.insert(target);
                        continue;
                    }
                    let Ok(text) = fs::read_to_string(&target) else {
                        continue;
                    };
                    update
                        .opened_or_changed
                        .push(self.open_or_update(&target, &text, 0)?);
                    pending.push((target, text));
                }
            }
        }
        Ok(())
    }

    /// Whether an editor document's text imports a component this loaded
    /// project does not hold yet but could: one on disk, or one `is_open`
    /// names. One lookup per `.svelte` specifier, so it can run on every
    /// keystroke; [`Self::follow_buffer_imports`] does the work.
    pub fn has_new_imports(
        &self,
        source_path: &Path,
        text: &str,
        is_open: impl Fn(&Path) -> bool,
    ) -> bool {
        if !self.loaded || !text.contains(".svelte") {
            return false;
        }
        let Some(directory) = source_path.parent() else {
            return false;
        };
        svelte_specifiers(text).into_iter().any(|specifier| {
            resolve_svelte_specifier(directory, specifier, self.alias_patterns())
                .iter()
                .any(|target| {
                    target.starts_with(&self.mirror_root)
                        && !self.entries.contains_key(target)
                        && (target.is_file() || is_open(target))
                        && fs::canonicalize(target)
                            .map_or(true, |canonical| !self.entries.contains_key(&canonical))
                })
        })
    }

    /// Copy the components an editor document's buffer imports that this
    /// project does not hold yet, transitively, as a saved file's imports are
    /// followed ([`Self::apply_file_changes`]): a new import resolves before
    /// the file is saved, as it does upstream, where module resolution reads
    /// the open documents.
    pub fn follow_buffer_imports(
        &mut self,
        source_path: &Path,
        text: &str,
        editor: &EditorBuffers,
    ) -> Result<OverlayUpdate, TsgoOverlayError> {
        let mut update = OverlayUpdate::default();
        self.follow_imports(
            vec![(source_path.to_path_buf(), text.to_string())],
            editor,
            &mut update,
        )?;
        if !update.opened_or_changed.is_empty() || !update.buffers.is_empty() {
            self.link_node_modules();
        }
        Ok(update)
    }

    /// Whether this project owns a file at `path` (canonical): inside its
    /// workspace, with no config, `node_modules` or hidden directory between
    /// (see `walk_owned_files`).
    fn owns_location(&self, path: &Path) -> bool {
        let Ok(relative) = path.strip_prefix(&self.workspace) else {
            return false;
        };
        let mut directory = self.workspace.clone();
        let mut components = relative.components().peekable();
        while let Some(component) = components.next() {
            let name = component.as_os_str().to_string_lossy();
            if name.starts_with('.') || name == "node_modules" {
                return false;
            }
            if components.peek().is_none() {
                return true;
            }
            directory.push(component);
            if directory.join("tsconfig.json").is_file()
                || directory.join("jsconfig.json").is_file()
            {
                return false;
            }
        }
        false
    }

    /// Every `.svelte` source this project needs: the ones it owns — its
    /// config is their nearest one, so a subdirectory with a config of its own
    /// is left to that project, as upstream keys a service by each document's
    /// nearest config (`service.ts:172-201`) — plus every component an owned
    /// component or owned plain module reaches through a relative or
    /// `paths`-aliased `.svelte` specifier, transitively, wherever it lives in
    /// the mirror root. tsgo resolves an import only inside this project's own
    /// shadow tree, so each of those needs a copy here.
    fn discover_sources(&mut self) -> BTreeSet<PathBuf> {
        let mut owned_components = Vec::new();
        let mut owned_modules = Vec::new();
        walk_owned_files(&self.workspace, &mut owned_components, &mut owned_modules);
        let mut discovered = owned_components
            .into_iter()
            .map(|path| fs::canonicalize(&path).unwrap_or(path))
            .collect::<BTreeSet<_>>();
        self.owned = discovered.clone();
        let aliases = self.alias_patterns();
        let mut missing = BTreeSet::new();
        let mut module_imports = BTreeSet::new();
        let mut pending = discovered.iter().cloned().collect::<Vec<_>>();
        pending.extend(owned_modules);
        while let Some(path) = pending.pop() {
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            if !text.contains(".svelte") {
                continue;
            }
            let Some(directory) = path.parent() else {
                continue;
            };
            let module = !path.to_string_lossy().ends_with(".svelte");
            for specifier in svelte_specifiers(&text) {
                for target in resolve_svelte_specifier(directory, specifier, aliases) {
                    let Ok(target) = fs::canonicalize(&target) else {
                        if target.starts_with(&self.mirror_root) {
                            missing.insert(target);
                        }
                        continue;
                    };
                    if !target.starts_with(&self.mirror_root) || !target.is_file() {
                        continue;
                    }
                    if module {
                        module_imports.insert(target.clone());
                    }
                    if discovered.insert(target.clone()) {
                        pending.push(target);
                    }
                }
            }
        }
        self.missing = missing;
        self.module_imports = module_imports;
        discovered
    }

    /// The JavaScript components an editor document can see the types of:
    /// those reachable from `roots` through `.svelte` imports among the
    /// components this project holds, plus every component an owned plain
    /// module imports (and what they reach). tsgo takes a buffer's script
    /// kind from its language id while it parses a body on disk by its
    /// `.tsx` name, losing JSDoc types, so these are opened as buffers; a
    /// JavaScript component nothing open reaches can stay on disk, where
    /// opening it would only cost a `didOpen` (a snapshot update in tsgo).
    #[must_use]
    pub fn reachable_javascript(
        &self,
        roots: impl IntoIterator<Item = PathBuf>,
    ) -> Vec<&ShadowDocument> {
        let mut pending = roots
            .into_iter()
            .chain(self.module_imports.iter().cloned())
            .collect::<Vec<_>>();
        let mut seen = BTreeSet::new();
        let mut reached = Vec::new();
        while let Some(path) = pending.pop() {
            if !seen.insert(path.clone()) {
                continue;
            }
            let Some(entry) = self.entries.get(&path) else {
                continue;
            };
            if entry.is_component() && entry.document.language_id == "javascriptreact" {
                reached.push(&entry.document);
            }
            if !entry.source_text.contains(".svelte") {
                continue;
            }
            let Some(directory) = path.parent() else {
                continue;
            };
            for specifier in svelte_specifiers(&entry.source_text) {
                for target in resolve_svelte_specifier(directory, specifier, self.alias_patterns())
                {
                    let target = if self.entries.contains_key(&target) {
                        target
                    } else {
                        fs::canonicalize(&target).unwrap_or(target)
                    };
                    if self.entries.contains_key(&target) && !seen.contains(&target) {
                        pending.push(target);
                    }
                }
            }
        }
        reached
    }

    /// The project's `paths` patterns, with each target absolute, plus the
    /// `*` pattern the overlay synthesizes from `baseUrl` (see
    /// [`Self::overlay_paths`]). Read once per overlay.
    fn alias_patterns(&self) -> &[AliasPattern] {
        self.aliases.get_or_init(|| self.read_alias_patterns())
    }

    fn read_alias_patterns(&self) -> Vec<AliasPattern> {
        let mut patterns = Vec::new();
        let Some(source) = self.source_tsconfig.as_deref() else {
            return patterns;
        };
        let config_dir = source.parent().unwrap_or_else(|| Path::new("."));
        if let Some((value, base)) = resolve_config_value_with_base(source, "paths") {
            for (pattern, targets) in value.as_object().into_iter().flatten() {
                let targets = targets
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(serde_json::Value::as_str)
                    .map(|target| rebase_config_spec(target, &base, config_dir))
                    .collect::<Vec<_>>();
                patterns.push(match pattern.split_once('*') {
                    Some((prefix, suffix)) => AliasPattern {
                        prefix: prefix.to_string(),
                        suffix: Some(suffix.to_string()),
                        targets,
                    },
                    None => AliasPattern {
                        prefix: pattern.clone(),
                        suffix: None,
                        targets,
                    },
                });
            }
        }
        if let Some(base_url) = self.overlay_base_url() {
            patterns.push(AliasPattern {
                prefix: String::new(),
                suffix: Some(String::new()),
                targets: vec![format!("{base_url}/*")],
            });
        }
        patterns
    }

    /// Resolve a source path to its virtual shadow document.
    #[must_use]
    pub fn shadow_for_source(&self, source_path: &Path) -> Option<&ShadowDocument> {
        let path = self.lookup_source_path(source_path);
        self.entries.get(&path).map(|entry| &entry.document)
    }

    /// URI a `.svelte` source would use after a prospective file rename.
    pub fn prospective_shadow_uri(&self, source_path: &Path) -> Result<Uri, TsgoOverlayError> {
        let source_path = self.confined_source(source_path)?;
        path_to_uri(&self.shadow_path_for(&source_path)?)
    }

    /// Resolve a virtual shadow path to its original source path.
    #[must_use]
    pub fn source_for_shadow(&self, shadow_path: &Path) -> Option<&Path> {
        let shadow_path = absolute_normalized(shadow_path);
        self.by_shadow.get(&shadow_path).map(PathBuf::as_path)
    }

    /// Every shadow that must be replayed after starting or restarting tsgo.
    #[must_use]
    pub fn eager_shadows(&self) -> Vec<&ShadowDocument> {
        self.entries
            .values()
            .filter(|entry| !entry.identity)
            .map(|entry| &entry.document)
            .collect()
    }

    /// Every open virtual document owned by this workspace.
    #[must_use]
    pub fn open_shadows(&self) -> Vec<&ShadowDocument> {
        self.entries.values().map(|entry| &entry.document).collect()
    }

    /// Exact mapping segments retained for one source projection.
    #[must_use]
    pub fn projection_map(&self, source_path: &Path) -> Option<&ProjectionMap> {
        let path = self.lookup_source_path(source_path);
        self.entries.get(&path).map(|entry| &entry.exact_map)
    }

    /// Original source text retained alongside one projection.
    #[must_use]
    pub fn source_text(&self, source_path: &Path) -> Option<&str> {
        let path = self.lookup_source_path(source_path);
        self.entries
            .get(&path)
            .map(|entry| entry.source_text.as_str())
    }

    /// Preprocessed Svelte text used as input to svelte2tsx.
    #[must_use]
    pub fn preprocessed_text(&self, source_path: &Path) -> Option<&str> {
        let path = self.lookup_source_path(source_path);
        self.entries.get(&path).map(|entry| {
            entry
                .preprocessed_text
                .as_deref()
                .unwrap_or(&entry.source_text)
        })
    }

    /// Standard v3 source map supplied by the preprocessor.
    #[must_use]
    pub fn preprocess_map(&self, source_path: &Path) -> Option<&str> {
        let path = self.lookup_source_path(source_path);
        self.entries
            .get(&path)
            .and_then(|entry| entry.preprocess_map.as_deref())
    }

    /// Mapping status of the preprocessed Svelte input.
    #[must_use]
    pub fn preprocess_status(&self, source_path: &Path) -> Option<PreprocessStatus> {
        let path = self.lookup_source_path(source_path);
        self.entries.get(&path).map(|entry| entry.preprocess_status)
    }

    /// Standard source map retained for rewritten-template position fallback.
    #[must_use]
    pub fn source_map(&self, source_path: &Path) -> Option<&str> {
        let path = self.lookup_source_path(source_path);
        self.entries
            .get(&path)
            .and_then(|entry| entry.source_map.as_deref())
    }

    /// Map an editor UTF-16 position to the tsgo UTF-8 shadow position.
    #[must_use]
    pub fn map_source_position(&self, source_path: &Path, position: Position) -> Option<Position> {
        let path = self.lookup_source_path(source_path);
        let entry = self.entries.get(&path)?;
        let source_offset = entry.source_index.offset(&entry.source_text, position);

        if entry.identity {
            let generated_offset = source_offset.saturating_sub(entry.fragment_offset)
                + entry
                    .plain_insertions
                    .iter()
                    .filter(|(source, _)| *source <= source_offset)
                    .map(|(_, generated)| generated.len())
                    .sum::<usize>();
            return Some(utf8_position(&entry.document.text, generated_offset));
        }

        if entry.preprocess_status == PreprocessStatus::Identity
            && let Some(generated) = exact_source_offset(&entry.exact_map, source_offset)
        {
            return Some(utf8_position(&entry.document.text, generated as usize));
        }

        let original_position = entry
            .source_index
            .position(&entry.source_text, source_offset);
        let preprocessed_position = map_original_to_preprocessed(entry, original_position)?;
        let preprocessed_text = entry
            .preprocessed_text
            .as_deref()
            .unwrap_or(&entry.source_text);
        let preprocessed_index = entry
            .preprocessed_index
            .as_ref()
            .unwrap_or(&entry.source_index);
        let preprocessed_position = preprocessed_index.position(
            preprocessed_text,
            preprocessed_index.offset(preprocessed_text, preprocessed_position),
        );
        let token = closest_source_token(&entry.tokens, preprocessed_position)?;
        let generated_offset = entry.generated_index.offset(
            &entry.document.text,
            Position::new(token.generated_line, token.generated_column),
        );
        Some(utf8_position(&entry.document.text, generated_offset))
    }

    /// Map an editor UTF-16 range to the tsgo UTF-8 shadow range.
    #[must_use]
    pub fn map_source_range(&self, source_path: &Path, range: Range) -> Option<Range> {
        let start = self.map_source_position(source_path, range.start)?;
        let end = self.map_source_position(source_path, range.end)?;
        Some(ordered_range(start, end))
    }

    /// Map a tsgo UTF-8 position back to the editor's UTF-16 source position.
    #[must_use]
    pub fn map_generated_position(
        &self,
        shadow_path: &Path,
        position: Position,
    ) -> Option<Position> {
        let source_path = self.source_for_shadow(shadow_path)?;
        let entry = self.entries.get(source_path)?;
        let generated_offset = utf8_offset(&entry.document.text, position);
        if entry.identity {
            if entry
                .plain_insertions
                .iter()
                .any(|(_, generated)| generated.contains(&generated_offset))
            {
                return None;
            }
            let source_offset = entry.fragment_offset + generated_offset
                - entry
                    .plain_insertions
                    .iter()
                    .filter(|(_, generated)| generated.end <= generated_offset)
                    .map(|(_, generated)| generated.len())
                    .sum::<usize>();
            return Some(
                entry
                    .source_index
                    .position(&entry.source_text, source_offset),
            );
        }
        if entry
            .generated_ranges
            .iter()
            .chain(&entry.import_suffix_ranges)
            .any(|range| range.contains(&generated_offset))
        {
            return None;
        }

        if entry.preprocess_status == PreprocessStatus::Identity
            && let Some(source) = exact_generated_offset(&entry.exact_map, generated_offset)
        {
            return Some(
                entry
                    .source_index
                    .position(&entry.source_text, source as usize),
            );
        }

        let generated_utf16 = entry
            .generated_index
            .position(&entry.document.text, generated_offset);
        let index = entry.tokens.partition_point(|token| {
            (token.generated_line, token.generated_column)
                <= (generated_utf16.line, generated_utf16.character)
        });
        let previous = index.checked_sub(1).and_then(|i| entry.tokens.get(i))?;
        if previous.generated_line != generated_utf16.line
            || generated_is_unmapped_gap(&entry.tokens, index, generated_utf16)
        {
            return None;
        }
        let (line, column) = previous.source?;
        map_preprocessed_to_original(entry, Position::new(line, column))
    }

    /// Map a tsgo UTF-8 range back to the editor's UTF-16 source range.
    #[must_use]
    pub fn map_generated_range(&self, shadow_path: &Path, range: Range) -> Option<Range> {
        if self.is_generated_range(shadow_path, range) {
            return None;
        }
        let start = self.map_generated_position(shadow_path, range.start)?;
        let end = self.map_generated_position(shadow_path, range.end)?;
        Some(ordered_range(start, end))
    }

    /// Whether a tsgo position falls inside an `Ωignore` generated-code region.
    #[must_use]
    pub fn is_generated_position(&self, shadow_path: &Path, position: Position) -> bool {
        let Some(source_path) = self.source_for_shadow(shadow_path) else {
            return false;
        };
        let Some(entry) = self.entries.get(source_path) else {
            return false;
        };
        let offset = utf8_offset(&entry.document.text, position);
        entry
            .generated_ranges
            .iter()
            .chain(&entry.import_suffix_ranges)
            .any(|range| range.contains(&offset))
    }

    /// Whether the shadow at `shadow_path` is svelte2tsx output rather than the
    /// user's own file. An `identity` entry's `function $$render()` would be a
    /// declaration someone wrote, not the generated header.
    #[must_use]
    pub fn is_projected_shadow(&self, shadow_path: &Path) -> bool {
        self.source_for_shadow(shadow_path)
            .and_then(|source_path| self.entries.get(source_path))
            .is_some_and(|entry| !entry.identity)
    }

    /// The generated shadow's text, for the filters that scan or parse it.
    #[must_use]
    pub fn shadow_text(&self, shadow_path: &Path) -> Option<&str> {
        let source_path = self.source_for_shadow(shadow_path)?;
        Some(self.entries.get(source_path)?.document.text.as_str())
    }

    /// `convertToTargetTextSpan` (`InlayHintProvider.ts:104-113`): the two
    /// endpoints are mapped independently and an unmappable one becomes offset 0
    /// (start) or the snapshot's length (end), so the span always exists. An
    /// editor asks for the visible viewport, which starts at `0:0` — a position
    /// whose shadow lands in the prologue (#4464).
    #[must_use]
    pub fn clamp_source_range(&self, source_path: &Path, range: Range) -> Option<Range> {
        let path = self.lookup_source_path(source_path);
        let entry = self.entries.get(&path)?;
        let start = self
            .map_source_position(source_path, range.start)
            .unwrap_or_else(|| Position::new(0, 0));
        let end = self
            .map_source_position(source_path, range.end)
            .unwrap_or_else(|| utf8_position(&entry.document.text, entry.document.text.len()));
        Some(ordered_range(start, end))
    }

    /// A shadow position as a byte offset into that text.
    #[must_use]
    pub fn shadow_offset(&self, shadow_path: &Path, position: Position) -> Option<usize> {
        let source_path = self.source_for_shadow(shadow_path)?;
        let entry = self.entries.get(source_path)?;
        Some(utf8_offset(&entry.document.text, position))
    }

    /// Whether a tsgo range is generated text, which is what decides that it
    /// maps back to nothing. An `Ωignore` region makes any range it intersects
    /// generated; the appended `.tsx` does not, because the range around it has
    /// source of its own — `'./X.svelte.tsx'` is the source specifier.
    #[must_use]
    pub fn is_generated_range(&self, shadow_path: &Path, range: Range) -> bool {
        self.generated_range_span(shadow_path, range, false)
    }

    /// Whether a tsgo range *touches* generated text. The question a request
    /// asks, where `is_generated_range` is the question a response asks: an
    /// editor range that runs through the appended `.tsx` covers bytes with no
    /// source, so the request is dropped rather than answered about them.
    #[must_use]
    pub fn touches_generated_range(&self, shadow_path: &Path, range: Range) -> bool {
        self.generated_range_span(shadow_path, range, true)
    }

    fn generated_range_span(&self, shadow_path: &Path, range: Range, touching: bool) -> bool {
        let Some(source_path) = self.source_for_shadow(shadow_path) else {
            return false;
        };
        let Some(entry) = self.entries.get(source_path) else {
            return false;
        };
        let start = utf8_offset(&entry.document.text, range.start);
        let end = utf8_offset(&entry.document.text, range.end).max(start);
        if entry
            .generated_ranges
            .iter()
            .any(|generated| generated.start < end && start < generated.end)
        {
            return true;
        }
        entry.import_suffix_ranges.iter().any(|suffix| {
            if touching {
                suffix.start < end && start < suffix.end
            } else {
                suffix.start <= start && end <= suffix.end
            }
        })
    }

    /// Resolution integrity for every eager shadow.
    #[must_use]
    pub fn resolution_info(&self) -> Vec<ShadowResolutionInfo> {
        self.entries
            .values()
            .filter(|entry| !entry.identity)
            .map(|entry| ShadowResolutionInfo {
                source_path: entry.source_path.clone(),
                shadow_path: entry.shadow_path.clone(),
                source_in_workspace: entry.source_path.starts_with(&self.mirror_root),
                shadow_in_cache: entry.shadow_path.starts_with(&self.shadow_dir),
                parent_directory_exists: entry.shadow_path.parent().is_some_and(Path::is_dir),
                shadow_registered: self
                    .by_shadow
                    .get(&entry.shadow_path)
                    .is_some_and(|source| source == &entry.source_path),
                included_by_overlay_config: entry
                    .shadow_path
                    .strip_prefix(&self.shadow_dir)
                    .is_ok_and(|relative| {
                        relative
                            .as_os_str()
                            .to_string_lossy()
                            .ends_with(".svelte.tsx")
                    }),
                body_exists_on_disk: entry.shadow_path.is_file(),
            })
            .collect()
    }

    /// Routes that would be vulnerable to silent ambient-any degradation.
    #[must_use]
    pub fn unresolved_shadow_routes(&self) -> Vec<ShadowResolutionInfo> {
        self.resolution_info()
            .into_iter()
            .filter(|info| !info.is_resolvable())
            .collect()
    }

    /// Every component shadow this overlay holds — projections and
    /// parser-error fallbacks, not plain modules routed through the project.
    #[must_use]
    pub fn component_shadows(&self) -> Vec<&ShadowDocument> {
        self.entries
            .values()
            .filter(|entry| entry.is_component())
            .map(|entry| &entry.document)
            .collect()
    }

    /// Write `source_path`'s component shadow where the overlay config's
    /// include finds it, atomically (a sibling temporary file the include
    /// ignores, then a rename). Returns the change tsgo has to be told about
    /// through `workspace/didChangeWatchedFiles`, or `None` when the file
    /// already holds exactly this text or the route is a plain module.
    ///
    /// The caller decides *which* text is persisted: an editor buffer's
    /// projection belongs in tsgo's in-memory overlay, never on disk.
    pub fn persist_body(
        &self,
        source_path: &Path,
    ) -> Result<Option<lsp_types::FileChangeType>, TsgoOverlayError> {
        let path = self.lookup_source_path(source_path);
        let Some(entry) = self.entries.get(&path).filter(|entry| entry.is_component()) else {
            return Ok(None);
        };
        write_body(&entry.shadow_path, &self.cache_dir, &entry.document.text)
    }

    /// Delete every body in the shadow tree that `keep` rejects — a source
    /// deleted or renamed while no server ran, or a route this overlay no
    /// longer serves — along with any temporary file an interrupted write left
    /// behind. Otherwise the include glob would keep a ghost component in the
    /// program. Returns the shadow paths removed.
    pub fn sweep_bodies(&self, keep: impl Fn(&Path) -> bool) -> Vec<PathBuf> {
        let mut removed = Vec::new();
        let mut pending = vec![self.shadow_dir.clone()];
        while let Some(directory) = pending.pop() {
            let Ok(entries) = fs::read_dir(&directory) else {
                continue;
            };
            for entry in entries.flatten() {
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                let path = entry.path();
                if file_type.is_dir() {
                    pending.push(path);
                    continue;
                }
                let name = entry.file_name();
                let name = name.to_string_lossy();
                let stale = if name.ends_with(BODY_TEMP_SUFFIX) {
                    true
                } else if name.ends_with(".svelte.tsx") {
                    !(self
                        .by_shadow
                        .get(&path)
                        .and_then(|source| self.entries.get(source))
                        .is_some_and(ShadowState::is_component)
                        && keep(&path))
                } else {
                    false
                };
                if stale && fs::remove_file(&path).is_ok() && !name.ends_with(BODY_TEMP_SUFFIX) {
                    removed.push(path);
                }
            }
        }
        removed
    }

    fn projection_options(
        &self,
        source_path: &Path,
        shadow_path: &Path,
        source: &str,
    ) -> Svelte2TsxOptions {
        Svelte2TsxOptions {
            filename: source_path.display().to_string(),
            is_ts_file: is_typescript_component(source),
            mode: Svelte2TsxMode::Ts,
            accessors: self.accessors,
            namespace: self.namespace,
            version: SvelteVersion::V5,
            runes: None,
            emit_jsdoc: true,
            // `LSAndTSDocResolver.ts:138`: the language server projects a
            // half-typed document; `svelte-check` deliberately does not.
            emit_on_template_error: true,
            rewrite_external_imports: Some(RewriteExternalImportsOptions {
                source_path: source_path.display().to_string(),
                generated_path: shadow_path.display().to_string(),
                workspace_path: self.workspace.display().to_string(),
            }),
            ..Svelte2TsxOptions::default()
        }
    }

    fn materialize_support_files(&self) -> Result<(), TsgoOverlayError> {
        reject_symlink_components(&self.cache_dir, &self.cache_dir)?;
        for (name, contents) in SHIM_FILES {
            let path = self.cache_dir.join(name);
            reject_symlink_components(&path, &self.cache_dir)?;
            write_if_changed(&path, contents)?;
        }
        self.write_tsconfig()
    }

    /// tsgo resolves `paths` against the config that declares them, so an
    /// inherited alias points into the source tree — where a component's
    /// `.svelte.tsx` shadow does not exist, because it is served from memory
    /// under `shadow_dir`. `rootDirs` covers relative resolution only, so every
    /// mapping needs its shadow-tree twin spelled out beside the original.
    fn overlay_paths(&self) -> Option<serde_json::Value> {
        let mut mappings = serde_json::Map::new();
        // The user's own patterns are written after these, so a project that
        // maps `svelte` itself still wins.
        for (module, target) in self.svelte_fallback.iter().flat_map(|fallback| {
            fallback
                .modules
                .iter()
                .map(move |m| (m, &fallback.declarations))
        }) {
            mappings.insert(module.clone(), json!([path_for_tsconfig(target)]));
        }
        let Some(source) = self.source_tsconfig.as_deref() else {
            return (!mappings.is_empty()).then_some(serde_json::Value::Object(mappings));
        };
        let config_dir = source.parent().unwrap_or_else(|| Path::new("."));
        // tsgo (TypeScript 7) has REMOVED `baseUrl`, and its own error names the
        // replacement: `"paths": {"*": ["./*"]}`. TypeScript 5.x, which
        // `svelte-language-server` runs, still honours it, so without this every
        // non-relative import a project roots at `baseUrl` loses its type here
        // and keeps it there. The overlay's `paths` is a total override, which
        // is the only place the translation can be written: a tsconfig cannot
        // unset an option it inherits through `extends`. A project that declares
        // `*` itself is left alone — the loop below overwrites this entry.
        if let Some(base_url) = self.overlay_base_url() {
            mappings.insert(
                "*".to_string(),
                json!([
                    format!("{base_url}/*"),
                    format!("{}/*", path_for_tsconfig(&self.shadow_dir))
                ]),
            );
        }
        let Some((value, base)) = resolve_config_value_with_base(source, "paths") else {
            return (!mappings.is_empty()).then_some(serde_json::Value::Object(mappings));
        };
        for (pattern, targets) in value.as_object().into_iter().flatten() {
            let mut candidates = Vec::new();
            for target in targets
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(serde_json::Value::as_str)
            {
                let absolute = rebase_config_spec(target, &base, config_dir);
                // The source target stays first, so nothing that resolves today
                // can start resolving somewhere else.
                candidates.push(json!(absolute));
                if let Ok(relative) = Path::new(&absolute).strip_prefix(&self.mirror_root) {
                    candidates.push(json!(path_for_tsconfig(&self.shadow_dir.join(relative))));
                }
            }
            // The override is total, so a pattern this loop cannot rewrite is a
            // pattern the project would lose; pass an unrecognised shape through.
            mappings.insert(
                pattern.clone(),
                if candidates.is_empty() {
                    targets.clone()
                } else {
                    serde_json::Value::Array(candidates)
                },
            );
        }
        (!mappings.is_empty()).then(|| serde_json::Value::Object(mappings))
    }

    /// The project's `baseUrl` as an absolute path, when it declares one.
    fn overlay_base_url(&self) -> Option<String> {
        let source = self.source_tsconfig.as_deref()?;
        let config_dir = source.parent().unwrap_or_else(|| Path::new("."));
        let (value, base) = resolve_config_value_with_base(source, "baseUrl")?;
        Some(rebase_config_spec(value.as_str()?, &base, config_dir))
    }

    fn write_tsconfig(&self) -> Result<(), TsgoOverlayError> {
        let specs = self
            .source_tsconfig
            .as_deref()
            .map(read_tsconfig_specs)
            .unwrap_or_default();
        let mut include = vec!["svelte/**/*".to_string()];
        if let Some(user_include) = specs.include {
            include.extend(user_include);
        } else if specs.files.is_none()
            && let Some(root) = self.source_tsconfig.as_deref().and_then(Path::parent)
        {
            // With no project config upstream builds its fallback with
            // `include: []` (`service.ts:874-878`, "not to flood the initial
            // files"), so a workspace glob here would put every `.d.ts` in the
            // repository — and its `declare global`s — into the program.
            include.push(format!("{}/**/*", path_for_tsconfig(root)));
        }
        let (mut shims, mut svelte_html) = global_type_files(&self.workspace);
        if let Some(fallback) = &self.svelte_fallback {
            svelte_html = fallback.svelte_html.clone();
            if svelte_html.is_some() {
                shims.retain(|name| *name != SHIM_JSX_V4_NAME);
            }
        }
        let mut files = shims
            .iter()
            .map(|name| (*name).to_string())
            .collect::<Vec<_>>();
        files.extend(svelte_html.as_deref().map(path_for_tsconfig));
        files.extend(
            self.svelte_fallback
                .as_ref()
                .map(|fallback| path_for_tsconfig(&fallback.declarations)),
        );
        files.extend(specs.files.unwrap_or_default());
        let mut config = json!({
            "compilerOptions": {
                "allowArbitraryExtensions": true,
                "allowImportingTsExtensions": true,
                "jsx": "preserve",
                "noEmit": true,
                "rootDirs": [
                    path_for_tsconfig(&self.mirror_root),
                    path_for_tsconfig(&self.shadow_dir)
                ]
            },
            "files": files,
            "include": include
        });
        if let Some(target) = overlay_target(self.source_tsconfig.as_deref()) {
            config["compilerOptions"]["target"] = json!(target);
        }
        if let Some(paths) = self.overlay_paths() {
            config["compilerOptions"]["paths"] = paths;
        }
        if let Some(source) = &self.source_tsconfig {
            config["extends"] = json!(path_for_tsconfig(source));
        }
        if let Some(exclude) = specs.exclude {
            config["exclude"] = json!(exclude);
        }
        let mut text = serde_json::to_string_pretty(&config).expect("JSON values are serializable");
        text.push('\n');
        reject_symlink_components(&self.tsconfig_path, &self.cache_dir)?;
        write_if_changed(&self.tsconfig_path, &text)
    }

    fn shadow_path_for(&self, source_path: &Path) -> Result<PathBuf, TsgoOverlayError> {
        let relative = source_path.strip_prefix(&self.mirror_root).map_err(|_| {
            TsgoOverlayError::InvalidSource {
                path: source_path.to_path_buf(),
                reason: "source escapes the workspace",
            }
        })?;
        if relative.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        }) {
            return Err(TsgoOverlayError::InvalidSource {
                path: source_path.to_path_buf(),
                reason: "source contains an escaping path component",
            });
        }
        let mut shadow = self.shadow_dir.join(relative).into_os_string();
        shadow.push(".tsx");
        Ok(PathBuf::from(shadow))
    }

    fn ensure_shadow_parent(&self, shadow_path: &Path) -> Result<(), TsgoOverlayError> {
        let parent = shadow_path
            .parent()
            .ok_or_else(|| TsgoOverlayError::InvalidSource {
                path: shadow_path.to_path_buf(),
                reason: "shadow has no parent directory",
            })?;
        // A dependency's file under a `node_modules` this overlay mirrors
        // ([`Self::link_node_modules`]): the shadow needs a real directory
        // there, so the mirror link (this overlay's own) gives way to it.
        if let Ok(relative) = parent.strip_prefix(&self.shadow_dir) {
            let mut current = self.shadow_dir.clone();
            for component in relative.components() {
                current.push(component);
                if component.as_os_str() == "node_modules"
                    && fs::symlink_metadata(&current)
                        .is_ok_and(|metadata| metadata.file_type().is_symlink())
                {
                    fs::remove_file(&current)?;
                }
            }
        }
        reject_symlink_components(parent, &self.cache_dir)?;
        fs::create_dir_all(parent)?;
        reject_symlink_components(parent, &self.cache_dir)?;
        reject_symlink_components(shadow_path, &self.cache_dir)?;
        Ok(())
    }

    fn confined_source(&self, source_path: &Path) -> Result<PathBuf, TsgoOverlayError> {
        let lexical = if source_path.is_absolute() {
            absolute_normalized(source_path)
        } else {
            absolute_normalized(&self.workspace.join(source_path))
        };
        if lexical
            .extension()
            .is_none_or(|extension| extension != "svelte")
        {
            return Err(TsgoOverlayError::InvalidSource {
                path: lexical,
                reason: "source does not end in .svelte",
            });
        }
        let existing = nearest_existing_path(&lexical)?;
        let real_existing = fs::canonicalize(existing)?;
        let suffix = lexical
            .strip_prefix(existing)
            .expect("nearest existing path is an ancestor");
        let real = join_existing_suffix(real_existing, suffix);
        if !real.starts_with(&self.mirror_root) {
            return Err(TsgoOverlayError::InvalidSource {
                path: lexical,
                reason: "source is redirected outside the workspace by a symlink",
            });
        }
        Ok(fs::canonicalize(&real).unwrap_or(real))
    }

    fn confined_any_source(&self, source_path: &Path) -> Result<PathBuf, TsgoOverlayError> {
        let lexical = if source_path.is_absolute() {
            absolute_normalized(source_path)
        } else {
            absolute_normalized(&self.workspace.join(source_path))
        };
        let existing = nearest_existing_path(&lexical)?;
        let real_existing = fs::canonicalize(existing)?;
        let suffix = lexical
            .strip_prefix(existing)
            .expect("nearest existing path is an ancestor");
        let real = join_existing_suffix(real_existing, suffix);
        if !real.starts_with(&self.mirror_root) {
            return Err(TsgoOverlayError::InvalidSource {
                path: lexical,
                reason: "source is redirected outside the workspace by a symlink",
            });
        }
        Ok(fs::canonicalize(&real).unwrap_or(real))
    }

    fn lookup_source_path(&self, source_path: &Path) -> PathBuf {
        let path = if source_path.is_absolute() {
            absolute_normalized(source_path)
        } else {
            absolute_normalized(&self.workspace.join(source_path))
        };
        fs::canonicalize(&path).unwrap_or(path)
    }

    fn remove_source(&mut self, source_path: &Path) -> Result<(), TsgoOverlayError> {
        let Some(entry) = self.entries.remove(source_path) else {
            return Ok(());
        };
        self.by_shadow.remove(&entry.shadow_path);
        if entry.is_component() {
            reject_symlink_components(&entry.shadow_path, &self.cache_dir)?;
            match fs::remove_file(&entry.shadow_path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        self.prune_empty_skeleton(entry.shadow_path.parent())
    }

    fn prune_empty_skeleton(&self, mut current: Option<&Path>) -> Result<(), TsgoOverlayError> {
        while let Some(directory) = current {
            if directory == self.shadow_dir {
                break;
            }
            reject_symlink_components(directory, &self.cache_dir)?;
            match fs::remove_dir(directory) {
                Ok(()) => current = directory.parent(),
                Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => break,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    current = directory.parent();
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }
}

/// A project the language server can load: its config, or `None` for a
/// workspace folder without one, and the workspace folder its shadows mirror.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProjectSpec {
    pub config: Option<PathBuf>,
    pub mirror_root: PathBuf,
}

impl ProjectSpec {
    /// Whether this is the workspace folder's own project rather than a
    /// nested config's.
    #[must_use]
    pub fn is_folder(&self) -> bool {
        self.config
            .as_deref()
            .is_none_or(|config| config.parent() == Some(self.mirror_root.as_path()))
    }

    /// Whether both name the same project.
    #[must_use]
    pub fn same_project(&self, other: &Self) -> bool {
        self.mirror_root == other.mirror_root
            && if self.is_folder() {
                other.is_folder()
            } else {
                self.config == other.config
            }
    }
}

/// Every project in the workspace folder `root` (canonical) that owns a
/// component: the nearest `tsconfig.json`, then `jsconfig.json`, of each
/// `.svelte` file, or the folder's own project for a file with none — one
/// walk that skips `node_modules`, hidden directories and symlinks, as
/// discovery does. These are the projects the eager design loaded; one that
/// owns no component (tests that import the app through an alias) loads when
/// a document it owns opens.
#[must_use]
pub fn component_projects(root: &Path) -> Vec<ProjectSpec> {
    fn walk(directory: &Path, config: Option<&Path>, found: &mut BTreeSet<Option<PathBuf>>) {
        let own = ["tsconfig.json", "jsconfig.json"]
            .into_iter()
            .map(|name| directory.join(name))
            .find(|path| path.is_file());
        let config = own.as_deref().or(config);
        let Ok(entries) = fs::read_dir(directory) else {
            return;
        };
        let mut owns_component = false;
        let mut subdirectories = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || name == "node_modules" {
                continue;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                subdirectories.push(entry.path());
            } else if file_type.is_file() && name.ends_with(".svelte") {
                owns_component = true;
            }
        }
        if owns_component {
            found.insert(config.map(Path::to_path_buf));
        }
        for subdirectory in subdirectories {
            walk(&subdirectory, config, found);
        }
    }
    let mut found = BTreeSet::new();
    walk(root, None, &mut found);
    found
        .into_iter()
        .map(|config| ProjectSpec {
            config,
            mirror_root: root.to_path_buf(),
        })
        .collect()
}

/// Where `require.resolve('svelte/package.json', { paths: [__dirname] })`
/// looks from: the directory holding the running server
/// (`importPackage.ts:32`). `@rsvelte/language-server` declares `svelte` for
/// this reason, exactly as `svelte-language-server` does — and the launcher
/// passes its own package root, because the native binary is installed in a
/// sibling platform package that cannot see it.
fn server_svelte_root() -> Option<PathBuf> {
    if let Some(root) = std::env::var_os("RSVELTE_LANGUAGE_SERVER_ROOT") {
        return Some(PathBuf::from(root));
    }
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.to_path_buf())
}

/// The project config that owns `source`: the nearest `tsconfig.json` — then
/// `jsconfig.json` — at or above its directory, without leaving `boundary` and
/// without crossing a `node_modules`. This is `findTsConfigPath`
/// (`plugins/typescript/utils.ts:146-167`), which resolves a project **per
/// document** rather than once per workspace root.
#[must_use]
pub fn nearest_tsconfig(source: &Path, boundary: &Path) -> Option<PathBuf> {
    let mut cursor = source.parent();
    while let Some(dir) = cursor {
        if !dir.starts_with(boundary) {
            return None;
        }
        if dir.file_name().is_some_and(|name| name == "node_modules") {
            return None;
        }
        for name in ["tsconfig.json", "jsconfig.json"] {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        if dir == boundary {
            return None;
        }
        cursor = dir.parent();
    }
    None
}

fn resolve_tsconfig(workspace: &Path, explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(explicit) = explicit {
        let path = if explicit.is_absolute() {
            explicit.to_path_buf()
        } else {
            workspace.join(explicit)
        };
        return Some(absolute_normalized(&path));
    }
    ["tsconfig.json", "jsconfig.json"]
        .into_iter()
        .map(|name| workspace.join(name))
        .find(|path| path.is_file())
}

#[derive(Debug, Default)]
struct InheritedConfigSpecs {
    include: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
    files: Option<Vec<String>>,
}

/// The `target` the shadow program must be given, or `None` to keep the
/// project's. Mirrors `service.ts:792-795`, where upstream forces
/// `ScriptTarget.Latest` when the project sets no target and raises anything
/// below ES2015 to ES2015 — without it a shadow program is checked against a
/// smaller lib than the editor's own program uses.
fn overlay_target(tsconfig: Option<&Path>) -> Option<&'static str> {
    match tsconfig.and_then(|path| resolve_compiler_option(path, "target")) {
        None => Some("ESNext"),
        Some(target) => {
            matches!(target.to_ascii_lowercase().as_str(), "es3" | "es5").then_some("ES2015")
        }
    }
}

fn resolve_compiler_option(tsconfig: &Path, key: &str) -> Option<String> {
    resolve_config_value(tsconfig, key).and_then(|value| match value {
        serde_json::Value::String(text) => Some(text),
        _ => None,
    })
}

fn read_tsconfig_specs(tsconfig: &Path) -> InheritedConfigSpecs {
    let config_dir = tsconfig.parent().unwrap_or_else(|| Path::new("."));
    let rebase = |key: &str| {
        resolve_config_specs(tsconfig, key).map(|(specs, base)| {
            specs
                .into_iter()
                .filter(|spec| key != "files" || !spec.ends_with(".svelte"))
                .map(|spec| rebase_config_spec(&spec, &base, config_dir))
                .collect()
        })
    };
    InheritedConfigSpecs {
        include: rebase("include"),
        exclude: rebase("exclude"),
        files: rebase("files"),
    }
}

const MAX_EXTENDS_CONFIGS: usize = 32;

fn resolve_config_specs(tsconfig: &Path, key: &str) -> Option<(Vec<String>, PathBuf)> {
    let mut pending = vec![absolute_normalized(tsconfig)];
    let mut seen = BTreeSet::new();
    let mut visited = 0usize;
    while let Some(path) = pending.pop() {
        if visited == MAX_EXTENDS_CONFIGS || !seen.insert(path.clone()) {
            continue;
        }
        visited += 1;
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let Some(parsed) = parse_jsonc(&raw) else {
            continue;
        };
        let base = path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        if let Some(specs) = parsed.get(key).and_then(serde_json::Value::as_array) {
            return Some((
                specs
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_string)
                    .collect(),
                base,
            ));
        }
        let parents = match parsed.get("extends") {
            Some(serde_json::Value::String(parent)) => vec![parent.as_str()],
            Some(serde_json::Value::Array(parents)) => parents
                .iter()
                .filter_map(serde_json::Value::as_str)
                .collect(),
            _ => Vec::new(),
        };
        pending.extend(
            parents
                .into_iter()
                .filter_map(|parent| resolve_extends_target(&base, parent)),
        );
    }
    None
}

fn resolve_config_value(tsconfig: &Path, key: &str) -> Option<serde_json::Value> {
    resolve_config_value_with_base(tsconfig, key).map(|(value, _)| value)
}

/// The declaring config's directory comes back too: a relative `paths` target
/// is resolved against the config that spelled it, not against the one that
/// inherits it.
fn resolve_config_value_with_base(
    tsconfig: &Path,
    key: &str,
) -> Option<(serde_json::Value, PathBuf)> {
    let mut pending = vec![absolute_normalized(tsconfig)];
    let mut seen = BTreeSet::new();
    let mut visited = 0usize;
    while let Some(path) = pending.pop() {
        if visited == MAX_EXTENDS_CONFIGS || !seen.insert(path.clone()) {
            continue;
        }
        visited += 1;
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let Some(parsed) = parse_jsonc(&raw) else {
            continue;
        };
        let base = path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        if let Some(value) = parsed
            .get("compilerOptions")
            .and_then(|options| options.get(key))
        {
            return Some((value.clone(), base));
        }
        let parents = match parsed.get("extends") {
            Some(serde_json::Value::String(parent)) => vec![parent.as_str()],
            Some(serde_json::Value::Array(parents)) => parents
                .iter()
                .filter_map(serde_json::Value::as_str)
                .collect(),
            _ => Vec::new(),
        };
        pending.extend(
            parents
                .into_iter()
                .filter_map(|parent| resolve_extends_target(&base, parent)),
        );
    }
    None
}

fn resolve_extends_target(base: &Path, specifier: &str) -> Option<PathBuf> {
    if specifier.starts_with('.') || Path::new(specifier).is_absolute() {
        return Some(resolve_extends_path(base, specifier));
    }
    let mut cursor = Some(absolute_normalized(base));
    while let Some(directory) = cursor {
        let candidate = directory.join("node_modules").join(specifier);
        for path in config_path_candidates(candidate) {
            if path.is_file() {
                return Some(path);
            }
        }
        cursor = directory.parent().map(Path::to_path_buf);
    }
    None
}

fn resolve_extends_path(base: &Path, specifier: &str) -> PathBuf {
    config_path_candidates(base.join(specifier))
        .into_iter()
        .find(|path| path.exists())
        .unwrap_or_else(|| base.join(specifier))
}

fn config_path_candidates(path: PathBuf) -> Vec<PathBuf> {
    if path.is_dir() {
        return vec![path.join("tsconfig.json")];
    }
    if path.extension().is_none() {
        let mut json = path.clone();
        json.set_extension("json");
        vec![path.join("tsconfig.json"), json, path]
    } else {
        vec![path]
    }
}

fn rebase_config_spec(spec: &str, base: &Path, config_dir: &Path) -> String {
    let project_dir = path_for_tsconfig(config_dir);
    let substituted = spec.replace("${configDir}", &project_dir);
    if Path::new(&substituted).is_absolute() {
        substituted.replace('\\', "/")
    } else {
        path_for_tsconfig(&absolute_normalized(&base.join(substituted)))
    }
}

fn parse_jsonc(source: &str) -> Option<serde_json::Value> {
    serde_json::from_str(&strip_jsonc(&strip_trailing_jsonc_commas(source))).ok()
}

fn strip_trailing_jsonc_commas(source: &str) -> String {
    let without_comments = strip_jsonc(source);
    let bytes = without_comments.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            output.push(byte);
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        if byte == b'"' {
            in_string = true;
            output.push(byte);
            index += 1;
            continue;
        }
        if byte == b',' {
            let mut next = index + 1;
            while bytes.get(next).is_some_and(u8::is_ascii_whitespace) {
                next += 1;
            }
            if bytes
                .get(next)
                .is_some_and(|next| matches!(next, b'}' | b']'))
            {
                index += 1;
                continue;
            }
        }
        output.push(byte);
        index += 1;
    }
    String::from_utf8(output).ok().unwrap_or_default()
}

fn strip_jsonc(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            output.push(byte);
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        if byte == b'"' {
            in_string = true;
            output.push(byte);
            index += 1;
            continue;
        }
        if byte == b'/' && index + 1 < bytes.len() {
            match bytes[index + 1] {
                b'/' => {
                    while index < bytes.len() && bytes[index] != b'\n' {
                        index += 1;
                    }
                    continue;
                }
                b'*' => {
                    index += 2;
                    while index + 1 < bytes.len()
                        && !(bytes[index] == b'*' && bytes[index + 1] == b'/')
                    {
                        index += 1;
                    }
                    index = (index + 2).min(bytes.len());
                    continue;
                }
                _ => {}
            }
        }
        output.push(byte);
        index += 1;
    }
    String::from_utf8(output).ok().unwrap_or_default()
}

fn parse_mapping_tokens(
    source_path: &Path,
    raw: Option<&str>,
) -> Result<Vec<MappingToken>, TsgoOverlayError> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    let map = SourceMap::from_slice(raw.as_bytes()).map_err(|error| {
        TsgoOverlayError::InvalidSourceMap {
            path: source_path.to_path_buf(),
            message: error.to_string(),
        }
    })?;
    let mut tokens = map
        .tokens()
        .map(|token| MappingToken {
            generated_line: token.get_dst_line(),
            generated_column: token.get_dst_col(),
            source: token
                .get_source()
                .map(|_| (token.get_src_line(), token.get_src_col())),
        })
        .collect::<Vec<_>>();
    tokens.sort_by_key(|token| (token.generated_line, token.generated_column));
    Ok(tokens)
}

fn parse_preprocess_mappings(
    source_path: &Path,
    raw: &str,
) -> Result<PreprocessMappings, TsgoOverlayError> {
    let map = SourceMap::from_slice(raw.as_bytes()).map_err(|error| {
        TsgoOverlayError::InvalidSourceMap {
            path: source_path.to_path_buf(),
            message: error.to_string(),
        }
    })?;
    let single_source = map.get_source_count() == 1;
    let mut mappings = PreprocessMappings::default();
    for token in map.tokens() {
        let belongs_to_document = token
            .get_source()
            .is_some_and(|source| single_source || source_matches(source_path, source));
        let source = belongs_to_document.then(|| (token.get_src_line(), token.get_src_col()));
        mappings.generated.push(MappingToken {
            generated_line: token.get_dst_line(),
            generated_column: token.get_dst_col(),
            source,
        });
        if let Some((source_line, source_column)) = source {
            mappings.original.push(ReverseMappingToken {
                source_line,
                source_column,
                generated_line: token.get_dst_line(),
                generated_column: token.get_dst_col(),
            });
        }
    }
    mappings
        .generated
        .sort_by_key(|token| (token.generated_line, token.generated_column));
    mappings.original.sort_by_key(|token| {
        (
            token.source_line,
            token.source_column,
            token.generated_line,
            token.generated_column,
        )
    });
    Ok(mappings)
}

fn source_matches(source_path: &Path, source: &str) -> bool {
    let expected = source_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    source == source_path.to_string_lossy()
        || source
            .rsplit(['/', '\\'])
            .next()
            .is_some_and(|name| name == expected)
}

#[allow(clippy::too_many_arguments)]
fn compose_source_map(
    source_path: &Path,
    original_text: &str,
    projection_source_map: Option<&str>,
    projection_tokens: &[MappingToken],
    preprocess_mappings: &PreprocessMappings,
    preprocess_status: PreprocessStatus,
    generated_text: &str,
    generated_index: &LineIndex,
    import_insertions: &[(usize, std::ops::Range<usize>)],
) -> Result<Option<String>, TsgoOverlayError> {
    if projection_source_map.is_none() || preprocess_status == PreprocessStatus::Unmapped {
        return Ok(None);
    }

    let mut composed = projection_tokens
        .iter()
        .map(|token| {
            let source = token.source.and_then(|(line, column)| {
                let position = Position::new(line, column);
                match preprocess_status {
                    PreprocessStatus::Identity => Some(position),
                    PreprocessStatus::Mapped => {
                        map_generated_tokens(&preprocess_mappings.generated, position)
                    }
                    PreprocessStatus::Unmapped => None,
                }
            });
            (token.generated_line, token.generated_column, source)
        })
        .collect::<Vec<_>>();
    composed.extend(import_insertions.iter().map(|(_, range)| {
        let position = generated_index.position(generated_text, range.start);
        (position.line, position.character, None)
    }));
    composed.sort_by_key(|(line, column, source)| (*line, *column, source.is_some()));

    let source_name = source_path.to_string_lossy();
    let mut builder = SourceMapBuilder::new(None);
    let source_id = builder.add_source(&source_name);
    builder.set_source_contents(source_id, Some(original_text));
    for (generated_line, generated_column, source) in composed {
        let (source_line, source_column, source_name) = source.map_or((0, 0, None), |position| {
            (
                position.line,
                position.character,
                Some(source_name.as_ref()),
            )
        });
        builder.add(
            generated_line,
            generated_column,
            source_line,
            source_column,
            source_name,
            None,
            false,
        );
    }
    let map = builder.into_sourcemap();
    let mut encoded = Vec::new();
    map.to_writer(&mut encoded)
        .map_err(|error| TsgoOverlayError::InvalidSourceMap {
            path: source_path.to_path_buf(),
            message: error.to_string(),
        })?;
    Ok(Some(
        String::from_utf8(encoded).expect("source-map JSON is UTF-8"),
    ))
}

/// `DocumentSnapshot.ts:232-237` picks `ScriptKind.JS` unless a script tag says
/// TypeScript, and tsgo reads the script kind off the LSP language id rather
/// than off the `.tsx` shadow name.
fn shadow_language_id(source: &str) -> &'static str {
    if is_typescript_component(source) {
        "typescriptreact"
    } else {
        "javascriptreact"
    }
}

/// A fingerprint of the `.svelte` specifiers `source` imports: when it
/// changes, the components a document reaches may have.
#[must_use]
pub fn svelte_import_fingerprint(source: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    if source.contains(".svelte") {
        svelte_specifiers(source).hash(&mut hasher);
    }
    hasher.finish()
}

/// The `.svelte` module specifiers in `source`, found the same way
/// [`rewrite_plain_svelte_imports`] finds the ones it suffixes.
fn svelte_specifiers(source: &str) -> Vec<&str> {
    let (_, insertions) = rewrite_plain_svelte_imports(source);
    insertions
        .into_iter()
        .filter_map(|(end, _)| {
            let start = source[..end].rfind(['\'', '"', '`'])? + 1;
            Some(&source[start..end])
        })
        .collect()
}

/// One `paths` pattern: `prefix*suffix` maps onto each target with the `*`
/// replaced; an exact pattern (no `*`) has no suffix.
#[derive(Debug, Clone)]
struct AliasPattern {
    prefix: String,
    suffix: Option<String>,
    targets: Vec<String>,
}

/// The files one `.svelte` specifier may name: relative to `directory`, or
/// through the project's `paths` patterns.
fn resolve_svelte_specifier(
    directory: &Path,
    specifier: &str,
    aliases: &[AliasPattern],
) -> Vec<PathBuf> {
    if specifier.starts_with("./") || specifier.starts_with("../") {
        return vec![absolute_normalized(&directory.join(specifier))];
    }
    let mut candidates = Vec::new();
    for alias in aliases {
        let star = match &alias.suffix {
            None if specifier == alias.prefix => "",
            Some(suffix)
                if specifier.len() >= alias.prefix.len() + suffix.len()
                    && specifier.starts_with(alias.prefix.as_str())
                    && specifier.ends_with(suffix.as_str()) =>
            {
                &specifier[alias.prefix.len()..specifier.len() - suffix.len()]
            }
            _ => continue,
        };
        for target in &alias.targets {
            candidates.push(absolute_normalized(Path::new(
                &target.replacen('*', star, 1),
            )));
        }
    }
    candidates
}

/// A script module `walk_owned_files` collects: not a declaration file.
fn is_script_module(name: &str) -> bool {
    !name.ends_with(".d.ts")
        && [".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs"]
            .iter()
            .any(|extension| name.ends_with(extension))
}

/// An event's path in the canonical form the overlay keys sources by, also
/// for a file that no longer exists (its directory usually does).
fn canonical_event_path(path: &Path) -> PathBuf {
    let path = absolute_normalized(path);
    if let Ok(canonical) = fs::canonicalize(&path) {
        return canonical;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => {
            fs::canonicalize(parent).map_or(path.clone(), |parent| parent.join(name))
        }
        _ => path,
    }
}

/// The files a project at `dir` owns: subdirectories carrying their own
/// `tsconfig.json` / `jsconfig.json` belong to that project, and
/// `node_modules` and hidden entries are skipped as the workspace walker
/// (`rsvelte_check::find_svelte_files`) skips them. Symlinks are not
/// followed. Components and script modules (not `.d.ts`) are collected
/// separately.
fn walk_owned_files(dir: &Path, components: &mut Vec<PathBuf>, modules: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || name == "node_modules" {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if file_type.is_dir() {
            if path.join("tsconfig.json").is_file() || path.join("jsconfig.json").is_file() {
                continue;
            }
            walk_owned_files(&path, components, modules);
        } else if file_type.is_file() {
            if name.ends_with(".svelte") {
                components.push(path);
            } else if is_script_module(&name) {
                modules.push(path);
            }
        }
    }
}

fn rewrite_plain_svelte_imports(source: &str) -> (String, Vec<(usize, std::ops::Range<usize>)>) {
    let bytes = source.as_bytes();
    let mut insertions = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let quote = bytes[index];
        if !matches!(quote, b'\'' | b'"' | b'`') {
            index += 1;
            continue;
        }
        let start = index;
        index += 1;
        let content_start = index;
        while index < bytes.len() {
            if bytes[index] == b'\\' {
                index = (index + 2).min(bytes.len());
                continue;
            }
            if bytes[index] == quote {
                break;
            }
            index += 1;
        }
        if index >= bytes.len() {
            break;
        }
        let content = &source[content_start..index];
        let svelte_end = content
            .rfind(".svelte")
            .map(|offset| content_start + offset + 7);
        let suffix_ok = svelte_end
            .is_some_and(|end| end == index || matches!(bytes.get(end), Some(b'?' | b'#')));
        let prefix = source[..start].trim_end();
        let module_context = prefix.ends_with("from")
            || prefix.ends_with("import")
            || prefix.ends_with("import(")
            || prefix.ends_with("require(");
        if suffix_ok && module_context {
            insertions.push(svelte_end.expect("checked above"));
        }
        index += 1;
    }

    let mut generated = source.to_string();
    for &offset in insertions.iter().rev() {
        generated.insert_str(offset, ".tsx");
    }
    let ranges = insertions
        .into_iter()
        .enumerate()
        .map(|(index, source)| {
            let start = source + index * 4;
            (source, start..start + 4)
        })
        .collect();
    (generated, ranges)
}

fn closest_source_token(tokens: &[MappingToken], source: Position) -> Option<MappingToken> {
    tokens
        .iter()
        .filter_map(|token| {
            let (line, column) = token.source?;
            (line == source.line && column <= source.character)
                .then(|| (source.character - column, *token))
        })
        .min_by_key(|(distance, token)| (*distance, token.generated_line, token.generated_column))
        .map(|(_, token)| token)
}

fn map_original_to_preprocessed(entry: &ShadowState, position: Position) -> Option<Position> {
    match entry.preprocess_status {
        PreprocessStatus::Identity => Some(position),
        PreprocessStatus::Mapped => {
            map_original_tokens(&entry.preprocess_mappings.original, position)
        }
        PreprocessStatus::Unmapped => None,
    }
}

fn map_preprocessed_to_original(entry: &ShadowState, position: Position) -> Option<Position> {
    match entry.preprocess_status {
        PreprocessStatus::Identity => Some(position),
        PreprocessStatus::Mapped => {
            map_generated_tokens(&entry.preprocess_mappings.generated, position)
        }
        PreprocessStatus::Unmapped => None,
    }
}

fn map_generated_tokens(tokens: &[MappingToken], position: Position) -> Option<Position> {
    let index = tokens.partition_point(|token| {
        (token.generated_line, token.generated_column) <= (position.line, position.character)
    });
    let previous = index.checked_sub(1).and_then(|index| tokens.get(index))?;
    if previous.generated_line != position.line
        || generated_is_unmapped_gap(tokens, index, position)
    {
        return None;
    }
    previous
        .source
        .map(|(line, column)| Position::new(line, column))
}

fn map_original_tokens(tokens: &[ReverseMappingToken], position: Position) -> Option<Position> {
    let key = (position.line, position.character);
    let first_equal_or_greater =
        tokens.partition_point(|token| (token.source_line, token.source_column) < key);
    let token = tokens
        .get(first_equal_or_greater)
        .filter(|token| (token.source_line, token.source_column) == key)
        .or_else(|| {
            first_equal_or_greater
                .checked_sub(1)
                .and_then(|index| tokens.get(index))
        })?;
    (token.source_line == position.line)
        .then_some(Position::new(token.generated_line, token.generated_column))
}

fn exact_source_offset(map: &ProjectionMap, offset: usize) -> Option<u32> {
    let offset = u32::try_from(offset).ok()?;
    if let Some(generated) = map.source_to_generated(offset).first() {
        return Some(*generated);
    }
    map.segments()
        .iter()
        .find(|segment| segment.source.end() == offset)
        .map(|segment| segment.generated.end())
}

fn exact_generated_offset(map: &ProjectionMap, offset: usize) -> Option<u32> {
    let offset = u32::try_from(offset).ok()?;
    if let Some(source) = map.generated_to_source(offset) {
        return Some(source);
    }
    map.segments()
        .iter()
        .find(|segment| segment.generated.end() == offset)
        .map(|segment| segment.source.end())
}

fn generated_is_unmapped_gap(
    tokens: &[MappingToken],
    next_index: usize,
    position: Position,
) -> bool {
    let Some(previous) = next_index.checked_sub(1).and_then(|i| tokens.get(i)) else {
        return true;
    };
    if (previous.generated_line, previous.generated_column) == (position.line, position.character) {
        return previous.source.is_none();
    }
    let Some(next) = tokens.get(next_index) else {
        return false;
    };
    next.generated_line == position.line
        && previous.source.is_some()
        && previous.source == next.source
}

fn ignored_ranges(text: &str) -> Vec<std::ops::Range<usize>> {
    let mut ranges = Vec::new();
    let mut cursor = 0;
    while let Some(relative_start) = text[cursor..].find(IGNORE_START) {
        let start = cursor + relative_start;
        let content_start = start + IGNORE_START.len();
        let Some(relative_end) = text[content_start..].find(IGNORE_END) else {
            ranges.push(start..text.len());
            break;
        };
        let end = content_start + relative_end + IGNORE_END.len();
        ranges.push(start..end);
        cursor = end;
    }
    ranges
}

/// JS `lastIndexOf`/`indexOf` return `-1` when absent and upstream compares those
/// sentinels directly (`lastEnd === nextEnd`), so the port keeps them as `i64`.
fn occurrences<'a>(text: &'a str, needle: &'a str) -> impl Iterator<Item = usize> + 'a {
    let bytes = text.as_bytes();
    let first = needle.as_bytes()[0];
    (0..bytes.len())
        .filter(move |&at| bytes[at] == first)
        .filter(move |&at| bytes[at..].starts_with(needle.as_bytes()))
}

fn last_index_of(text: &str, needle: &str, from: usize) -> i64 {
    occurrences(text, needle)
        .take_while(|&at| at <= from)
        .last()
        .map_or(-1, |at| at as i64)
}

fn index_of(text: &str, needle: &str, from: usize) -> i64 {
    occurrences(text, needle)
        .find(|&at| at >= from)
        .map_or(-1, |at| at as i64)
}

/// Port of `isInGeneratedCode`
/// (`language-server/src/plugins/typescript/features/utils.ts:102-109`).
///
/// Both markers open and close with `/`, so an occurrence can overlap the one
/// before it and a non-overlapping scan misses the later of the pair.
pub(crate) fn is_in_generated_code(text: &str, start: usize, end: usize) -> bool {
    let last_start = last_index_of(text, IGNORE_START, start);
    let last_end = last_index_of(text, IGNORE_END, start);
    let next_end = index_of(text, IGNORE_END, end);
    (last_start > last_end || last_end == next_end) && last_start < next_end
}

fn ordered_range(start: Position, end: Position) -> Range {
    if (start.line, start.character) <= (end.line, end.character) {
        Range::new(start, end)
    } else {
        Range::new(end, start)
    }
}

fn utf8_position(text: &str, offset: usize) -> Position {
    let offset = floor_char_boundary(text, offset.min(text.len()));
    let line = text.as_bytes()[..offset]
        .iter()
        .filter(|byte| **byte == b'\n')
        .count();
    let line_start = text.as_bytes()[..offset]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |position| position + 1);
    Position::new(
        u32::try_from(line).unwrap_or(u32::MAX),
        u32::try_from(offset - line_start).unwrap_or(u32::MAX),
    )
}

fn utf8_offset(text: &str, position: Position) -> usize {
    let mut line = 0u32;
    let mut line_start = 0usize;
    for (index, byte) in text.bytes().enumerate() {
        if line == position.line {
            break;
        }
        if byte == b'\n' {
            line += 1;
            line_start = index + 1;
        }
    }
    if line != position.line {
        return text.len();
    }
    let line_end = text[line_start..]
        .find('\n')
        .map_or(text.len(), |relative| line_start + relative);
    floor_char_boundary(
        text,
        line_start
            .saturating_add(position.character as usize)
            .min(line_end),
    )
}

fn floor_char_boundary(text: &str, mut offset: usize) -> usize {
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

/// Marks the cache root as ignored by git. The cache is created inside every
/// workspace folder the server is opened against, so without this the user gets
/// an untracked directory in `git status` from nothing but opening an editor —
/// and unlike `rsvelte-check`, there is no command they ran that could have
/// added the ignore entry for them.
///
/// Best-effort: a cache root we cannot write into still yields a working
/// overlay, so a failure here must not fail the build. A symlinked
/// `.gitignore` is left alone rather than written through, for the same reason
/// `reject_symlink_components` exists.
fn write_cache_gitignore(cache_root: &Path) {
    let path = cache_root.join(".gitignore");
    if reject_symlink_components(&path, cache_root).is_err() {
        return;
    }
    let _ = write_if_changed(&path, CACHE_GITIGNORE);
}

/// Most threads a project load projects its members on.
pub(crate) const MAX_PROJECTION_THREADS: usize = 8;
/// Fewer members than this per thread are projected on one thread.
const MIN_SOURCES_PER_THREAD: usize = 8;
/// svelte2tsx recurses over the markup. Address space, not resident memory,
/// as for the analysis worker.
const PROJECTION_STACK_SIZE: usize = 64 * 1024 * 1024;

/// Suffix of the temporary file a body is written to before the rename. The
/// name also starts with a dot, so neither tsgo's include wildcards nor the
/// sweep's `.svelte.tsx` match ever sees it as a component.
const BODY_TEMP_SUFFIX: &str = ".tmp";

fn write_body(
    path: &Path,
    cache_dir: &Path,
    text: &str,
) -> Result<Option<lsp_types::FileChangeType>, TsgoOverlayError> {
    reject_symlink_components(path, cache_dir)?;
    let existed = match fs::read(path) {
        Ok(existing) if existing == text.as_bytes() => return Ok(None),
        Ok(_) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(TsgoOverlayError::InvalidSource {
            path: path.to_path_buf(),
            reason: "shadow has no parent directory",
        });
    };
    fs::create_dir_all(parent)?;
    reject_symlink_components(parent, cache_dir)?;
    let temporary = parent.join(format!(
        ".{}.{}{BODY_TEMP_SUFFIX}",
        name.to_string_lossy(),
        std::process::id()
    ));
    fs::write(&temporary, text)?;
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    Ok(Some(if existed {
        lsp_types::FileChangeType::CHANGED
    } else {
        lsp_types::FileChangeType::CREATED
    }))
}

/// Make `link` a symlink to `target`, unless something real is already there.
fn place_symlink(link: &Path, target: &Path, cache_root: &Path) -> io::Result<()> {
    match fs::symlink_metadata(link) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            if fs::read_link(link).is_ok_and(|existing| existing == target) {
                return Ok(());
            }
            fs::remove_file(link)?;
        }
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let Some(parent) = link.parent() else {
        return Ok(());
    };
    fs::create_dir_all(parent)?;
    reject_symlink_components(parent, cache_root)?;
    #[cfg(unix)]
    return std::os::unix::fs::symlink(target, link);
    #[cfg(windows)]
    return std::os::windows::fs::symlink_dir(target, link);
}

/// Whether `path` lies inside an overlay cache, where every file is this
/// server's own output and never a change to the user's project.
#[must_use]
pub fn is_cache_path(path: &Path) -> bool {
    path.components()
        .any(|component| component.as_os_str() == CACHE_DIRECTORY)
}

fn write_if_changed(path: &Path, contents: &str) -> Result<(), TsgoOverlayError> {
    if fs::read_to_string(path).is_ok_and(|existing| existing == contents) {
        return Ok(());
    }
    fs::write(path, contents)?;
    Ok(())
}

fn reject_symlink_components(path: &Path, cache_dir: &Path) -> io::Result<()> {
    let relative = path.strip_prefix(cache_dir).map_err(|_| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("overlay path escapes cache: {}", path.display()),
        )
    })?;
    let mut current = cache_dir.to_path_buf();
    reject_symlink(&current)?;
    for component in relative.components() {
        current.push(component.as_os_str());
        reject_symlink(&current)?;
    }
    Ok(())
}

fn reject_symlink(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("overlay cache path contains a symlink: {}", path.display()),
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn nearest_existing_path(path: &Path) -> io::Result<&Path> {
    let mut current = Some(path);
    while let Some(candidate) = current {
        match fs::symlink_metadata(candidate) {
            Ok(_) => return Ok(candidate),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                current = candidate.parent();
            }
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("no existing ancestor for {}", path.display()),
    ))
}

fn join_existing_suffix(existing: PathBuf, suffix: &Path) -> PathBuf {
    if suffix.as_os_str().is_empty() {
        existing
    } else {
        existing.join(suffix)
    }
}

fn absolute_normalized(path: &Path) -> PathBuf {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(
                    normalized.components().next_back(),
                    Some(Component::Normal(_))
                ) {
                    normalized.pop();
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn path_for_tsconfig(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn path_to_uri(path: &Path) -> Result<Uri, TsgoOverlayError> {
    let path = path_for_tsconfig(path);
    let mut encoded = String::with_capacity(path.len() + 8);
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b':' | b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            write!(&mut encoded, "%{byte:02X}").expect("writing to String cannot fail");
        }
    }
    let prefix = if encoded.starts_with('/') {
        "file://"
    } else {
        "file:///"
    };
    Uri::from_str(&format!("{prefix}{encoded}"))
        .map_err(|_| TsgoOverlayError::InvalidUri(path.to_owned().into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsvelte_check::overlay::{SHIM_NATIVE_JSX_NAME, SHIM_SHIMS_V4_NAME};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Every test that is not about the fallback pins it off, so the program a
    /// test asserts on does not depend on whether the tree it runs in happens
    /// to have `node_modules/svelte` above the test binary.
    fn build_overlay(workspace: &Path) -> Result<TsgoOverlay, TsgoOverlayError> {
        TsgoOverlay::build_with(workspace, None, None)
    }

    struct TestWorkspace(PathBuf);

    impl TestWorkspace {
        fn new(name: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "rsvelte-tsgo-overlay-{name}-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }
    }

    impl Drop for TestWorkspace {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn encoded_preprocess_map(
        source_path: &Path,
        original_text: &str,
        mappings: &[(Position, Option<Position>)],
    ) -> String {
        let source_name = source_path.file_name().unwrap().to_string_lossy();
        let mut builder = SourceMapBuilder::new(None);
        let source_id = builder.add_source(&source_name);
        builder.set_source_contents(source_id, Some(original_text));
        let mut mappings = mappings.to_vec();
        mappings.sort_by_key(|(generated, _)| (generated.line, generated.character));
        for (generated, original) in mappings {
            let (line, column, source) = original.map_or((0, 0, None), |original| {
                (
                    original.line,
                    original.character,
                    Some(source_name.as_ref()),
                )
            });
            builder.add(
                generated.line,
                generated.character,
                line,
                column,
                source,
                None,
                false,
            );
        }
        let mut output = Vec::new();
        builder.into_sourcemap().to_writer(&mut output).unwrap();
        String::from_utf8(output).unwrap()
    }

    fn positions_of(text: &str, needle: &str) -> Vec<(Position, Position)> {
        let index = LineIndex::new(text);
        text.match_indices(needle)
            .map(|(offset, value)| {
                (
                    index.position(text, offset),
                    index.position(text, offset + value.len()),
                )
            })
            .collect()
    }

    #[test]
    fn is_in_generated_code_agrees_with_upstream_at_every_marker_boundary() {
        let region = format!("let a = 1;{IGNORE_START}gen{IGNORE_END}let b = 2;");
        let open = region.find(IGNORE_START).unwrap();
        let close = region.find(IGNORE_END).unwrap();
        // Expected values printed by upstream's own `isInGeneratedCode`.
        let cells: &[(usize, usize, bool, &str)] = &[
            (open, open, true, "at the START marker's own leading slash"),
            (
                open + IGNORE_START.len() + 1,
                open + IGNORE_START.len() + 1,
                true,
                "region body",
            ),
            (close, close, true, "at the END marker's leading slash"),
            (
                close + IGNORE_END.len(),
                close + IGNORE_END.len(),
                false,
                "past the END marker",
            ),
            (0, 0, false, "before any marker"),
            (
                open,
                close + IGNORE_END.len(),
                false,
                "a span covering the whole region",
            ),
        ];
        for &(start, end, expected, what) in cells {
            assert_eq!(
                is_in_generated_code(&region, start, end),
                expected,
                "{what} [{start},{end}]"
            );
        }
        assert!(
            cells.iter().any(|cell| cell.2) && cells.iter().any(|cell| !cell.2),
            "liveness: the cells must exercise both answers"
        );
    }

    #[test]
    fn is_in_generated_code_sees_an_end_marker_overlapping_the_one_before_it() {
        // Both markers open and close with `/`, so `E` immediately followed by
        // its own tail contains a second `E` starting one byte before the first
        // one ends; a non-overlapping scan reports `false` here.
        let text = format!("{IGNORE_START}g{IGNORE_END}{}", &IGNORE_END[1..]);
        let at = text.find(IGNORE_END).unwrap() + IGNORE_END.len() - 1;
        assert_eq!(
            text.match_indices(IGNORE_END).count(),
            1,
            "liveness: a non-overlapping scan must find only the first marker"
        );
        assert!(
            is_in_generated_code(&text, at, at),
            "overlapping END at {at}"
        );
    }

    #[test]
    fn an_existing_leaf_does_not_gain_an_empty_path_component() {
        let existing = PathBuf::from("/workspace/src/App.svelte");
        assert_eq!(
            join_existing_suffix(existing.clone(), Path::new("")),
            existing
        );
    }

    // Measures, not asserts: how many of the positions the LSP corpus gate
    // requests survive `map_source_position` -> `map_generated_position`
    // unchanged. `forward-none` is the correct answer where a position has no TS
    // projection (a `<style>` body, markup text), so an equality assertion here
    // would pin correct behaviour; only `moved` is a defect class. Measured on
    // submodules/melt-ui 2026-09-01: positions=12043 identity=9344 moved=1239
    // forward_none=1460 reverse_none=0, all 43 files preprocess_status=Identity.
    // `ROUNDTRIP_ROOT` selects another corpus repo; `ROUNDTRIP_OUT` writes one
    // `<file>\t<line>:<col>\t<class>` row per position.
    #[test]
    #[ignore]
    fn projection_roundtrip_identity_measure() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let root = repo.join(
            std::env::var("ROUNDTRIP_ROOT").unwrap_or_else(|_| "submodules/melt-ui".to_string()),
        );
        let mut files = Vec::new();
        fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
            let Ok(entries) = fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if path.is_dir() {
                    if name != "node_modules" && !name.starts_with('.') {
                        walk(&path, out);
                    }
                } else if name.ends_with(".svelte") {
                    out.push(path);
                }
            }
        }
        walk(&root, &mut files);
        files.sort();
        assert!(
            !files.is_empty(),
            "no .svelte files under {}",
            root.display()
        );

        let mut overlay = build_overlay(&root).unwrap();
        let (mut positions, mut identity, mut forward_none, mut reverse_none, mut moved) =
            (0usize, 0usize, 0usize, 0usize, 0usize);
        let mut open_failed = 0usize;
        let mut by_status: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        let mut sample: Vec<String> = Vec::new();
        let mut classes: Vec<String> = Vec::new();
        for file in &files {
            let text = fs::read_to_string(file).unwrap();
            let rel = file.strip_prefix(&root).unwrap().display().to_string();
            if overlay.open_or_update(file, &text, 0).is_err() {
                open_failed += 1;
                continue;
            }
            let shadow_path = overlay.shadow_path_for(file).unwrap();
            let status = overlay
                .entries
                .get(&overlay.lookup_source_path(file))
                .map(|entry| format!("{:?}/identity={}", entry.preprocess_status, entry.identity))
                .unwrap_or_else(|| "missing".to_string());
            *by_status.entry(status).or_default() += 1;
            for (line, line_text) in text.split('\n').enumerate() {
                for (column, position) in identifier_probe_positions(line_text) {
                    let _ = column;
                    positions += 1;
                    let source = Position::new(line as u32, position);
                    let Some(generated) = overlay.map_source_position(file, source) else {
                        forward_none += 1;
                        classes.push(format!(
                            "{}\t{}:{}\tforward-none",
                            rel, source.line, source.character
                        ));
                        continue;
                    };
                    let Some(back) = overlay.map_generated_position(&shadow_path, generated) else {
                        reverse_none += 1;
                        classes.push(format!(
                            "{}\t{}:{}\treverse-none",
                            rel, source.line, source.character
                        ));
                        continue;
                    };
                    if back == source {
                        identity += 1;
                        classes.push(format!(
                            "{}\t{}:{}\tidentity",
                            rel, source.line, source.character
                        ));
                    } else {
                        classes.push(format!(
                            "{}\t{}:{}\tmoved",
                            rel, source.line, source.character
                        ));
                        moved += 1;
                        if sample.len() < 20 {
                            sample.push(format!(
                                "{}  {}:{} -> gen {}:{} -> {}:{}",
                                file.strip_prefix(&root).unwrap().display(),
                                source.line,
                                source.character,
                                generated.line,
                                generated.character,
                                back.line,
                                back.character
                            ));
                        }
                    }
                }
            }
        }
        if let Ok(path) = std::env::var("ROUNDTRIP_OUT") {
            fs::write(path, classes.join("\n") + "\n").unwrap();
        }
        println!("files={} open_failed={}", files.len(), open_failed);
        println!(
            "positions={positions} identity={identity} moved={moved} forward_none={forward_none} reverse_none={reverse_none}"
        );
        for (status, count) in &by_status {
            println!("  entry status {status}: {count} files");
        }
        for line in &sample {
            println!("  moved: {line}");
        }
    }

    // Mirrors `identifierPositionIterator` in scripts/compat-lsp/suites.mjs: the
    // gate requests the SECOND character of each identifier, never its start.
    fn identifier_probe_positions(line: &str) -> Vec<(usize, u32)> {
        let chars: Vec<char> = line.chars().collect();
        let is_start = |c: char| c == '$' || c == '_' || c.is_alphabetic();
        let is_continue = |c: char| c == '$' || c == '_' || c.is_alphanumeric();
        let mut out = Vec::new();
        let mut index = 0usize;
        // UTF-16 column of each char index.
        let mut utf16: Vec<u32> = Vec::with_capacity(chars.len() + 1);
        let mut acc = 0u32;
        for c in &chars {
            utf16.push(acc);
            acc += c.len_utf16() as u32;
        }
        utf16.push(acc);
        while index < chars.len() {
            if is_start(chars[index]) {
                let start = index;
                index += 1;
                while index < chars.len() && is_continue(chars[index]) {
                    index += 1;
                }
                let offset = if index - start > 1 { 1 } else { 0 };
                out.push((start, utf16[start + offset]));
            } else {
                index += 1;
            }
        }
        out
    }

    #[test]
    fn build_discovers_project_components_and_persists_their_bodies() {
        let workspace = TestWorkspace::new("build");
        let app = workspace.0.join("src/App.svelte");
        let nested = workspace.0.join("src/lib/Nested.svelte");
        write(&app, "<h1>Hello</h1>");
        write(&nested, "<p>Nested</p>");
        write(
            &workspace.0.join("node_modules/pkg/Skipped.svelte"),
            "<p>skip</p>",
        );

        let overlay = build_overlay(&workspace.0).unwrap();
        assert_eq!(overlay.eager_shadows().len(), 2);
        let shadow = overlay.shadow_for_source(&app).unwrap().clone();
        let shadow_path = overlay.shadow_dir.join("src/App.svelte.tsx");
        assert_eq!(shadow.shadow_uri, path_to_uri(&shadow_path).unwrap());
        assert!(shadow_path.parent().unwrap().is_dir());
        assert!(overlay.tsconfig_path().is_file());
        assert!(overlay.cache_dir().join(SHIM_SHIMS_V4_NAME).is_file());
        assert!(overlay.cache_dir().join(SHIM_JSX_V4_NAME).is_file());
        assert_eq!(
            overlay.unresolved_shadow_routes().len(),
            2,
            "a body tsgo cannot read is not resolvable"
        );

        assert_eq!(
            overlay.persist_body(&app).unwrap(),
            Some(lsp_types::FileChangeType::CREATED)
        );
        assert_eq!(
            overlay.persist_body(&nested).unwrap(),
            Some(lsp_types::FileChangeType::CREATED)
        );
        assert_eq!(fs::read_to_string(&shadow_path).unwrap(), shadow.text);
        assert!(overlay.unresolved_shadow_routes().is_empty());
        assert_eq!(overlay.persist_body(&app).unwrap(), None, "unchanged");
        let leftovers = fs::read_dir(shadow_path.parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .ends_with(BODY_TEMP_SUFFIX)
            })
            .count();
        assert_eq!(leftovers, 0, "the temporary file is renamed into place");
    }

    #[test]
    fn a_changed_source_rewrites_its_body_and_a_deleted_one_removes_it() {
        let workspace = TestWorkspace::new("body-lifecycle");
        let app = workspace.0.join("src/App.svelte");
        write(&app, "<p>one</p>");
        let mut overlay = build_overlay(&workspace.0).unwrap();
        let body = overlay.shadow_dir.join("src/App.svelte.tsx");
        overlay.persist_body(&app).unwrap();

        write(
            &app,
            "<script lang=\"ts\">let two = 2;</script><p>{two}</p>",
        );
        let update = overlay.refresh().unwrap();
        assert_eq!(update.opened_or_changed.len(), 1);
        assert_eq!(
            overlay.persist_body(&app).unwrap(),
            Some(lsp_types::FileChangeType::CHANGED)
        );
        assert!(fs::read_to_string(&body).unwrap().contains("let two = 2;"));

        fs::remove_file(&app).unwrap();
        let update = overlay.refresh().unwrap();
        assert_eq!(update.closed.len(), 1);
        assert!(!body.exists(), "a deleted component leaves no ghost behind");
        assert!(!overlay.shadow_dir.join("src").exists(), "nor its skeleton");
    }

    #[test]
    fn the_sweep_removes_orphan_bodies_and_torn_writes_only() {
        let workspace = TestWorkspace::new("sweep");
        let app = workspace.0.join("src/App.svelte");
        write(&app, "<p />");
        let overlay = build_overlay(&workspace.0).unwrap();
        overlay.persist_body(&app).unwrap();
        let orphan = overlay.shadow_dir.join("src/Gone.svelte.tsx");
        let torn = overlay.shadow_dir.join("src/.App.svelte.tsx.42.tmp");
        let foreign = overlay.shadow_dir.join("src/notes.txt");
        write(&orphan, "export default {}");
        write(&torn, "partial");
        write(&foreign, "left alone");

        let removed = overlay.sweep_bodies(|_| true);
        assert_eq!(removed, vec![orphan.clone()]);
        assert!(!orphan.exists() && !torn.exists());
        assert!(foreign.exists());
        assert!(overlay.shadow_dir.join("src/App.svelte.tsx").is_file());

        let removed = overlay.sweep_bodies(|_| false);
        assert_eq!(removed, vec![overlay.shadow_dir.join("src/App.svelte.tsx")]);
    }

    #[test]
    fn only_paths_inside_an_overlay_cache_are_cache_paths() {
        assert!(is_cache_path(Path::new(
            "/w/.svelte-rs/projects/a/tsgo/tsconfig.json"
        )));
        assert!(!is_cache_path(Path::new("/w/src/tsconfig.json")));
        assert!(!is_cache_path(Path::new("/w/src/.svelte-rs-notes/a.ts")));
    }

    #[test]
    fn overlay_config_merges_inherited_plain_project_specs() {
        let workspace = TestWorkspace::new("config-specs");
        write(
            &workspace.0.join("configs/base.json"),
            r#"{
                // inherited independently from the root config
                "include": ["../src/**/*.ts",],
                "files": ["../ambient.d.ts", "../Ignored.svelte"],
                "exclude": ["../src/generated/**"]
            }"#,
        );
        write(
            &workspace.0.join("tsconfig.json"),
            r#"{ "extends": "./configs/base.json", "compilerOptions": { "strict": true } }"#,
        );
        write(&workspace.0.join("src/main.ts"), "export const main = 1;");
        write(
            &workspace.0.join("ambient.d.ts"),
            "declare const ambient: 1;",
        );
        write(&workspace.0.join("App.svelte"), "<p>{ambient}</p>");

        let overlay = build_overlay(&workspace.0).unwrap();
        let config: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(overlay.tsconfig_path()).unwrap()).unwrap();
        let include = config["include"].as_array().unwrap();
        assert!(include.contains(&json!("svelte/**/*")));
        assert!(include.contains(&json!(path_for_tsconfig(
            &overlay.workspace().join("src/**/*.ts")
        ))));
        let files = config["files"].as_array().unwrap();
        assert!(files.contains(&json!(SHIM_SHIMS_V4_NAME)));
        assert!(files.contains(&json!(SHIM_JSX_V4_NAME)));
        assert!(files.contains(&json!(path_for_tsconfig(
            &overlay.workspace().join("ambient.d.ts")
        ))));
        assert!(!files.iter().any(|file| {
            file.as_str()
                .is_some_and(|file| file.ends_with("Ignored.svelte"))
        }));
        assert_eq!(
            config["exclude"],
            json!([path_for_tsconfig(
                &overlay.workspace().join("src/generated/**")
            )])
        );
    }

    #[test]
    fn a_paths_alias_gains_a_shadow_tree_candidate() {
        // SvelteKit's real layout: the generated config declares the alias and
        // the root config extends it, so the targets are relative to
        // `.svelte-kit/` and not to the config the overlay is handed.
        let workspace = TestWorkspace::new("paths-shadow");
        write(
            &workspace.0.join(".svelte-kit/tsconfig.json"),
            r#"{ "compilerOptions": { "paths": { "$lib": ["../src/lib"], "$lib/*": ["../src/lib/*"] } } }"#,
        );
        write(
            &workspace.0.join("tsconfig.json"),
            r#"{ "extends": "./.svelte-kit/tsconfig.json" }"#,
        );
        write(&workspace.0.join("src/lib/Widget.svelte"), "<p />");

        let overlay = build_overlay(&workspace.0).unwrap();
        let config: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(overlay.tsconfig_path()).unwrap()).unwrap();
        let source = path_for_tsconfig(&overlay.workspace().join("src/lib/*"));
        let shadow = path_for_tsconfig(&overlay.shadow_dir.join("src/lib/*"));
        // The source target stays first: anything resolving today keeps resolving
        // to the same file, and the shadow is only reached as a fallback.
        assert_eq!(
            config["compilerOptions"]["paths"]["$lib/*"],
            json!([source, shadow])
        );
        assert_eq!(
            config["compilerOptions"]["paths"]["$lib"],
            json!([
                path_for_tsconfig(&overlay.workspace().join("src/lib")),
                path_for_tsconfig(&overlay.shadow_dir.join("src/lib"))
            ])
        );
    }

    #[test]
    fn a_project_without_paths_declares_none() {
        let workspace = TestWorkspace::new("paths-absent");
        write(
            &workspace.0.join("tsconfig.json"),
            r#"{ "compilerOptions": { "strict": true } }"#,
        );
        write(&workspace.0.join("src/App.svelte"), "<p />");

        let config = overlay_config_of(&workspace.0);
        assert!(config["compilerOptions"].get("paths").is_none());
    }

    #[test]
    fn a_paths_target_outside_the_workspace_gets_no_shadow_twin() {
        // `strip_prefix` is what decides this, so a target that escapes the
        // workspace must carry one candidate rather than a shadow path that
        // could never hold a component.
        let workspace = TestWorkspace::new("paths-outside");
        write(
            &workspace.0.join("tsconfig.json"),
            r#"{ "compilerOptions": { "paths": { "@out/*": ["../outside/*"], "@in/*": ["./src/*"] } } }"#,
        );
        write(&workspace.0.join("src/App.svelte"), "<p />");

        let overlay = build_overlay(&workspace.0).unwrap();
        let config: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(overlay.tsconfig_path()).unwrap()).unwrap();
        assert_eq!(
            config["compilerOptions"]["paths"]["@out/*"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            config["compilerOptions"]["paths"]["@in/*"],
            json!([
                path_for_tsconfig(&overlay.workspace().join("src/*")),
                path_for_tsconfig(&overlay.shadow_dir.join("src/*"))
            ])
        );
    }

    #[test]
    fn a_paths_pattern_that_cannot_be_rewritten_is_preserved_verbatim() {
        // Overriding `paths` replaces the inherited map wholesale, so a shape
        // this rewrite does not understand has to survive rather than vanish.
        let workspace = TestWorkspace::new("paths-verbatim");
        write(
            &workspace.0.join("tsconfig.json"),
            r#"{ "compilerOptions": { "paths": { "@ok/*": ["./src/*"], "@weird/*": "not-an-array", "@empty/*": [] } } }"#,
        );
        write(&workspace.0.join("src/App.svelte"), "<p />");

        let overlay = build_overlay(&workspace.0).unwrap();
        let config: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(overlay.tsconfig_path()).unwrap()).unwrap();
        let paths = &config["compilerOptions"]["paths"];
        // Live control: the rewritable pattern still gains its shadow twin, so a
        // rule that preserved everything verbatim would fail here.
        assert_eq!(
            paths["@ok/*"],
            json!([
                path_for_tsconfig(&overlay.workspace().join("src/*")),
                path_for_tsconfig(&overlay.shadow_dir.join("src/*"))
            ])
        );
        assert_eq!(paths["@weird/*"], json!("not-an-array"));
        assert_eq!(paths["@empty/*"], json!([]));
    }

    fn overlay_config_of(workspace: &Path) -> serde_json::Value {
        let overlay = build_overlay(workspace).unwrap();
        serde_json::from_str(&fs::read_to_string(overlay.tsconfig_path()).unwrap()).unwrap()
    }

    #[test]
    fn a_project_without_a_config_does_not_pull_in_the_whole_workspace() {
        let workspace = TestWorkspace::new("no-config-include");
        write(&workspace.0.join("src/App.svelte"), "<p />");
        write(
            &workspace.0.join("src/globals.d.ts"),
            "declare const unrelated: 1;",
        );

        let config = overlay_config_of(&workspace.0);
        // `service.ts:874-878` builds its fallback with `include: []`.
        assert_eq!(config["include"], json!(["svelte/**/*"]));
        assert_eq!(config["compilerOptions"]["target"], json!("ESNext"));
        assert!(config.get("extends").is_none());
    }

    #[test]
    fn a_target_below_es2015_is_raised_and_a_modern_one_is_left_alone() {
        for (declared, expected) in [
            ("ES5", Some("ES2015")),
            ("es3", Some("ES2015")),
            ("ES2020", None),
            ("ESNext", None),
        ] {
            let workspace = TestWorkspace::new(&format!("target-{declared}"));
            write(
                &workspace.0.join("tsconfig.json"),
                &format!("{{ \"compilerOptions\": {{ \"target\": \"{declared}\" }} }}"),
            );
            write(&workspace.0.join("App.svelte"), "<p />");

            let config = overlay_config_of(&workspace.0);
            assert_eq!(
                config["compilerOptions"]
                    .get("target")
                    .and_then(|v| v.as_str()),
                expected,
                "declared {declared}"
            );
        }
    }

    #[test]
    fn a_target_is_read_through_the_extends_chain() {
        let workspace = TestWorkspace::new("target-extends");
        write(
            &workspace.0.join("configs/base.json"),
            r#"{ "compilerOptions": { "target": "ES2022" } }"#,
        );
        write(
            &workspace.0.join("tsconfig.json"),
            r#"{ "extends": "./configs/base.json" }"#,
        );
        write(&workspace.0.join("App.svelte"), "<p />");

        assert!(
            overlay_config_of(&workspace.0)["compilerOptions"]
                .get("target")
                .is_none()
        );
    }

    #[test]
    fn the_native_jsx_shim_is_part_of_every_program() {
        let workspace = TestWorkspace::new("native-jsx-shim");
        write(&workspace.0.join("App.svelte"), "<p />");

        let overlay = build_overlay(&workspace.0).unwrap();
        assert!(overlay.cache_dir().join(SHIM_NATIVE_JSX_NAME).is_file());
        let config: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(overlay.tsconfig_path()).unwrap()).unwrap();
        assert!(
            config["files"]
                .as_array()
                .unwrap()
                .contains(&json!(SHIM_NATIVE_JSX_NAME))
        );
    }

    #[test]
    fn the_jsx_shim_is_a_fallback_for_a_package_without_svelte_html() {
        // `get_global_types` pushes `svelte-jsx-v4.d.ts` only when the
        // installed svelte has no `svelte-html.d.ts`; shipping both would put
        // two `svelteHTML` namespaces in one program.
        let workspace = TestWorkspace::new("jsx-shim-fallback");
        let svelte = workspace.0.join("node_modules").join("svelte");
        write(&svelte.join("package.json"), r#"{"version":"5.0.0"}"#);

        let (without, html) = global_type_files(&workspace.0);
        assert_eq!(html, None);
        assert!(without.contains(&SHIM_JSX_V4_NAME));

        write(&svelte.join("svelte-html.d.ts"), "");
        let (with, html) = global_type_files(&workspace.0);
        assert!(html.is_some_and(|path| path.ends_with("svelte/svelte-html.d.ts")));
        assert!(!with.contains(&SHIM_JSX_V4_NAME));
        assert!(with.contains(&SHIM_SHIMS_V4_NAME) && with.contains(&SHIM_NATIVE_JSX_NAME));
    }

    #[test]
    fn config_without_file_specs_keeps_default_plain_project_membership() {
        let workspace = TestWorkspace::new("config-default-include");
        write(
            &workspace.0.join("tsconfig.json"),
            r#"{ "compilerOptions": { "strict": true } }"#,
        );
        write(&workspace.0.join("src/main.ts"), "export const main = 1;");
        write(&workspace.0.join("App.svelte"), "<p />");

        let overlay = build_overlay(&workspace.0).unwrap();
        let config: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(overlay.tsconfig_path()).unwrap()).unwrap();
        assert!(
            config["include"]
                .as_array()
                .unwrap()
                .contains(&json!(format!(
                    "{}/**/*",
                    path_for_tsconfig(overlay.workspace())
                )))
        );
    }

    #[test]
    fn open_update_close_keeps_or_removes_the_eager_shadow() {
        let workspace = TestWorkspace::new("lifecycle");
        let app = workspace.0.join("src/App.svelte");
        write(&app, "<p>disk</p>");
        let mut overlay = build_overlay(&workspace.0).unwrap();

        let changed = overlay.open_or_update(&app, "<p>buffer</p>", 7).unwrap();
        assert_eq!(changed.version, 7);
        assert_eq!(fs::read_to_string(&app).unwrap(), "<p>disk</p>");
        assert_eq!(
            overlay
                .entries
                .get(&fs::canonicalize(&app).unwrap())
                .unwrap()
                .source_text,
            "<p>buffer</p>"
        );
        let reverted = overlay.close(&app).unwrap().unwrap();
        assert_eq!(reverted.version, 8);
        overlay.persist_body(&app).unwrap();
        let body = overlay.shadow_dir.join("src/App.svelte.tsx");
        assert_eq!(fs::read_to_string(&body).unwrap(), reverted.text);
        assert_eq!(
            overlay
                .entries
                .get(&fs::canonicalize(&app).unwrap())
                .unwrap()
                .source_text,
            "<p>disk</p>"
        );

        fs::remove_file(&app).unwrap();
        assert!(overlay.close(&app).unwrap().is_none());
        assert!(overlay.shadow_for_source(&app).is_none());
        assert!(!body.exists());
        assert!(!overlay.shadow_dir.join("src").exists());
    }

    #[test]
    fn a_refresh_leaves_an_editor_buffer_alone() {
        let workspace = TestWorkspace::new("refresh-editor");
        let app = workspace.0.join("src/App.svelte");
        let other = workspace.0.join("src/Other.svelte");
        write(&app, "<p>disk</p>");
        write(&other, "<p>other</p>");
        let mut overlay = build_overlay(&workspace.0).unwrap();
        overlay.open_or_update(&app, "<p>buffer</p>", 3).unwrap();
        write(&other, "<p>changed</p>");
        let app_path = fs::canonicalize(&app).unwrap();
        let mut editor = EditorBuffers::default();
        editor.insert(app_path.clone(), Arc::new("<p>buffer</p>".to_string()), 3);

        let update = overlay.refresh_except(&editor).unwrap();
        assert_eq!(update.opened_or_changed.len(), 1, "only the closed file");
        assert_eq!(overlay.source_text(&app), Some("<p>buffer</p>"));
        assert_eq!(overlay.source_text(&other), Some("<p>changed</p>"));

        fs::remove_file(&app).unwrap();
        let update = overlay.refresh_except(&editor).unwrap();
        assert!(
            update.closed.is_empty(),
            "an unsaved buffer outlives its file"
        );
        assert!(overlay.shadow_for_source(&app_path).is_some());
    }

    #[test]
    fn plain_buffers_join_the_overlay_project_without_disk_bodies() {
        let workspace = TestWorkspace::new("plain");
        let source = workspace.0.join("src/main.ts");
        write(&source, "const face = '😀';\nface;\n");
        let mut overlay = build_overlay(&workspace.0).unwrap();

        let shadow = overlay
            .open_plain(&source, "const face = '😀';\nface;\n", 3, "typescript")
            .unwrap();
        let shadow_path = overlay.shadow_dir.join("src/main.ts");
        assert_eq!(shadow.shadow_uri, path_to_uri(&shadow_path).unwrap());
        assert_eq!(overlay.persist_body(&source).unwrap(), None);
        assert!(!shadow_path.exists(), "a plain module is never written");
        assert_eq!(overlay.eager_shadows().len(), 0);
        assert_eq!(
            overlay.map_source_position(&source, Position::new(0, 17)),
            Some(Position::new(0, 19))
        );
        assert_eq!(
            overlay.map_generated_position(&shadow_path, Position::new(0, 19)),
            Some(Position::new(0, 17))
        );
        assert_eq!(
            overlay.close_plain(&source).unwrap(),
            Some(shadow.shadow_uri)
        );
        assert!(overlay.shadow_for_source(&source).is_none());
    }

    #[test]
    fn plain_module_specifiers_target_open_svelte_shadows() {
        let source = "import App from './App.svelte';\nApp;\n";
        let (generated, insertions) = rewrite_plain_svelte_imports(source);
        assert_eq!(generated, "import App from './App.svelte.tsx';\nApp;\n");
        assert_eq!(insertions.len(), 1);
        assert_eq!(&generated[insertions[0].1.clone()], ".tsx");
    }

    #[test]
    fn component_query_anchor_survives_overlay_import_rewriting() {
        use lsp_types::{CompletionContext, CompletionTriggerKind};

        let workspace = TestWorkspace::new("component-query");
        let app = workspace.0.join("App.svelte");
        let source =
            "<script lang=\"ts\">\n  import Child from './Child.svelte';\n</script>\n<Child  />\n";
        write(&app, source);
        write(&workspace.0.join("Child.svelte"), "<p>child</p>");
        let overlay = build_overlay(&workspace.0).unwrap();
        let cursor = LineIndex::new(source).position(source, source.rfind("  ").unwrap() + 1);
        let site = crate::tsgo_component_info::component_completion_site(
            source,
            cursor,
            Some(&CompletionContext {
                trigger_kind: CompletionTriggerKind::INVOKED,
                trigger_character: None,
            }),
            false,
        )
        .unwrap();
        let shadow = overlay.shadow_for_source(&app).unwrap();
        assert!(shadow.text.contains("from './Child.svelte.tsx'"));
        let ranges = crate::tsgo_component_info::generated_component_ranges(
            overlay.projection_map(&app).unwrap(),
            &site,
            &shadow.text,
        );
        assert_eq!(ranges.len(), 1, "{}", shadow.text);
        assert_eq!(&shadow.text[ranges[0].clone()], "Child");
        let source_position =
            LineIndex::new(source).position(source, source.find("Child").unwrap());
        let generated_position = overlay.map_source_position(&app, source_position).unwrap();
        assert_eq!(
            overlay.map_generated_position(
                &overlay.shadow_dir.join("App.svelte.tsx"),
                generated_position
            ),
            Some(source_position)
        );
    }

    #[test]
    fn positions_cross_utf16_and_utf8_only_at_the_editor_boundary() {
        let workspace = TestWorkspace::new("positions");
        let app = workspace.0.join("src/App.svelte");
        let source = "<script lang=\"ts\">\nconst 名前 = \"💡\";\nconsole.log(名前);\n</script>\n";
        write(&app, source);
        let overlay = build_overlay(&workspace.0).unwrap();
        let shadow_path = overlay.shadow_dir.join("src/App.svelte.tsx");
        let source_offset = source.rfind("名前").unwrap();
        let source_position = LineIndex::new(source).position(source, source_offset);
        let generated = overlay
            .map_source_position(&app, source_position)
            .expect("script identifier maps forward");
        let back = overlay
            .map_generated_position(&shadow_path, generated)
            .expect("generated identifier maps back");
        assert_eq!(back, source_position);
    }

    #[test]
    fn preprocessing_composes_original_preprocessed_and_tsx_positions() {
        let workspace = TestWorkspace::new("preprocess-compose");
        let app = workspace.0.join("src/App.svelte");
        let original = concat!(
            "<script lang=\"ts\">\r\n",
            "const emoji = \"😀\"; let value = 1;\r\n",
            "</script>\r\n",
            "<p>{value}</p>\r\n"
        );
        let preprocessed = format!("<!-- generated -->\n{original}");
        write(&app, original);
        let original_values = positions_of(original, "value");
        let preprocessed_values = positions_of(&preprocessed, "value");
        let mappings = preprocessed_values
            .iter()
            .zip(&original_values)
            .flat_map(
                |(&(generated_start, generated_end), &(source_start, source_end))| {
                    [
                        (generated_start, Some(source_start)),
                        (generated_end, Some(source_end)),
                    ]
                },
            )
            .collect::<Vec<_>>();
        let preprocess_map = encoded_preprocess_map(&app, original, &mappings);
        let mut overlay = build_overlay(&workspace.0).unwrap();
        let shadow = overlay
            .open_or_update_preprocessed(&app, original, &preprocessed, Some(&preprocess_map), 4)
            .unwrap();
        let shadow_path = overlay.shadow_dir.join("src/App.svelte.tsx");

        assert_eq!(
            overlay.preprocess_status(&app),
            Some(PreprocessStatus::Mapped)
        );
        assert_eq!(overlay.preprocessed_text(&app), Some(preprocessed.as_str()));
        assert_eq!(overlay.preprocess_map(&app), Some(preprocess_map.as_str()));
        assert!(overlay.projection_map(&app).unwrap().segments().is_empty());

        let source_start = original_values[0].0;
        let source_offset = original.find("value").unwrap();
        let line_start = original[..source_offset].rfind('\n').map_or(0, |at| at + 1);
        assert_eq!(
            u32::try_from(source_offset - line_start).unwrap(),
            source_start.character + 2,
            "the source position must count the astral character as two UTF-16 units"
        );
        let generated_start = overlay
            .map_source_position(&app, source_start)
            .expect("original position maps through preprocessing and TSX");
        assert_eq!(
            overlay.map_generated_position(&shadow_path, generated_start),
            Some(source_start)
        );

        let source_range = Range::new(original_values[0].0, original_values[1].1);
        let generated_range = overlay
            .map_source_range(&app, source_range)
            .expect("multiline range maps through all layers");
        assert_eq!(
            overlay.map_generated_range(&shadow_path, generated_range),
            Some(source_range)
        );

        let generated_offset = utf8_offset(&shadow.text, generated_start);
        let generated_utf16 = LineIndex::new(&shadow.text).position(&shadow.text, generated_offset);
        let composed = SourceMap::from_slice(overlay.source_map(&app).unwrap().as_bytes()).unwrap();
        let token = composed
            .lookup_token(generated_utf16.line, generated_utf16.character)
            .unwrap();
        assert_eq!(token.get_src(), (source_start.line, source_start.character));
    }

    #[test]
    fn preprocessing_rejects_removed_and_generated_only_positions() {
        let workspace = TestWorkspace::new("preprocess-unmapped");
        let app = workspace.0.join("App.svelte");
        let original = "<script>\nlet kept = 1;\nlet removed = 2;\n</script>\n";
        let preprocessed = "<script>\nlet kept = 1;\nlet injected = 3;\n</script>\n";
        write(&app, original);
        let original_kept = positions_of(original, "kept")[0].0;
        let preprocessed_kept = positions_of(preprocessed, "kept")[0].0;
        let original_end = Position::new(3, 0);
        let preprocessed_end = Position::new(3, 0);
        let injected = positions_of(preprocessed, "injected")[0].0;
        let preprocess_map = encoded_preprocess_map(
            &app,
            original,
            &[
                (preprocessed_kept, Some(original_kept)),
                (injected, None),
                (preprocessed_end, Some(original_end)),
            ],
        );
        let mut overlay = build_overlay(&workspace.0).unwrap();
        let shadow = overlay
            .open_or_update_preprocessed(&app, original, preprocessed, Some(&preprocess_map), 1)
            .unwrap();
        let shadow_path = overlay.shadow_dir.join("App.svelte.tsx");

        let removed = positions_of(original, "removed")[0].0;
        assert_eq!(overlay.map_source_position(&app, removed), None);
        let generated_injected = shadow.text.find("injected").unwrap();
        assert_eq!(
            overlay.map_generated_position(
                &shadow_path,
                utf8_position(&shadow.text, generated_injected)
            ),
            None
        );

        overlay
            .open_or_update_preprocessed(&app, original, preprocessed, None, 2)
            .unwrap();
        assert_eq!(
            overlay.preprocess_status(&app),
            Some(PreprocessStatus::Unmapped)
        );
        assert_eq!(overlay.source_map(&app), None);
        assert_eq!(overlay.map_source_position(&app, original_kept), None);
    }

    #[test]
    fn source_map_supplies_a_fallback_for_rewritten_markup() {
        let workspace = TestWorkspace::new("fallback");
        let app = workspace.0.join("App.svelte");
        let source = "<input bind:value={value}>";
        write(&app, source);
        let overlay = build_overlay(&workspace.0).unwrap();
        let opener = Position::new(0, 0);
        assert!(
            overlay.map_source_position(&app, opener).is_some(),
            "rewritten template offsets must not inherit ProjectionMap's holes"
        );
    }

    /// The oracle is the real emitter's output, not a copy of a constant: a
    /// wrong answer here compiles, matches nothing, and reads as "no hints to
    /// filter" rather than as a failure.
    fn render_return_type_slot(source: &str, name: &str) -> (String, Option<u32>) {
        let workspace = TestWorkspace::new(name);
        let app = workspace.0.join("App.svelte");
        write(&app, source);
        let overlay = build_overlay(&workspace.0).unwrap();
        let text = overlay.shadow_for_source(&app).unwrap().text.clone();
        let slot = match crate::tsgo_inlay_hints::ShadowNodes::parse(&text) {
            Some(nodes) => nodes.render_return_type(),
            // The filter answers without a tree too, so the helper must.
            None => crate::tsgo_inlay_hints::render_return_type_without_a_tree(&text),
        };
        (text, slot)
    }

    #[test]
    fn the_render_return_type_slot_is_the_generated_header_close_paren() {
        // The second source is #4464's own carrier. An `import` moves the `;`
        // that precedes the header onto its own line, which is why a literal
        // needle answered `None` on every component that imports anything.
        for (name, source) in [
            (
                "render-return",
                "<script>let value = 1;</script><p>{value}</p>",
            ),
            (
                "render-return-import",
                "<script lang=\"ts\">\n  import Comp from './Comp.svelte';\n  const n = 1;\n</script>\n<Comp a={n} />",
            ),
        ] {
            let (text, slot) = render_return_type_slot(source, name);
            let offset = slot.expect("a projected component carries a $$render header") as usize;
            assert!(
                crate::tsgo_inlay_hints::ShadowNodes::parse(&text).is_some(),
                "liveness: {name} must exercise the tree, not the fallback"
            );
            assert_eq!(&text[offset - 1..offset], ")", "{name}");
            assert!(text[..offset].ends_with("$$render()"), "{name}");
        }
    }

    /// The completion fixtures are deliberately unparseable (`new A().`), and
    /// oxc recovers nothing from them — `fatal_error`, zero statements — so the
    /// tree the other filters need does not exist. Upstream keeps filtering
    /// there because TypeScript's `SourceFile` is best-effort; the fallback is
    /// what keeps rsvelte from answering one hint official does not.
    #[test]
    fn an_unparseable_shadow_still_finds_the_render_return_type_slot() {
        let source = "<script>class A { b() { return true; } } new A().</script>";
        let (text, slot) = render_return_type_slot(source, "render-return-unparseable");
        assert!(
            crate::tsgo_inlay_hints::ShadowNodes::parse(&text).is_none(),
            "liveness: this shadow must be the one oxc rejects"
        );
        let offset = slot.expect("the fallback answers where the tree cannot") as usize;
        assert_eq!(&text[offset - 1..offset], ")");
        assert!(text[..offset].ends_with("$$render()"));
    }

    #[test]
    fn a_forged_render_header_in_user_text_does_not_move_the_slot() {
        // svelte2tsx hoists a props type annotation verbatim, so a string
        // literal can forge the header's text above the real one. The tree
        // reads it as a string; a text needle read it as a second header.
        let source = "<script lang=\"ts\">\n  let { a }: { a: \";function $$render() {\" } = $props();\n</script>\n<p>{a}</p>";
        let (text, slot) = render_return_type_slot(source, "render-return-forged");
        assert_eq!(
            text.matches(";function $$render() {").count(),
            2,
            "liveness: the oracle must still splice the forged text above the header"
        );
        let offset = slot.expect("the forged text must not suppress the real header") as usize;
        assert!(text[..offset].ends_with("$$render()"));
        assert!(
            offset > text.rfind(";function $$render() {").unwrap(),
            "the slot must be the real header, not the forged one"
        );
    }

    #[test]
    fn generated_ignore_regions_are_not_mapped_back() {
        let text = "a/*Ωignore_startΩ*/hidden/*Ωignore_endΩ*/b";
        let ranges = ignored_ranges(text);
        assert_eq!(ranges.len(), 1);
        assert!(ranges[0].contains(&text.find("hidden").unwrap()));
        assert!(!ranges[0].contains(&0));

        let workspace = TestWorkspace::new("ignore-map");
        let app = workspace.0.join("App.svelte");
        write(&app, "<script>export let value;</script><p>{value}</p>");
        let overlay = build_overlay(&workspace.0).unwrap();
        let shadow_path = overlay.shadow_dir.join("App.svelte.tsx");
        let shadow = overlay.shadow_for_source(&app).unwrap();
        let ignored = shadow.text.find(IGNORE_START).unwrap() + IGNORE_START.len();
        let position = utf8_position(&shadow.text, ignored);
        assert!(overlay.is_generated_position(&shadow_path, position));
        assert_eq!(overlay.map_generated_position(&shadow_path, position), None);
    }

    #[test]
    fn the_cache_root_ignores_itself_so_opening_an_editor_leaves_git_status_clean() {
        let workspace = TestWorkspace::new("gitignore");
        write(
            &workspace.0.join("App.svelte"),
            "<script>export let value;</script><p>{value}</p>",
        );
        build_overlay(&workspace.0).unwrap();
        let ignore = workspace.0.join(CACHE_DIRECTORY).join(".gitignore");
        assert_eq!(fs::read_to_string(&ignore).unwrap(), "*\n");
    }

    #[test]
    fn rebuilding_over_an_existing_cache_keeps_the_ignore_file() {
        // The overlay is rebuilt on config changes and on every server start,
        // and `git clean` / a manual delete can take the marker with it.
        let workspace = TestWorkspace::new("gitignore-rebuild");
        write(&workspace.0.join("App.svelte"), "<p>hi</p>");
        build_overlay(&workspace.0).unwrap();
        let ignore = workspace.0.join(CACHE_DIRECTORY).join(".gitignore");
        fs::remove_file(&ignore).unwrap();
        build_overlay(&workspace.0).unwrap();
        assert_eq!(fs::read_to_string(&ignore).unwrap(), "*\n");
    }

    #[cfg(unix)]
    #[test]
    fn cache_symlink_is_rejected_before_any_write_through_it() {
        use std::os::unix::fs::symlink;

        let workspace = TestWorkspace::new("symlink");
        let outside = TestWorkspace::new("outside");
        let cache_parent = workspace.0.join(CACHE_DIRECTORY);
        fs::create_dir_all(&cache_parent).unwrap();
        symlink(&outside.0, cache_parent.join(TSGO_DIRECTORY)).unwrap();
        let error = build_overlay(&workspace.0).unwrap_err();
        assert!(error.to_string().contains("symlink"));
        assert!(fs::read_dir(&outside.0).unwrap().next().is_none());
    }

    #[test]
    fn a_component_without_a_typescript_script_opens_as_javascript() {
        // tsgo decides the script kind from the language id, not from the
        // `.tsx` shadow name, so this is what keeps TypeScript-only keywords
        // out of a plain component's completions.
        assert_eq!(
            shadow_language_id("<script>let a = 1;</script>"),
            "javascriptreact"
        );
        assert_eq!(shadow_language_id("<div>{a}</div>"), "javascriptreact");
        assert_eq!(
            shadow_language_id("<script lang=\"ts\">let a: number = 1;</script>"),
            "typescriptreact"
        );
        assert_eq!(
            shadow_language_id("<script module lang=\"ts\">export const a = 1;</script>"),
            "typescriptreact"
        );
    }

    /// A `svelte` install whose bundled declarations are shaped like the real
    /// one: the ambient `*.svelte` wildcard plus named modules.
    fn write_svelte_package(root: &Path, svelte_html: bool) -> PathBuf {
        let package = root.join("node_modules/svelte");
        write(&package.join("package.json"), r#"{"version":"5.57.0"}"#);
        write(
            &package.join("types/index.d.ts"),
            "declare module 'svelte' {\n\texport function mount(): void;\n}\n\
             declare module 'svelte/transition' {\n\texport function fade(): void;\n}\n\
             declare module '*.svelte' {\n\texport default 1;\n}\n",
        );
        if svelte_html {
            write(&package.join("svelte-html.d.ts"), "");
        }
        package
    }

    fn overlay_config(overlay: &TsgoOverlay) -> serde_json::Value {
        serde_json::from_str(&fs::read_to_string(overlay.tsconfig_path()).unwrap()).unwrap()
    }

    #[test]
    fn a_project_without_node_modules_types_against_the_svelte_beside_the_server() {
        // Upstream resolves `svelte` with `paths = [workspace, __dirname]`, so a
        // project with no `node_modules` keeps every `svelte/*` type instead of
        // losing them to a 2307 (#4101).
        let workspace = TestWorkspace::new("svelte-fallback");
        let server = TestWorkspace::new("svelte-fallback-server");
        write_svelte_package(&server.0, false);
        write(&workspace.0.join("src/App.svelte"), "<p />");

        let overlay = TsgoOverlay::build_with(&workspace.0, None, Some(&server.0)).unwrap();
        let config = overlay_config(&overlay);
        let shadow = path_for_tsconfig(&overlay.cache_dir().join("svelte-types.d.ts"));
        assert_eq!(
            config["compilerOptions"]["paths"]["svelte/transition"],
            json!([shadow.clone()])
        );
        assert_eq!(
            config["compilerOptions"]["paths"]["svelte"],
            json!([shadow.clone()])
        );
        // The ambient `*.svelte` wildcard must not come with them, or a missing
        // component would type as a default-only import.
        assert!(config["compilerOptions"]["paths"]["*.svelte"].is_null());
        assert!(config["files"].as_array().unwrap().contains(&json!(shadow)));
    }

    #[test]
    fn a_project_with_its_own_svelte_is_left_alone() {
        // Live negative control for the test above: tsgo's own module
        // resolution reaches an installed `svelte`, so nothing is overridden.
        let workspace = TestWorkspace::new("svelte-installed");
        let server = TestWorkspace::new("svelte-installed-server");
        write_svelte_package(&workspace.0, false);
        write_svelte_package(&server.0, false);
        write(&workspace.0.join("src/App.svelte"), "<p />");

        let overlay = TsgoOverlay::build_with(&workspace.0, None, Some(&server.0)).unwrap();
        let config = overlay_config(&overlay);
        assert!(config["compilerOptions"]["paths"].is_null());
        assert!(!overlay.cache_dir().join("svelte-types.d.ts").is_file());
    }

    #[test]
    fn a_project_that_maps_svelte_itself_keeps_its_own_mapping() {
        let workspace = TestWorkspace::new("svelte-user-mapping");
        let server = TestWorkspace::new("svelte-user-mapping-server");
        write_svelte_package(&server.0, false);
        write(
            &workspace.0.join("tsconfig.json"),
            r#"{ "compilerOptions": { "paths": { "svelte": ["./vendor/svelte.d.ts"] } } }"#,
        );
        write(&workspace.0.join("vendor/svelte.d.ts"), "export {};");
        write(&workspace.0.join("src/App.svelte"), "<p />");

        let overlay = TsgoOverlay::build_with(&workspace.0, None, Some(&server.0)).unwrap();
        let config = overlay_config(&overlay);
        assert_eq!(
            config["compilerOptions"]["paths"]["svelte"],
            json!([
                path_for_tsconfig(&overlay.workspace().join("vendor/svelte.d.ts")),
                path_for_tsconfig(&overlay.shadow_dir.join("vendor/svelte.d.ts"))
            ])
        );
        // The subpath it did not map still comes from the fallback.
        assert_eq!(
            config["compilerOptions"]["paths"]["svelte/transition"],
            json!([path_for_tsconfig(
                &overlay.cache_dir().join("svelte-types.d.ts")
            )])
        );
    }

    #[test]
    fn the_fallback_package_supplies_svelte_html_in_place_of_the_jsx_shim() {
        let workspace = TestWorkspace::new("svelte-fallback-html");
        let server = TestWorkspace::new("svelte-fallback-html-server");
        write_svelte_package(&server.0, true);
        let package = fs::canonicalize(&server.0)
            .unwrap()
            .join("node_modules/svelte");
        write(&workspace.0.join("src/App.svelte"), "<p />");

        let overlay = TsgoOverlay::build_with(&workspace.0, None, Some(&server.0)).unwrap();
        let files = overlay_config(&overlay)["files"].clone();
        let files = files.as_array().unwrap();
        assert!(files.contains(&json!(path_for_tsconfig(&package.join("svelte-html.d.ts")))));
        assert!(!files.contains(&json!(rsvelte_check::overlay::SHIM_JSX_V4_NAME)));
    }

    #[test]
    fn without_a_fallback_root_nothing_changes() {
        let workspace = TestWorkspace::new("svelte-no-fallback");
        write(&workspace.0.join("src/App.svelte"), "<p />");

        let config = overlay_config(&build_overlay(&workspace.0).unwrap());
        assert!(config["compilerOptions"]["paths"].is_null());
        assert!(
            config["files"]
                .as_array()
                .unwrap()
                .contains(&json!(rsvelte_check::overlay::SHIM_JSX_V4_NAME))
        );
    }

    #[test]
    fn base_url_is_translated_into_the_paths_tsgo_still_honours() {
        // tsgo removed `baseUrl`; TypeScript 5.x, which the official server
        // runs, still honours it, so a `baseUrl`-rooted import kept its type
        // there and lost it here (#4392).
        let workspace = TestWorkspace::new("base-url");
        write(
            &workspace.0.join("tsconfig.json"),
            r#"{ "compilerOptions": { "baseUrl": "." } }"#,
        );
        write(&workspace.0.join("src/App.svelte"), "<p />");

        let overlay = build_overlay(&workspace.0).unwrap();
        let config = overlay_config(&overlay);
        assert_eq!(
            config["compilerOptions"]["paths"]["*"],
            json!([
                format!("{}/*", path_for_tsconfig(overlay.workspace())),
                format!("{}/*", path_for_tsconfig(&overlay.shadow_dir))
            ])
        );
    }

    #[test]
    fn base_url_resolves_against_the_config_that_declares_it() {
        let workspace = TestWorkspace::new("base-url-extends");
        write(
            &workspace.0.join("configs/base.json"),
            r#"{ "compilerOptions": { "baseUrl": "../app" } }"#,
        );
        write(
            &workspace.0.join("tsconfig.json"),
            r#"{ "extends": "./configs/base.json" }"#,
        );
        write(&workspace.0.join("app/src/App.svelte"), "<p />");

        let overlay = build_overlay(&workspace.0).unwrap();
        let config = overlay_config(&overlay);
        assert_eq!(
            config["compilerOptions"]["paths"]["*"][0],
            json!(format!(
                "{}/*",
                path_for_tsconfig(&overlay.workspace().join("app"))
            ))
        );
    }

    #[test]
    fn a_project_that_maps_the_star_pattern_itself_keeps_its_own() {
        let workspace = TestWorkspace::new("base-url-star");
        write(
            &workspace.0.join("tsconfig.json"),
            r#"{ "compilerOptions": { "baseUrl": ".", "paths": { "*": ["./vendor/*"] } } }"#,
        );
        write(&workspace.0.join("src/App.svelte"), "<p />");

        let overlay = build_overlay(&workspace.0).unwrap();
        let config = overlay_config(&overlay);
        assert_eq!(
            config["compilerOptions"]["paths"]["*"],
            json!([
                path_for_tsconfig(&overlay.workspace().join("vendor/*")),
                path_for_tsconfig(&overlay.shadow_dir.join("vendor/*"))
            ])
        );
    }

    #[test]
    fn a_project_without_base_url_gains_no_star_pattern() {
        // Live negative control: the translation must not appear on its own.
        let workspace = TestWorkspace::new("base-url-absent");
        write(
            &workspace.0.join("tsconfig.json"),
            r#"{ "compilerOptions": { "strict": true } }"#,
        );
        write(&workspace.0.join("src/App.svelte"), "<p />");

        let config = overlay_config(&build_overlay(&workspace.0).unwrap());
        assert!(config["compilerOptions"]["paths"].is_null());
    }

    #[test]
    fn a_svelte_module_specifier_maps_back_without_the_appended_tsx() {
        let workspace = TestWorkspace::new("svelte-specifier-hover");
        let app = workspace.0.join("src/App.svelte");
        let source =
            "<script lang=\"ts\">\n\timport Other from './Other.svelte';\n</script>\n\n<Other />\n";
        write(&app, source);
        write(&workspace.0.join("src/Other.svelte"), "<p>other</p>\n");
        let overlay = build_overlay(&workspace.0).unwrap();
        let shadow = overlay.shadow_for_source(&app).unwrap();
        let shadow_path = crate::uri::uri_to_path(shadow.shadow_uri.as_str());
        let index = LineIndex::new(&shadow.text);
        let literal = "'./Other.svelte.tsx'";
        let at = shadow.text.find(literal).expect("rewritten specifier");
        let range = Range::new(
            index.position(&shadow.text, at),
            index.position(&shadow.text, at + literal.len()),
        );
        assert!(!overlay.is_generated_range(&shadow_path, range));

        let source_index = LineIndex::new(source);
        let source_literal = "'./Other.svelte'";
        let source_at = source.find(source_literal).unwrap();
        assert_eq!(
            overlay.map_generated_range(&shadow_path, range),
            Some(Range::new(
                source_index.position(source, source_at),
                source_index.position(source, source_at + source_literal.len()),
            ))
        );

        // The four bytes themselves stay generated: they have no source at all.
        let suffix = at + "'./Other.svelte".len();
        let suffix_range = Range::new(
            index.position(&shadow.text, suffix),
            index.position(&shadow.text, suffix + ".tsx".len()),
        );
        assert!(overlay.is_generated_range(&shadow_path, suffix_range));
        assert_eq!(
            overlay.map_generated_range(&shadow_path, suffix_range),
            None
        );
        assert!(
            overlay.is_generated_position(&shadow_path, index.position(&shadow.text, suffix + 1))
        );

        // A request range that merely runs *through* the appended `.tsx` is a
        // different question from a response range that sits on it, and the
        // specifier is the case where the two answers differ: the editor's range
        // covers four bytes with no source, so the request is still dropped
        // (#4464's `0:0` viewport maps to a range of exactly this shape), while
        // the response range maps back to the source specifier.
        assert!(overlay.touches_generated_range(&shadow_path, range));
        assert!(!overlay.is_generated_range(&shadow_path, range));
    }

    #[test]
    fn a_nested_config_owns_the_documents_beneath_it() {
        // `findTsConfigPath` searches from the DOCUMENT's directory upward, so a
        // subdirectory carrying its own config is its own project (#4391).
        let workspace = TestWorkspace::new("nearest-tsconfig");
        let root = fs::canonicalize(&workspace.0).unwrap();
        write(&root.join("tsconfig.json"), "{}");
        write(&root.join("docs/tsconfig.json"), "{}");
        write(&root.join("docs/src/App.svelte"), "<p />");
        write(&root.join("src/App.svelte"), "<p />");

        assert_eq!(
            nearest_tsconfig(&root.join("docs/src/App.svelte"), &root),
            Some(root.join("docs/tsconfig.json"))
        );
        assert_eq!(
            nearest_tsconfig(&root.join("src/App.svelte"), &root),
            Some(root.join("tsconfig.json"))
        );
    }

    #[test]
    fn a_jsconfig_answers_only_where_no_tsconfig_does() {
        let workspace = TestWorkspace::new("nearest-jsconfig");
        let root = fs::canonicalize(&workspace.0).unwrap();
        write(&root.join("docs/jsconfig.json"), "{}");
        write(&root.join("docs/src/App.svelte"), "<p />");
        assert_eq!(
            nearest_tsconfig(&root.join("docs/src/App.svelte"), &root),
            Some(root.join("docs/jsconfig.json"))
        );

        // Upstream prefers the closest, and at one directory that is tsconfig.
        write(&root.join("docs/tsconfig.json"), "{}");
        assert_eq!(
            nearest_tsconfig(&root.join("docs/src/App.svelte"), &root),
            Some(root.join("docs/tsconfig.json"))
        );
    }

    #[test]
    fn the_search_stops_at_the_boundary_and_at_node_modules() {
        let workspace = TestWorkspace::new("nearest-boundary");
        let root = fs::canonicalize(&workspace.0).unwrap();
        write(&root.join("tsconfig.json"), "{}");
        write(&root.join("node_modules/dep/src/App.svelte"), "<p />");
        write(&root.join("plain/App.svelte"), "<p />");

        // A dependency's document does not join the workspace project.
        assert_eq!(
            nearest_tsconfig(&root.join("node_modules/dep/src/App.svelte"), &root),
            None
        );
        // Nothing above the boundary is reachable either, even though this
        // workspace's own config would otherwise answer.
        assert_eq!(
            nearest_tsconfig(&root.join("plain/App.svelte"), &root.join("plain")),
            None
        );
        assert_eq!(
            nearest_tsconfig(&root.join("plain/App.svelte"), &root),
            Some(root.join("tsconfig.json"))
        );
    }

    #[test]
    fn a_nested_project_shadows_the_svelte_files_it_imports_from_beside_it() {
        // upstream-testfiles' `different-ts-service`: the project directory
        // imports `../shared-comp.svelte`, which only a shadow at the same
        // position relative to the workspace can resolve.
        let workspace = TestWorkspace::new("nested-outside-import");
        let root = fs::canonicalize(&workspace.0).unwrap();
        write(&root.join("tsconfig.json"), "{}");
        write(&root.join("nested/tsconfig.json"), "{}");
        write(
            &root.join("nested/App.svelte"),
            "<script lang=\"ts\">import { foo } from '../shared.svelte';</script>",
        );
        write(
            &root.join("shared.svelte"),
            "<script lang=\"ts\">import X from './deeper/Leaf.svelte';</script>",
        );
        write(&root.join("deeper/Leaf.svelte"), "<p />");
        write(&root.join("unrelated.svelte"), "<p />");

        let nested = TsgoOverlay::build_nested(
            &root.join("nested"),
            &root.join("nested/tsconfig.json"),
            &root,
        )
        .unwrap();
        let shadowed = |path: &str| {
            nested
                .shadow_for_source(&root.join(path))
                .map(|doc| doc.shadow_uri.clone())
        };
        assert!(shadowed("nested/App.svelte").is_some());
        assert_eq!(
            shadowed("shared.svelte"),
            Some(path_to_uri(&nested.shadow_dir.join("shared.svelte.tsx")).unwrap()),
            "the import leaves the project, so its shadow sits where `../` finds it"
        );
        assert!(
            shadowed("deeper/Leaf.svelte").is_some(),
            "followed transitively"
        );
        assert!(
            shadowed("unrelated.svelte").is_none(),
            "only what is imported"
        );
        assert_eq!(
            overlay_config(&nested)["compilerOptions"]["rootDirs"][0],
            json!(path_for_tsconfig(&root))
        );

        // Live control: a project that is not nested mirrors itself.
        let rooted = build_overlay(&root).unwrap();
        assert_eq!(rooted.mirror_root(), rooted.workspace());
    }

    #[test]
    fn a_project_owns_only_documents_whose_nearest_config_it_is() {
        let workspace = TestWorkspace::new("ownership");
        let root = fs::canonicalize(&workspace.0).unwrap();
        write(&root.join("tsconfig.json"), "{}");
        write(&root.join("src/App.svelte"), "<p />");
        write(&root.join("docs/tsconfig.json"), "{}");
        write(&root.join("docs/src/Page.svelte"), "<p />");
        write(&root.join("tools/jsconfig.json"), "{}");
        write(&root.join("tools/Tool.svelte"), "<p />");

        let overlay = build_overlay(&root).unwrap();
        assert!(overlay.owns(&root.join("src/App.svelte")));
        for foreign in ["docs/src/Page.svelte", "tools/Tool.svelte"] {
            assert!(!overlay.owns(&root.join(foreign)), "{foreign}");
            assert!(
                overlay.shadow_for_source(&root.join(foreign)).is_none(),
                "{foreign} is another project's, and nothing here imports it"
            );
        }
        assert_eq!(
            nearest_tsconfig(&root.join("docs/src/Page.svelte"), &root),
            Some(root.join("docs/tsconfig.json")),
            "ownership agrees with the per-document config lookup"
        );
    }

    #[test]
    fn a_configless_root_owns_only_documents_without_any_config() {
        let workspace = TestWorkspace::new("ownership-configless");
        let root = fs::canonicalize(&workspace.0).unwrap();
        write(&root.join("App.svelte"), "<p />");
        write(&root.join("lib/Button.svelte"), "<p />");
        write(&root.join("app/tsconfig.json"), "{}");
        write(&root.join("app/Page.svelte"), "<p />");
        let overlay = build_overlay(&root).unwrap();
        assert!(overlay.owns(&root.join("App.svelte")));
        assert!(overlay.owns(&root.join("lib/Button.svelte")));
        assert!(!overlay.owns(&root.join("app/Page.svelte")));
        assert_eq!(overlay.eager_shadows().len(), 2);
    }

    #[test]
    fn a_parallel_load_projects_exactly_what_one_thread_does() {
        let workspace = TestWorkspace::new("parallel-load");
        let root = fs::canonicalize(&workspace.0).unwrap();
        for index in 0..40 {
            write(
                &root.join(format!("src/c{}/C{index}.svelte", index % 5)),
                &format!(
                    "<script lang=\"ts\">\n\tlet {{ n = {index} }}: {{ n?: number }} = $props();\n</script>\n<p>{{n}}</p>\n"
                ),
            );
        }
        write(
            &root.join("src/Broken.svelte"),
            "<script lang=\"ts\">let a = 1;</script>{#if}",
        );
        let parallel = build_overlay(&root).unwrap();
        assert_eq!(parallel.component_shadows().len(), 41);
        let mut sequential = TsgoOverlay::build_mirrored(&root, None, None, None, false).unwrap();
        for shadow in parallel.component_shadows() {
            let path = crate::uri::uri_to_path(shadow.source_uri.as_str());
            let text = fs::read_to_string(&path).unwrap();
            let expected = sequential.open_or_update(&path, &text, 0).unwrap();
            assert_eq!(&expected, shadow, "{}", path.display());
            assert_eq!(sequential.source_map(&path), parallel.source_map(&path));
            assert_eq!(sequential.parser_error(&path), parallel.parser_error(&path));
        }
        assert!(
            parallel
                .parser_error(&root.join("src/Broken.svelte"))
                .is_some()
        );
    }

    #[test]
    fn file_events_touch_only_the_files_they_name() {
        use lsp_types::FileChangeType;
        let workspace = TestWorkspace::new("file-events");
        let root = fs::canonicalize(&workspace.0).unwrap();
        write(&root.join("app/tsconfig.json"), "{}");
        write(&root.join("app/A.svelte"), "<p>a</p>");
        write(&root.join("app/B.svelte"), "<p>b</p>");
        write(&root.join("app/nested/tsconfig.json"), "{}");
        write(
            &root.join("lib/Shared.svelte"),
            "<script lang=\"ts\">import Leaf from './Leaf.svelte';</script>",
        );
        write(&root.join("lib/Leaf.svelte"), "<p />");
        let mut overlay =
            TsgoOverlay::build_nested(&root.join("app"), &root.join("app/tsconfig.json"), &root)
                .unwrap();
        let none = EditorBuffers::default();
        let changed = |overlay: &mut TsgoOverlay, path: &Path, change| {
            overlay
                .apply_file_changes(&[(path.to_path_buf(), change)], &none)
                .unwrap()
                .expect("placeable")
        };

        write(&root.join("app/A.svelte"), "<p>a2</p>");
        let update = changed(
            &mut overlay,
            &root.join("app/A.svelte"),
            FileChangeType::CHANGED,
        );
        assert_eq!(update.opened_or_changed.len(), 1);
        assert_eq!(
            overlay.source_text(&root.join("app/A.svelte")),
            Some("<p>a2</p>")
        );
        assert!(
            changed(
                &mut overlay,
                &root.join("README.md"),
                FileChangeType::CHANGED
            )
            .opened_or_changed
            .is_empty()
        );

        write(&root.join("app/C.svelte"), "<p>c</p>");
        let update = changed(
            &mut overlay,
            &root.join("app/C.svelte"),
            FileChangeType::CREATED,
        );
        assert_eq!(update.opened_or_changed.len(), 1);
        assert!(overlay.owns(&root.join("app/C.svelte")));
        write(&root.join("app/nested/D.svelte"), "<p>d</p>");
        changed(
            &mut overlay,
            &root.join("app/nested/D.svelte"),
            FileChangeType::CREATED,
        );
        assert!(
            overlay
                .shadow_for_source(&root.join("app/nested/D.svelte"))
                .is_none(),
            "a nested config's component is its own project's"
        );

        // A new import of another project's component brings a copy, and its
        // imports too.
        write(
            &root.join("app/B.svelte"),
            "<script lang=\"ts\">import Shared from '../lib/Shared.svelte';\nimport Soon from './Soon.svelte';</script>",
        );
        let update = changed(
            &mut overlay,
            &root.join("app/B.svelte"),
            FileChangeType::CHANGED,
        );
        assert_eq!(update.opened_or_changed.len(), 3, "B, Shared, Leaf");
        assert!(
            overlay
                .shadow_for_source(&root.join("lib/Leaf.svelte"))
                .is_some()
        );
        assert!(!overlay.owns(&root.join("lib/Shared.svelte")));
        // ...and an import of a file that does not exist yet resolves once it does.
        write(&root.join("app/Soon.svelte"), "<p />");
        changed(
            &mut overlay,
            &root.join("app/Soon.svelte"),
            FileChangeType::CREATED,
        );
        assert!(
            overlay
                .shadow_for_source(&root.join("app/Soon.svelte"))
                .is_some()
        );

        overlay.persist_body(&root.join("app/C.svelte")).unwrap();
        let body = overlay.shadow_dir.join("app/C.svelte.tsx");
        assert!(body.is_file());
        fs::remove_file(root.join("app/C.svelte")).unwrap();
        let update = changed(
            &mut overlay,
            &root.join("app/C.svelte"),
            FileChangeType::DELETED,
        );
        assert_eq!(update.closed.len(), 1);
        assert!(!body.exists());
        assert!(!overlay.owns(&root.join("app/C.svelte")));

        write(&root.join("app/A.svelte"), "<p>a3</p>");
        let a = root.join("app/A.svelte");
        let mut editor = EditorBuffers::default();
        editor.insert(a.clone(), Arc::new("<p>a2</p>".to_string()), 2);
        let skipped = overlay
            .apply_file_changes(&[(a.clone(), FileChangeType::CHANGED)], &editor)
            .unwrap()
            .unwrap();
        assert!(
            skipped.opened_or_changed.is_empty(),
            "an editor buffer wins"
        );

        assert!(
            overlay
                .apply_file_changes(&[(root.join("app"), FileChangeType::CHANGED)], &none)
                .unwrap()
                .is_none(),
            "a directory needs a full refresh"
        );
        assert!(
            overlay
                .apply_file_changes(
                    &[(root.join("app/node_modules/x"), FileChangeType::CREATED)],
                    &none
                )
                .unwrap()
                .is_some(),
            "a node_modules path holds no member"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_nested_packages_node_modules_is_mirrored_into_the_cache() {
        let workspace = TestWorkspace::new("node-modules");
        let root = fs::canonicalize(&workspace.0).unwrap();
        write(&root.join("node_modules/root-pkg/index.d.ts"), "export {};");
        write(
            &root.join("admin/node_modules/admin-pkg/index.d.ts"),
            "export {};",
        );
        write(&root.join("admin/src/tsconfig.json"), "{}");
        write(&root.join("admin/src/ui/A.svelte"), "<p />");
        let overlay = TsgoOverlay::build_nested(
            &root.join("admin/src"),
            &root.join("admin/src/tsconfig.json"),
            &root,
        )
        .unwrap();
        let cache_root = root.join(CACHE_DIRECTORY);
        let linked = |link: PathBuf| fs::read_link(&link).ok();
        assert_eq!(
            linked(overlay.shadow_dir.join("admin/node_modules")),
            Some(root.join("admin/node_modules")),
            "a shadow finds the package where its source does"
        );
        assert_eq!(
            linked(overlay.shadow_dir.join("node_modules")),
            None,
            "the mirror root's own is found through the cache's ancestry"
        );
        assert_eq!(
            linked(cache_root.join("projects/admin/node_modules")),
            Some(root.join("admin/node_modules")),
            "the config's `types` and `typeRoots` walk the project's ancestry"
        );
        assert!(
            !cache_root.join("projects/admin/src/node_modules").exists(),
            "only directories that have one"
        );
        // Idempotent, and the sweep leaves the links alone.
        overlay.link_node_modules();
        assert!(overlay.sweep_bodies(|_| true).is_empty());
        assert!(
            overlay
                .shadow_dir
                .join("admin/node_modules/admin-pkg/index.d.ts")
                .is_file()
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_dependencys_component_gets_a_real_shadow_directory() {
        let workspace = TestWorkspace::new("node-modules-member");
        let root = fs::canonicalize(&workspace.0).unwrap();
        write(&root.join("tsconfig.json"), "{}");
        write(&root.join("src/App.svelte"), "<p />");
        write(&root.join("node_modules/pkg/dist/Comp.svelte"), "<p />");
        write(&root.join("lib/Card.svelte"), "<p />");
        write(&root.join("lib/node_modules/dep/Dep.svelte"), "<p />");
        let mut overlay = build_overlay(&root).unwrap();
        assert!(
            overlay.shadow_dir.join("lib/node_modules").is_symlink(),
            "a member's own package directory is mirrored"
        );
        for library in [
            "node_modules/pkg/dist/Comp.svelte",
            "lib/node_modules/dep/Dep.svelte",
        ] {
            let shadow = overlay
                .open_or_update(&root.join(library), "<p />", 1)
                .unwrap_or_else(|error| panic!("{library}: {error}"));
            let shadow_path = crate::uri::uri_to_path(shadow.shadow_uri.as_str());
            assert!(shadow_path.starts_with(&overlay.shadow_dir), "{library}");
            assert!(shadow_path.parent().unwrap().is_dir());
        }
        assert!(!overlay.shadow_dir.join("lib/node_modules").is_symlink());
        assert!(
            !root.join("lib/node_modules/dep/Dep.svelte.tsx").exists(),
            "nothing is written into the dependency"
        );
        overlay.link_node_modules();
        assert!(
            !overlay.shadow_dir.join("lib/node_modules").is_symlink(),
            "a member keeps the directory real"
        );
    }

    #[test]
    fn a_deferred_overlay_projects_nothing_until_loaded() {
        let workspace = TestWorkspace::new("deferred");
        let root = fs::canonicalize(&workspace.0).unwrap();
        write(&root.join("src/App.svelte"), "<p />");
        let mut overlay = TsgoOverlay::build_mirrored(&root, None, None, None, false).unwrap();
        assert!(!overlay.is_loaded());
        assert!(overlay.eager_shadows().is_empty());
        assert!(
            overlay.tsconfig_path().is_file(),
            "the config is written up front"
        );
        assert!(overlay.cache_dir().join(SHIM_SHIMS_V4_NAME).is_file());

        assert!(overlay.load().unwrap());
        assert!(!overlay.load().unwrap(), "loads once");
        assert_eq!(overlay.eager_shadows().len(), 1);
        assert!(overlay.owns(&root.join("src/App.svelte")));
    }

    #[test]
    fn the_import_closure_follows_paths_aliases_from_owned_plain_modules() {
        // monorepo/admin/tests: a project with no component of its own, whose
        // tests import the app's components through an alias.
        let workspace = TestWorkspace::new("alias-closure");
        let root = fs::canonicalize(&workspace.0).unwrap();
        write(&root.join("src/tsconfig.json"), "{}");
        write(
            &root.join("src/app/Error.svelte"),
            "<script lang=\"ts\">import Icon from '../lib/Icon.svelte';</script><Icon />",
        );
        write(&root.join("src/lib/Icon.svelte"), "<svg />");
        write(&root.join("src/lib/Unused.svelte"), "<p />");
        write(
            &root.join("tests/tsconfig.json"),
            r#"{ "compilerOptions": { "paths": { "@app/*": ["../src/*"] } } }"#,
        );
        write(
            &root.join("tests/browser/router.test.ts"),
            "import Error from '@app/app/Error.svelte';\nexport { Error };\n",
        );

        let mut tests = TsgoOverlay::build_nested(
            &root.join("tests"),
            &root.join("tests/tsconfig.json"),
            &root,
        )
        .unwrap();
        let error = root.join("src/app/Error.svelte");
        assert!(tests.shadow_for_source(&error).is_some(), "aliased import");
        assert!(
            tests
                .shadow_for_source(&root.join("src/lib/Icon.svelte"))
                .is_some(),
            "followed transitively"
        );
        assert!(
            tests
                .shadow_for_source(&root.join("src/lib/Unused.svelte"))
                .is_none()
        );
        assert!(!tests.owns(&error), "a copy, not a member the project owns");
        assert_eq!(
            tests.shadow_for_source(&error).unwrap().shadow_uri,
            path_to_uri(&tests.shadow_dir.join("src/app/Error.svelte.tsx")).unwrap(),
            "where the alias's shadow-tree twin points"
        );
        for shadow in tests.component_shadows() {
            tests
                .persist_body(&uri_to_path_for_test(&shadow.source_uri))
                .unwrap();
        }
        assert!(tests.unresolved_shadow_routes().is_empty());
        assert!(!tests.load().unwrap());
    }

    fn uri_to_path_for_test(uri: &Uri) -> PathBuf {
        crate::uri::uri_to_path(uri.as_str())
    }

    #[test]
    fn an_editor_documents_new_import_is_followed_before_and_after_a_save() {
        use lsp_types::FileChangeType;
        let workspace = TestWorkspace::new("open-importer");
        let root = fs::canonicalize(&workspace.0).unwrap();
        write(&root.join("a/tsconfig.json"), "{}");
        write(&root.join("a/Plain.svelte"), "<p />");
        write(&root.join("b/tsconfig.json"), "{}");
        write(&root.join("b/New.svelte"), "<p />");
        write(&root.join("b/Other.svelte"), "<p />");
        let plain = root.join("a/Plain.svelte");
        let mut a =
            TsgoOverlay::build_nested(&root.join("a"), &root.join("a/tsconfig.json"), &root)
                .unwrap();
        let edited = "<script lang=\"ts\">import New from '../b/New.svelte';</script><New />";
        a.open_or_update(&plain, edited, 2).unwrap();
        let mut editor = EditorBuffers::default();
        editor.insert(plain.clone(), Arc::new(edited.to_string()), 2);

        // Typed, not saved: the keystroke path copies the import's target.
        assert!(a.has_new_imports(&plain, edited, |_| false));
        let update = a.follow_buffer_imports(&plain, edited, &editor).unwrap();
        assert_eq!(update.opened_or_changed.len(), 1, "b/New.svelte, from disk");
        assert!(a.shadow_for_source(&root.join("b/New.svelte")).is_some());
        assert!(
            !a.has_new_imports(&plain, edited, |_| false),
            "nothing left to follow"
        );

        // Saved with another new import: the event for the open document still
        // follows its imports (from the buffer), without re-projecting it.
        let saved = "<script lang=\"ts\">import New from '../b/New.svelte';\nimport Other from '../b/Other.svelte';</script><New /><Other />";
        a.open_or_update(&plain, saved, 3).unwrap();
        editor.insert(plain.clone(), Arc::new(saved.to_string()), 3);
        write(&plain, saved);
        let update = a
            .apply_file_changes(&[(plain.clone(), FileChangeType::CHANGED)], &editor)
            .unwrap()
            .expect("placeable");
        assert_eq!(update.opened_or_changed.len(), 1, "only the new copy");
        assert!(a.shadow_for_source(&root.join("b/Other.svelte")).is_some());
        assert_eq!(
            a.source_text(&plain),
            Some(saved),
            "the buffer is left alone"
        );
    }

    #[test]
    fn a_component_opened_before_it_exists_is_owned_by_its_project() {
        let workspace = TestWorkspace::new("owned-on-open");
        let root = fs::canonicalize(&workspace.0).unwrap();
        write(&root.join("b/tsconfig.json"), "{}");
        write(&root.join("b/Other.svelte"), "<p />");
        let fresh = root.join("b/Fresh.svelte");
        let mut b =
            TsgoOverlay::build_nested(&root.join("b"), &root.join("b/tsconfig.json"), &root)
                .unwrap();
        // The editor opens a file that is not on disk yet.
        b.open_or_update(&fresh, "<p />", 1).unwrap();
        assert!(b.owns(&fresh));
        // A copy another project projects is not owned there.
        write(&root.join("a/tsconfig.json"), "{}");
        let mut a =
            TsgoOverlay::build_nested(&root.join("a"), &root.join("a/tsconfig.json"), &root)
                .unwrap();
        a.open_or_update(&fresh, "<p />", 1).unwrap();
        assert!(!a.owns(&fresh));
    }

    #[test]
    fn deleting_a_directory_with_a_dot_in_its_name_removes_its_members() {
        use lsp_types::FileChangeType;
        let workspace = TestWorkspace::new("dotted-directory");
        let root = fs::canonicalize(&workspace.0).unwrap();
        write(&root.join("tsconfig.json"), "{}");
        write(&root.join("src/ui.v1/Button.svelte"), "<p />");
        write(&root.join("src/ui.v1.svelte"), "<p />");
        write(&root.join("src/Card.svelte"), "<p />");
        let mut overlay = build_overlay(&root).unwrap();
        let button = root.join("src/ui.v1/Button.svelte");
        overlay.persist_body(&button).unwrap();
        let body = overlay.shadow_dir.join("src/ui.v1/Button.svelte.tsx");
        assert!(body.is_file());
        // A file whose name extends the directory's sorts after the directory's
        // members, and is no member of it.
        assert!(!overlay.holds_members_under(&root.join("src/ui.v1.svelte")));
        assert!(overlay.holds_members_under(&root.join("src/ui.v1")));

        fs::remove_dir_all(root.join("src/ui.v1")).unwrap();
        let editor = EditorBuffers::default();
        assert!(
            overlay
                .apply_file_changes(
                    &[(root.join("src/ui.v1"), FileChangeType::DELETED)],
                    &editor
                )
                .unwrap()
                .is_none(),
            "a removed directory is not a file event"
        );
        let update = overlay.refresh_except(&editor).unwrap();
        assert_eq!(update.closed.len(), 1);
        assert!(overlay.shadow_for_source(&button).is_none());
        assert!(!body.exists());
        assert!(
            overlay
                .shadow_for_source(&root.join("src/ui.v1.svelte"))
                .is_some()
        );
    }

    #[test]
    fn javascript_components_an_open_document_reaches_are_the_ones_found() {
        let workspace = TestWorkspace::new("reachable-javascript");
        let root = fs::canonicalize(&workspace.0).unwrap();
        write(&root.join("tsconfig.json"), "{}");
        write(
            &root.join("src/Page.svelte"),
            "<script lang=\"ts\">import Card from './Card.svelte';</script><Card />",
        );
        write(
            &root.join("src/Card.svelte"),
            "<script>import Icon from './Icon.svelte';</script><Icon />",
        );
        write(&root.join("src/Icon.svelte"), "<p />");
        write(&root.join("src/Unreached.svelte"), "<p />");
        write(&root.join("src/Exported.svelte"), "<p />");
        write(
            &root.join("src/index.ts"),
            "export { default as Exported } from './Exported.svelte';\n",
        );
        let overlay = build_overlay(&root).unwrap();
        let names = |shadows: Vec<&ShadowDocument>| {
            let mut names = shadows
                .into_iter()
                .map(|shadow| {
                    uri_to_path_for_test(&shadow.source_uri)
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned()
                })
                .collect::<Vec<_>>();
            names.sort();
            names
        };
        assert_eq!(
            names(overlay.reachable_javascript([root.join("src/Page.svelte")])),
            ["Card.svelte", "Exported.svelte", "Icon.svelte"],
            "transitively, plus what a module re-exports; not Unreached"
        );
        assert_eq!(names(overlay.reachable_javascript([])), ["Exported.svelte"]);
    }

    #[test]
    fn the_projects_that_own_components_are_found_in_one_walk() {
        let workspace = TestWorkspace::new("component-projects");
        let root = fs::canonicalize(&workspace.0).unwrap();
        write(&root.join("App.svelte"), "<p />");
        write(&root.join("app/tsconfig.json"), "{}");
        write(&root.join("app/src/deep/Page.svelte"), "<p />");
        write(&root.join("tools/jsconfig.json"), "{}");
        write(&root.join("tools/Tool.svelte"), "<p />");
        write(&root.join("tests/tsconfig.json"), "{}");
        write(&root.join("tests/only.test.ts"), "export {};");
        write(&root.join("node_modules/pkg/tsconfig.json"), "{}");
        write(&root.join("node_modules/pkg/Lib.svelte"), "<p />");
        write(&root.join(".hidden/tsconfig.json"), "{}");
        write(&root.join(".hidden/Secret.svelte"), "<p />");
        let projects = component_projects(&root);
        let spec = |config: Option<&str>| ProjectSpec {
            config: config.map(|config| root.join(config)),
            mirror_root: root.clone(),
        };
        assert_eq!(
            projects,
            vec![
                spec(None),
                spec(Some("app/tsconfig.json")),
                spec(Some("tools/jsconfig.json")),
            ]
        );
        let folder = TsgoOverlay::build_for(&projects[0]).unwrap();
        assert!(folder.is_project(&projects[0]));
        assert!(!folder.is_project(&projects[1]));
        let app = TsgoOverlay::build_for(&projects[1]).unwrap();
        assert!(app.is_project(&projects[1]));
        assert!(!app.is_loaded(), "built deferred");
        assert_eq!(app.workspace(), root.join("app"));
    }

    #[test]
    fn a_deleted_copy_comes_back_with_its_file() {
        use lsp_types::FileChangeType;
        let workspace = TestWorkspace::new("deleted-copy");
        let root = fs::canonicalize(&workspace.0).unwrap();
        write(&root.join("a/tsconfig.json"), "{}");
        write(
            &root.join("a/Parent.svelte"),
            "<script lang=\"ts\">import Child from '../b/Child.svelte';</script><Child />",
        );
        write(&root.join("b/tsconfig.json"), "{}");
        write(&root.join("b/Child.svelte"), "<p />");
        let child = root.join("b/Child.svelte");
        let none = EditorBuffers::default();
        let mut a =
            TsgoOverlay::build_nested(&root.join("a"), &root.join("a/tsconfig.json"), &root)
                .unwrap();
        assert!(a.shadow_for_source(&child).is_some());

        fs::remove_file(&child).unwrap();
        let update = a
            .apply_file_changes(&[(child.clone(), FileChangeType::DELETED)], &none)
            .unwrap()
            .expect("placeable");
        assert_eq!(update.closed.len(), 1);
        assert!(a.shadow_for_source(&child).is_none());

        write(&child, "<p />");
        let update = a
            .apply_file_changes(&[(child.clone(), FileChangeType::CREATED)], &none)
            .unwrap()
            .expect("placeable");
        assert_eq!(update.opened_or_changed.len(), 1, "restored");
        assert!(a.shadow_for_source(&child).is_some());
        assert!(!a.owns(&child), "still a copy");

        // The same when the deletion is noticed by closing the open child.
        fs::remove_file(&child).unwrap();
        assert!(a.close(&child).unwrap().is_none());
        write(&child, "<p />");
        a.apply_file_changes(&[(child.clone(), FileChangeType::CREATED)], &none)
            .unwrap();
        assert!(a.shadow_for_source(&child).is_some());
    }

    #[test]
    fn a_new_import_of_an_editor_open_component_copies_its_buffer() {
        use lsp_types::FileChangeType;
        let workspace = TestWorkspace::new("open-target");
        let root = fs::canonicalize(&workspace.0).unwrap();
        write(&root.join("a/tsconfig.json"), "{}");
        write(&root.join("a/Plain.svelte"), "<p />");
        write(&root.join("b/tsconfig.json"), "{}");
        write(&root.join("b/New.svelte"), "<p>disk</p>");
        let target = root.join("b/New.svelte");
        let mut editor = EditorBuffers::default();
        editor.insert(target.clone(), Arc::new("<p>buffer</p>".to_string()), 7);
        let import = "<script lang=\"ts\">import New from '../b/New.svelte';</script><New />";

        // A file event: the importer changed on disk while the target is open.
        let mut a =
            TsgoOverlay::build_nested(&root.join("a"), &root.join("a/tsconfig.json"), &root)
                .unwrap();
        write(&root.join("a/Plain.svelte"), import);
        let update = a
            .apply_file_changes(
                &[(root.join("a/Plain.svelte"), FileChangeType::CHANGED)],
                &editor,
            )
            .unwrap()
            .expect("placeable");
        assert_eq!(update.opened_or_changed.len(), 1, "the importer, from disk");
        assert_eq!(update.buffers.len(), 1, "the open target, from its buffer");
        assert_eq!(update.buffers[0].version, 7);
        assert_eq!(a.source_text(&target), Some("<p>buffer</p>"));

        // A full refresh finds the same copy.
        write(&root.join("a/Plain.svelte"), "<p />");
        let mut a =
            TsgoOverlay::build_nested(&root.join("a"), &root.join("a/tsconfig.json"), &root)
                .unwrap();
        write(&root.join("a/Plain.svelte"), import);
        let update = a.refresh_except(&editor).unwrap();
        assert_eq!(update.buffers.len(), 1);
        assert_eq!(a.source_text(&target), Some("<p>buffer</p>"));
        // Once the target closes, the copy follows the file like any other.
        assert!(a.close(&target).unwrap().is_some());
        assert_eq!(a.source_text(&target), Some("<p>disk</p>"));
    }

    #[test]
    fn svelte_specifiers_are_the_ones_the_import_rewrite_finds() {
        let source = "import A from './A.svelte';\nimport B from '../b/B.svelte';\n\
                      import C from 'pkg/C.svelte';\nconst s = './not-an-import.svelte';";
        assert_eq!(
            svelte_specifiers(source),
            vec!["./A.svelte", "../b/B.svelte", "pkg/C.svelte"]
        );
    }

    #[test]
    fn a_nested_overlay_resolves_the_nested_configs_own_paths() {
        // The reason the split matters: `paths` declared one level down are
        // invisible to a single workspace-root project.
        let workspace = TestWorkspace::new("nested-paths");
        let root = fs::canonicalize(&workspace.0).unwrap();
        write(&root.join("tsconfig.json"), "{}");
        write(
            &root.join("docs/tsconfig.json"),
            r#"{ "compilerOptions": { "paths": { "@/*": ["./src/*"] } } }"#,
        );
        write(&root.join("docs/src/App.svelte"), "<p />");

        let owner = nearest_tsconfig(&root.join("docs/src/App.svelte"), &root).unwrap();
        let nested = TsgoOverlay::build_with(owner.parent().unwrap(), Some(&owner), None).unwrap();
        let config = overlay_config(&nested);
        assert_eq!(
            config["compilerOptions"]["paths"]["@/*"],
            json!([
                path_for_tsconfig(&root.join("docs/src/*")),
                path_for_tsconfig(&nested.shadow_dir.join("src/*"))
            ])
        );

        // Live control: the root project, which is what the server used to hand
        // every document, has no such mapping.
        let rooted = build_overlay(&root).unwrap();
        assert!(overlay_config(&rooted)["compilerOptions"]["paths"].is_null());
    }
}
