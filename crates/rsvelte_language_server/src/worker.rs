//! The analysis worker.
//!
//! Linting and formatting run the Svelte compiler, which recurses once per
//! level of nested markup. Two hazards follow, and both are contained here
//! rather than on the message loop, which would take the whole session down
//! with it: deeply nested input needs far more stack than a thread is given by
//! default, and a rule that panics must cost at most its own document. This is
//! the same isolation contract `rsvelte-lint`'s CLI applies per file.
//!
//! The resolved-config caches live here too, so the loop never touches the
//! filesystem.
//!
//! Jobs run on two lanes, each a thread with its own caches. The analysis
//! lane runs the linter over a whole document (pushed and pulled diagnostics,
//! code actions) and walks the workspace for file references: tens to
//! hundreds of milliseconds on a large component. Everything else, what the
//! editor waits for while the user types (completion, hover, symbols,
//! folding, …), runs on the other lane and never queues behind an analysis.
//! An analysis that a newer one of the same document already waits behind is
//! not run: a stale pull is answered with the newer pull's result, a stale
//! pushed lint is dropped.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use crossbeam_channel::{Receiver, Sender, unbounded};
use lsp_server::RequestId;
use lsp_types::{
    CodeActionOrCommand, CodeLens, CompletionList, Diagnostic, DocumentSymbolResponse,
    FoldingRange, Hover, Location, Range, SelectionRange, TextEdit, Uri,
};
use serde_json::Value;

use crate::format::FormatSessions;
use crate::lint::LintConfigCache;
use crate::log;
use crate::settings::CompilerWarnings;
use crate::settings::FormatConfig;
use crate::tsgo_custom::{WorkspaceSource, find_file_references};
use crate::uri::path_to_uri;

/// `rsvelte_core`'s own deeply-nested-AST tests reserve the same 256 MiB. It is
/// address space, not resident memory — pages are committed only as the
/// recursion actually reaches them.
const STACK_SIZE: usize = 256 * 1024 * 1024;

pub enum Job {
    Lint {
        key: String,
        uri: Uri,
        version: i32,
        path: PathBuf,
        text: Arc<String>,
        preprocessed: Option<PreprocessedAnalysis>,
        warnings: CompilerWarnings,
        svelte_diagnostics: bool,
        css_diagnostics: bool,
    },
    Format {
        id: RequestId,
        path: PathBuf,
        text: Arc<String>,
        range: Range,
        config: FormatConfig,
    },
    Compile {
        id: RequestId,
        path: PathBuf,
        text: Arc<String>,
        sourcemap: Option<Arc<String>>,
    },
    Complete {
        id: RequestId,
        path: PathBuf,
        text: Arc<String>,
        offset: usize,
        strict_mode: bool,
        markdown_documentation: bool,
        emmet: bool,
    },
    Hover {
        id: RequestId,
        path: PathBuf,
        text: Arc<String>,
        offset: usize,
        markdown_hover: bool,
    },
    CodeAction {
        id: RequestId,
        uri: Uri,
        path: PathBuf,
        text: Arc<String>,
        diagnostics: Vec<Diagnostic>,
        quickfix: bool,
        suggestions: bool,
        fix_all: bool,
    },
    CodeLens {
        id: RequestId,
        path: PathBuf,
        text: Arc<String>,
    },
    ExtractComponent {
        id: RequestId,
        uri: Uri,
        text: Arc<String>,
        range: Range,
        file_path: String,
    },
    FoldingRange {
        id: RequestId,
        path: PathBuf,
        text: Arc<String>,
        line_folding_only: bool,
    },
    SelectionRange {
        id: RequestId,
        path: PathBuf,
        text: Arc<String>,
        offsets: Vec<usize>,
    },
    DocumentSymbol {
        id: RequestId,
        uri: Uri,
        path: PathBuf,
        text: Arc<String>,
        hierarchical: bool,
    },
    PullDiagnostics {
        id: RequestId,
        path: PathBuf,
        text: Arc<String>,
        preprocessed: Option<PreprocessedAnalysis>,
        warnings: CompilerWarnings,
        svelte_diagnostics: bool,
        css_diagnostics: bool,
    },
    FileReferences {
        id: RequestId,
        target: PathBuf,
        roots: Vec<PathBuf>,
        open_documents: Vec<FileReferenceSource>,
    },
    /// Drop the resolved `rsvelte-lint.json` / `.oxfmtrc` caches so the next
    /// job re-reads them from disk.
    ClearCaches,
}

#[derive(Clone)]
pub struct PreprocessedAnalysis {
    pub text: Arc<String>,
    pub map: Option<Arc<String>>,
    pub identity: bool,
}

pub enum Outcome {
    Diagnostics {
        key: String,
        uri: Uri,
        version: i32,
        diagnostics: Vec<Diagnostic>,
    },
    Formatted {
        id: RequestId,
        edits: Vec<TextEdit>,
    },
    Compiled {
        id: RequestId,
        result: Option<Value>,
    },
    Completed {
        id: RequestId,
        list: Option<CompletionList>,
    },
    Hovered {
        id: RequestId,
        hover: Option<Hover>,
    },
    CodeActions {
        id: RequestId,
        actions: Vec<CodeActionOrCommand>,
    },
    CodeLenses {
        id: RequestId,
        lenses: Vec<CodeLens>,
    },
    ExtractedComponent {
        id: RequestId,
        result: Value,
    },
    FoldingRanges {
        id: RequestId,
        ranges: Vec<FoldingRange>,
    },
    SelectionRanges {
        id: RequestId,
        ranges: Option<Vec<SelectionRange>>,
    },
    DocumentSymbols {
        id: RequestId,
        symbols: DocumentSymbolResponse,
    },
    PulledDiagnostics {
        id: RequestId,
        path: PathBuf,
        diagnostics: Vec<Diagnostic>,
    },
    FileReferences {
        id: RequestId,
        locations: Vec<Location>,
    },
}

pub struct FileReferenceSource {
    pub path: PathBuf,
    pub uri: Uri,
    pub text: String,
}

pub struct Worker {
    /// Requests the editor waits on while typing.
    quick: Lane,
    /// Whole-document lints and workspace walks.
    analysis: Lane,
    stopping: Arc<AtomicBool>,
}

struct Lane {
    jobs: Option<Sender<Job>>,
    handle: Option<JoinHandle<()>>,
}

impl Lane {
    fn spawn(name: &str, outcomes: Sender<Outcome>, stopping: Arc<AtomicBool>) -> Self {
        let (jobs, receiver) = unbounded();
        let handle = std::thread::Builder::new()
            .name(name.to_string())
            .stack_size(STACK_SIZE)
            .spawn(move || run(&receiver, &outcomes, &stopping))
            .expect("spawn an analysis worker");
        Self {
            jobs: Some(jobs),
            handle: Some(handle),
        }
    }

    fn submit(&self, job: Job) {
        if let Some(jobs) = &self.jobs {
            let _ = jobs.send(job);
        }
    }

    fn stop(&mut self) {
        self.jobs.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Worker {
    /// # Panics
    ///
    /// Panics if an analysis worker thread cannot be spawned.
    #[must_use]
    pub fn spawn(outcomes: Sender<Outcome>) -> Self {
        let stopping = Arc::new(AtomicBool::new(false));
        Self {
            quick: Lane::spawn("rsvelte-analysis", outcomes.clone(), Arc::clone(&stopping)),
            analysis: Lane::spawn("rsvelte-diagnostics", outcomes, Arc::clone(&stopping)),
            stopping,
        }
    }

    pub fn submit(&self, job: Job) {
        match job {
            Job::ClearCaches => {
                self.quick.submit(Job::ClearCaches);
                self.analysis.submit(Job::ClearCaches);
            }
            Job::Lint { .. }
            | Job::PullDiagnostics { .. }
            | Job::CodeAction { .. }
            | Job::FileReferences { .. } => self.analysis.submit(job),
            job => self.quick.submit(job),
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        self.quick.stop();
        self.analysis.stop();
    }
}

/// Whether `job` is an analysis a job queued after it makes pointless: a pull
/// or a pushed lint of the same document.
fn superseded(job: &Job, queued: &VecDeque<Job>) -> bool {
    match job {
        Job::PullDiagnostics { path, .. } => queued.iter().any(
            |later| matches!(later, Job::PullDiagnostics { path: later, .. } if later == path),
        ),
        Job::Lint { key, .. } => queued
            .iter()
            .any(|later| matches!(later, Job::Lint { key: later, .. } if later == key)),
        _ => false,
    }
}

fn run(jobs: &Receiver<Job>, outcomes: &Sender<Outcome>, stopping: &AtomicBool) {
    let mut lint_configs = LintConfigCache::default();
    let mut format_sessions = FormatSessions::default();
    let mut queued = VecDeque::new();
    // Pulls answered with the result of a later pull of the same document.
    let mut stale_pulls = HashMap::<PathBuf, Vec<RequestId>>::new();

    loop {
        if queued.is_empty() {
            let Ok(job) = jobs.recv() else {
                break;
            };
            queued.push_back(job);
        }
        queued.extend(jobs.try_iter());
        let Some(job) = queued.pop_front() else {
            continue;
        };
        if stopping.load(Ordering::Acquire) {
            break;
        }
        if superseded(&job, &queued) {
            if let Job::PullDiagnostics { id, path, .. } = job {
                stale_pulls.entry(path).or_default().push(id);
            }
            continue;
        }
        let outcome = match job {
            Job::ClearCaches => {
                lint_configs.clear();
                format_sessions.clear();
                continue;
            }
            Job::Lint {
                key,
                uri,
                version,
                path,
                text,
                preprocessed,
                warnings,
                svelte_diagnostics,
                css_diagnostics,
            } => {
                let dir = path.parent().unwrap_or(Path::new("."));
                let config = lint_configs.get(dir);
                let compile = lint_configs.compile_options(dir);
                let diagnostics = guard("lint", &path, || {
                    let mut diagnostics = if svelte_diagnostics {
                        lint_with_preprocessor(
                            &path,
                            &text,
                            preprocessed.as_ref(),
                            &config,
                            &compile,
                            &warnings,
                        )
                    } else {
                        Vec::new()
                    };
                    if css_diagnostics {
                        diagnostics.extend(crate::css::diagnostics(&text));
                    }
                    diagnostics
                })
                .unwrap_or_default();
                Outcome::Diagnostics {
                    key,
                    uri,
                    version,
                    diagnostics,
                }
            }
            Job::Format {
                id,
                path,
                text,
                range,
                config,
            } => {
                let edits = format(&mut format_sessions, &path, &text, range, &config);
                Outcome::Formatted { id, edits }
            }
            Job::Compile {
                id,
                path,
                text,
                sourcemap,
            } => Outcome::Compiled {
                id,
                result: guard("compiled code", &path, || {
                    let options = rsvelte_core::CompileOptions {
                        filename: Some(path.display().to_string()),
                        sourcemap: sourcemap.as_deref().cloned(),
                        ..rsvelte_core::CompileOptions::default()
                    };
                    rsvelte_core::compile(&text, options).ok().map(|compiled| {
                        let map = |map: Option<String>| {
                            map.and_then(|map| serde_json::from_str(&map).ok())
                                .unwrap_or(Value::Null)
                        };
                        let css = compiled.css.map(|css| {
                            serde_json::json!({
                                "code": css.code,
                                "map": map(css.map),
                                "hasGlobal": css.has_global,
                            })
                        });
                        serde_json::json!({
                            "js": { "code": compiled.js.code, "map": map(compiled.js.map) },
                            "css": css,
                        })
                    })
                })
                .flatten(),
            },
            Job::Complete {
                id,
                path,
                text,
                offset,
                strict_mode,
                markdown_documentation,
                emmet,
            } => Outcome::Completed {
                id,
                list: guard("completion", &path, || {
                    crate::completions::completions_with_strict_mode(
                        &text,
                        offset,
                        strict_mode,
                        markdown_documentation,
                        emmet,
                    )
                })
                .flatten(),
            },
            Job::Hover {
                id,
                path,
                text,
                offset,
                markdown_hover,
            } => Outcome::Hovered {
                id,
                hover: guard("hover", &path, || {
                    crate::hover::hover(&text, offset, markdown_hover)
                })
                .flatten(),
            },
            Job::CodeAction {
                id,
                uri,
                path,
                text,
                diagnostics,
                quickfix,
                suggestions,
                fix_all,
            } => {
                let dir = path.parent().unwrap_or(Path::new("."));
                let config = lint_configs.get(dir);
                let compile = lint_configs.compile_options(dir);
                let actions = guard("code action", &path, || {
                    let mut actions = if quickfix {
                        crate::code_actions::quickfixes(&text, &uri, &diagnostics)
                    } else {
                        Vec::new()
                    };
                    actions.extend(crate::code_actions::lint_actions(
                        &text,
                        &path,
                        &uri,
                        &config,
                        &compile,
                        &diagnostics,
                        quickfix,
                        suggestions,
                        fix_all,
                    ));
                    actions
                })
                .unwrap_or_default();
                Outcome::CodeActions { id, actions }
            }
            Job::CodeLens { id, path, text } => {
                let compile = lint_configs.compile_options(path.parent().unwrap_or(Path::new(".")));
                Outcome::CodeLenses {
                    id,
                    lenses: guard("code lens", &path, || {
                        crate::code_lens::code_lenses_with(&text, &path, &compile)
                    })
                    .unwrap_or_default(),
                }
            }
            Job::ExtractComponent {
                id,
                uri,
                text,
                range,
                file_path,
            } => Outcome::ExtractedComponent {
                id,
                result: guard("extract component", Path::new(uri.as_str()), || {
                    crate::extract::component(&text, uri.as_str(), range, &file_path)
                })
                .unwrap_or_else(|| Err("Invalid selection range".to_string()))
                .unwrap_or_else(Value::String),
            },
            Job::FoldingRange {
                id,
                path,
                text,
                line_folding_only,
            } => Outcome::FoldingRanges {
                id,
                ranges: guard("folding range", &path, || {
                    crate::folding::folding_ranges(&text, line_folding_only)
                })
                .unwrap_or_default(),
            },
            Job::SelectionRange {
                id,
                path,
                text,
                offsets,
            } => Outcome::SelectionRanges {
                id,
                ranges: guard("selection range", &path, || {
                    crate::selection_ranges::selection_ranges(&text, &offsets)
                })
                .flatten(),
            },
            Job::DocumentSymbol {
                id,
                uri,
                path,
                text,
                hierarchical,
            } => Outcome::DocumentSymbols {
                id,
                symbols: guard("document symbol", &path, || {
                    crate::symbols::document_symbols(&text, &uri, hierarchical)
                })
                .unwrap_or_else(|| DocumentSymbolResponse::Nested(Vec::new())),
            },
            Job::PullDiagnostics {
                id,
                path,
                text,
                preprocessed,
                warnings,
                svelte_diagnostics,
                css_diagnostics,
            } => {
                let dir = path.parent().unwrap_or(Path::new("."));
                let config = lint_configs.get(dir);
                let compile = lint_configs.compile_options(dir);
                let diagnostics = guard("pull diagnostics", &path, || {
                    let mut diagnostics = if svelte_diagnostics {
                        lint_with_preprocessor(
                            &path,
                            &text,
                            preprocessed.as_ref(),
                            &config,
                            &compile,
                            &warnings,
                        )
                    } else {
                        Vec::new()
                    };
                    if css_diagnostics {
                        diagnostics.extend(crate::css::diagnostics(&text));
                    }
                    diagnostics
                })
                .unwrap_or_default();
                for stale in stale_pulls.remove(&path).unwrap_or_default() {
                    let _ = outcomes.send(Outcome::PulledDiagnostics {
                        id: stale,
                        path: path.clone(),
                        diagnostics: diagnostics.clone(),
                    });
                }
                Outcome::PulledDiagnostics {
                    id,
                    path,
                    diagnostics,
                }
            }
            Job::FileReferences {
                id,
                target,
                roots,
                open_documents,
            } => Outcome::FileReferences {
                id,
                locations: file_references(&target, &roots, open_documents),
            },
        };
        if outcomes.send(outcome).is_err() {
            break;
        }
    }
}

fn lint_with_preprocessor(
    path: &Path,
    raw: &str,
    preprocessed: Option<&PreprocessedAnalysis>,
    config: &rsvelte_lint::LintConfig,
    compile: &rsvelte_core::CompileOptions,
    warnings: &CompilerWarnings,
) -> Vec<Diagnostic> {
    let mut diagnostics = crate::lint::lint_with(path, raw, config, compile)
        .iter()
        .filter_map(|diagnostic| {
            let compiler = diagnostic
                .code
                .as_deref()
                .is_some_and(crate::diagnostics::is_compiler_code);
            if compiler && preprocessed.is_some() {
                return None;
            }
            let diagnostic = crate::diagnostics::to_lsp(diagnostic, warnings)?;
            (!compiler || crate::diagnostics::keep_raw_compiler_diagnostic(&diagnostic, raw))
                .then_some(diagnostic)
        })
        .collect::<Vec<_>>();
    let Some(preprocessed) = preprocessed else {
        return diagnostics;
    };
    let processed_diagnostics = crate::lint::lint_with(path, &preprocessed.text, config, compile);
    let mapped = processed_diagnostics
        .into_iter()
        .filter(|diagnostic| {
            diagnostic
                .code
                .as_deref()
                .is_some_and(crate::diagnostics::is_compiler_code)
        })
        .filter_map(|diagnostic| crate::diagnostics::to_lsp(&diagnostic, warnings))
        .filter_map(|diagnostic| {
            if preprocessed.identity {
                Some(diagnostic)
            } else {
                crate::diagnostics::map_preprocessed_diagnostic(
                    diagnostic,
                    preprocessed.map.as_deref()?,
                )
            }
        });
    diagnostics.extend(mapped);
    diagnostics
}

fn file_references(
    target: &Path,
    roots: &[PathBuf],
    open_documents: Vec<FileReferenceSource>,
) -> Vec<Location> {
    let mut sources = HashMap::<PathBuf, FileReferenceSource>::new();
    for root in roots {
        collect_source_directory(root, &mut sources);
    }
    for source in open_documents {
        sources.insert(source.path.clone(), source);
    }
    let sources = sources.into_values().collect::<Vec<_>>();
    let views = sources
        .iter()
        .map(|source| WorkspaceSource {
            path: &source.path,
            uri: &source.uri,
            text: &source.text,
        })
        .collect::<Vec<_>>();
    find_file_references(target, &views)
}

fn collect_source_directory(directory: &Path, sources: &mut HashMap<PathBuf, FileReferenceSource>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            if matches!(
                entry.file_name().to_str(),
                Some("node_modules" | ".git" | ".rsvelte-language-server" | "target")
            ) || entry.file_name() == crate::tsgo_overlay::CACHE_DIRECTORY
            {
                continue;
            }
            collect_source_directory(&path, sources);
            continue;
        }
        if !file_type.is_file()
            || !path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| {
                    matches!(
                        extension,
                        "ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs" | "svelte"
                    )
                })
        {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let canonical = crate::uri::canonicalize(&path).unwrap_or(path);
        let Some(uri) = path_to_uri(&canonical) else {
            continue;
        };
        sources.insert(
            canonical.clone(),
            FileReferenceSource {
                path: canonical,
                uri,
                text,
            },
        );
    }
}

fn format(
    sessions: &mut FormatSessions,
    path: &Path,
    text: &str,
    range: Range,
    config: &FormatConfig,
) -> Vec<TextEdit> {
    let session = match sessions.get(path) {
        Ok(session) => session,
        Err(err) => {
            log::warn(format_args!(
                "no formatter config for {}: {err:#}",
                path.display()
            ));
            return Vec::new();
        }
    };
    // Formatting is never an error to the client: a failure yields no edits.
    let Some(formatted) = guard("format", path, || session.format(text, path)) else {
        return Vec::new();
    };
    let formatted = formatted
        .and_then(|formatted| crate::format::apply_editor_config(&formatted, path, config));
    match formatted {
        Ok(formatted) if formatted != text => vec![TextEdit {
            range,
            new_text: formatted,
        }],
        Ok(_) => Vec::new(),
        Err(err) => {
            log::warn(format_args!(
                "format failed for {}: {err:#}",
                path.display()
            ));
            Vec::new()
        }
    }
}

/// Run one analysis, turning a panic into "no result" instead of the loss of
/// every other document's diagnostics.
fn guard<T>(what: &str, path: &Path, run: impl FnOnce() -> T) -> Option<T> {
    if let Ok(value) = catch_unwind(AssertUnwindSafe(run)) {
        Some(value)
    } else {
        log::warn(format_args!("{what} panicked on {}", path.display()));
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{Job, Outcome, Worker};
    use crate::settings::CompilerWarnings;
    use crossbeam_channel::{Receiver, unbounded};
    use lsp_server::RequestId;
    use std::fmt::Write as _;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    /// A component whose lint takes long enough that a job submitted right
    /// after it is still queued when the worker comes back.
    fn large_component() -> Arc<String> {
        let mut text = "<script lang=\"ts\">\n".to_string();
        let mut template = String::new();
        for index in 0..1_500 {
            writeln!(text, "\tlet v{index} = $state({index});").unwrap();
            writeln!(template, "<div class=\"r{index}\">{{v{index}}}</div>").unwrap();
        }
        text.push_str("</script>\n");
        text.push_str(&template);
        Arc::new(text)
    }

    fn pull(id: i32, path: &str, text: Arc<String>) -> Job {
        Job::PullDiagnostics {
            id: RequestId::from(id),
            path: std::env::temp_dir().join(path),
            text,
            preprocessed: None,
            warnings: CompilerWarnings::default(),
            svelte_diagnostics: true,
            css_diagnostics: true,
        }
    }

    fn next(outcomes: &Receiver<Outcome>) -> Outcome {
        outcomes
            .recv_timeout(Duration::from_secs(60))
            .expect("the worker answers")
    }

    /// The editor waits on a completion while it types; the analysis of the
    /// previous keystroke's text (a diagnostic pull of a large component)
    /// runs on its own lane and does not hold it up.
    #[test]
    fn a_completion_does_not_wait_for_the_analysis_of_a_large_component() {
        let (sender, outcomes) = unbounded();
        let worker = Worker::spawn(sender);
        let text = large_component();
        worker.submit(pull(1, "Large.svelte", Arc::clone(&text)));
        worker.submit(Job::Complete {
            id: RequestId::from(2),
            path: PathBuf::from("Large.svelte"),
            text,
            offset: 0,
            strict_mode: false,
            markdown_documentation: true,
            emmet: true,
        });
        assert!(
            matches!(next(&outcomes), Outcome::Completed { id, .. } if id == RequestId::from(2)),
            "the completion is answered first"
        );
        assert!(matches!(
            next(&outcomes),
            Outcome::PulledDiagnostics { id, .. } if id == RequestId::from(1)
        ));
    }

    /// Pulls of one document that pile up behind an analysis are not each
    /// analysed: an earlier one is answered with the newest one's result,
    /// before it. A pushed lint a newer one of its document follows is
    /// dropped.
    #[test]
    fn a_pull_a_later_pull_of_its_document_follows_gets_that_ones_result() {
        let (sender, outcomes) = unbounded();
        let worker = Worker::spawn(sender);
        worker.submit(pull(1, "Large.svelte", large_component()));
        let clean = Arc::new("<p>clean</p>\n".to_string());
        let warned = Arc::new("<img src=\"a.png\">\n".to_string());
        worker.submit(pull(2, "Small.svelte", clean));
        worker.submit(pull(3, "Small.svelte", Arc::clone(&warned)));
        let lint = |version: i32, text: Arc<String>| Job::Lint {
            key: "small".to_string(),
            uri: "file:///Small.svelte".parse().unwrap(),
            version,
            path: std::env::temp_dir().join("Small.svelte"),
            text,
            preprocessed: None,
            warnings: CompilerWarnings::default(),
            svelte_diagnostics: true,
            css_diagnostics: true,
        };
        worker.submit(lint(1, Arc::new("<p>clean</p>\n".to_string())));
        worker.submit(lint(2, warned));

        let mut answered = Vec::new();
        while answered.len() < 4 {
            match next(&outcomes) {
                Outcome::PulledDiagnostics {
                    id, diagnostics, ..
                } => answered.push((format!("pull {id}"), diagnostics)),
                Outcome::Diagnostics {
                    version,
                    diagnostics,
                    ..
                } => answered.push((format!("lint {version}"), diagnostics)),
                _ => panic!("unexpected outcome"),
            }
        }
        let names = answered
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["pull 1", "pull 2", "pull 3", "lint 2"]);
        let warning = |diagnostics: &[lsp_types::Diagnostic]| {
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("alt"))
        };
        assert!(warning(&answered[2].1), "{:?}", answered[2].1);
        assert_eq!(
            answered[1].1, answered[2].1,
            "the earlier pull gets the later one's result"
        );
        assert!(
            outcomes.recv_timeout(Duration::from_millis(200)).is_err(),
            "the first lint is never run"
        );
    }
}
