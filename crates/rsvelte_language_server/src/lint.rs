//! Lint configuration discovery and the lint pass itself.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rsvelte_core::CompileOptions;
use rsvelte_diagnostics::Diagnostic;
use rsvelte_lint::{LintConfig, Severity, lint_source};

/// Config file names, in the order a directory is probed — the same two
/// `rsvelte-lint` itself discovers. An `ESLint` config is deliberately *not*
/// consulted: importing it is opt-in on the CLI (`--config-from-eslint`), and a
/// server that read it on its own would report a different rule set in the
/// editor than the same project's CI does.
const CONFIG_NAMES: &[&str] = &["rsvelte-lint.json", ".rsvelte-lintrc.json"];

/// The Svelte and Vite configs whose `compilerOptions` the compiler honours,
/// in the order vite-plugin-svelte reads them.
const SVELTE_CONFIG_NAMES: &[&str] = &[
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
];

/// Resolved lint configs and compiler options, keyed by the directory
/// discovery started from. Discovery walks to the filesystem root, so it is
/// cached rather than redone on every keystroke.
#[derive(Default)]
pub struct LintConfigCache {
    by_dir: HashMap<PathBuf, Arc<LintConfig>>,
    compile_by_dir: HashMap<PathBuf, Arc<CompileOptions>>,
}

impl LintConfigCache {
    /// The config governing a document, discovered upward from its directory.
    /// Without an `rsvelte-lint.json` only the compiler's own diagnostics are
    /// reported ([`compiler_only`]), as `svelte-language-server` does.
    pub fn get(&mut self, dir: &Path) -> Arc<LintConfig> {
        if let Some(config) = self.by_dir.get(dir) {
            return Arc::clone(config);
        }
        let config = Arc::new(discover(dir).unwrap_or_else(compiler_only));
        self.by_dir.insert(dir.to_path_buf(), Arc::clone(&config));
        config
    }

    /// The compiler options the nearest Svelte or Vite config sets for a
    /// document in `dir` (`experimental.async`, `runes`). Without them a
    /// project that enables top-level `await` gets `experimental_async` on
    /// every component that uses it.
    pub fn compile_options(&mut self, dir: &Path) -> Arc<CompileOptions> {
        if let Some(options) = self.compile_by_dir.get(dir) {
            return Arc::clone(options);
        }
        let options = Arc::new(project_compile_options(dir));
        self.compile_by_dir
            .insert(dir.to_path_buf(), Arc::clone(&options));
        options
    }

    pub fn clear(&mut self) {
        self.by_dir.clear();
        self.compile_by_dir.clear();
    }
}

/// The recommended preset with every named rule off: what remains are the
/// compiler's warnings and errors under their own codes (the validator wrap),
/// at their default severities. `svelte-language-server` reports exactly
/// those; rules such as `svelte/no-unused-svelte-ignore` or
/// `svelte/require-each-key` belong to a project that opted into linting with
/// an `rsvelte-lint.json`.
fn compiler_only() -> LintConfig {
    rsvelte_lint::registry::registered_rule_metas()
        .into_iter()
        .fold(LintConfig::recommended(), |config, meta| {
            config.with_override(meta.name, Severity::Off)
        })
}

/// Compile options from the nearest directory, walking up from `dir`, that
/// holds a Svelte or Vite config. Only statically readable values are used
/// (see `rsvelte_check::config`); anything else keeps the compiler default.
fn project_compile_options(dir: &Path) -> CompileOptions {
    let mut options = CompileOptions::default();
    let Some(root) = dir.ancestors().find(|ancestor| {
        SVELTE_CONFIG_NAMES
            .iter()
            .any(|name| ancestor.join(name).is_file())
    }) else {
        return options;
    };
    let settings = rsvelte_check::config::load_compiler_options(root);
    options.experimental.r#async = settings.experimental_async;
    options.runes = settings.runes;
    options
}

/// Walk up from `start` for the nearest lint config. An unreadable or invalid
/// config yields `None`, falling back to the recommended preset — a config
/// error must not leave the editor without diagnostics.
fn discover(start: &Path) -> Option<LintConfig> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        for name in CONFIG_NAMES {
            let candidate = d.join(name);
            if candidate.is_file() {
                return match std::fs::read_to_string(&candidate)
                    .map_err(|e| e.to_string())
                    .and_then(|text| LintConfig::from_json_str(&text).map_err(|e| e.to_string()))
                {
                    Ok(config) => Some(config),
                    Err(err) => {
                        crate::log::warn(format_args!("{}: {err}", candidate.display()));
                        None
                    }
                };
            }
        }
        dir = d.parent();
    }
    None
}

/// Lint one open document. Suppression comments (`<!-- svelte-ignore -->`,
/// `eslint-disable`) and inline rule config are applied inside `lint_source`.
#[must_use]
pub fn lint(path: &Path, source: &str, config: &LintConfig) -> Vec<Diagnostic> {
    lint_with(path, source, config, &CompileOptions::default())
}

/// [`lint`] with the project's compiler options ([`LintConfigCache::compile_options`]).
#[must_use]
pub fn lint_with(
    path: &Path,
    source: &str,
    config: &LintConfig,
    base: &CompileOptions,
) -> Vec<Diagnostic> {
    let options = CompileOptions {
        filename: Some(path.display().to_string()),
        ..base.clone()
    };
    lint_source(source, path, &options, config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_a_config_only_compiler_diagnostics_remain() {
        let codes: Vec<_> = lint(
            Path::new("App.svelte"),
            "<div>{@html x}</div>\n<img src=\"x.png\" />",
            &compiler_only(),
        )
        .into_iter()
        .filter_map(|d| d.code)
        .collect();
        assert!(
            codes.iter().any(|c| c == "a11y_missing_attribute"),
            "{codes:?}"
        );
        assert!(!codes.iter().any(|c| c.contains('/')), "{codes:?}");
    }

    #[test]
    fn compiler_options_come_from_the_nearest_svelte_or_vite_config() {
        let dir = std::env::temp_dir().join(format!(
            "rsvelte-ls-compile-options-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(dir.join("src/routes")).unwrap();
        assert!(
            !project_compile_options(&dir.join("src/routes"))
                .experimental
                .r#async
        );
        std::fs::write(
            dir.join("vite.config.ts"),
            "import { sveltekit } from '@sveltejs/kit/vite';\nexport default { plugins: [sveltekit({ compilerOptions: { experimental: { async: true }, runes: true } })] };\n",
        )
        .unwrap();
        let options = project_compile_options(&dir.join("src/routes"));
        assert!(options.experimental.r#async);
        assert_eq!(options.runes, Some(true));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn suppression_comments_are_honoured() {
        let path = PathBuf::from("App.svelte");
        let config = LintConfig::recommended();
        let source = "<div>{@html x}</div>";
        assert!(!lint(&path, source, &config).is_empty());

        let suppressed =
            "<!-- eslint-disable-next-line svelte/no-at-html-tags -->\n<div>{@html x}</div>";
        let codes: Vec<_> = lint(&path, suppressed, &config)
            .into_iter()
            .filter_map(|d| d.code)
            .collect();
        assert!(!codes.iter().any(|c| c == "svelte/no-at-html-tags"));
    }

    #[test]
    fn a_config_file_turns_a_rule_off() {
        let dir = std::env::temp_dir().join(format!(
            "rsvelte-ls-lint-config-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("rsvelte-lint.json"),
            r#"{ "rules": { "svelte/no-at-html-tags": "off" } }"#,
        )
        .unwrap();

        let mut cache = LintConfigCache::default();
        let config = cache.get(&dir);
        let codes: Vec<_> = lint(&dir.join("App.svelte"), "<div>{@html x}</div>", &config)
            .into_iter()
            .filter_map(|d| d.code)
            .collect();
        assert!(!codes.iter().any(|c| c == "svelte/no-at-html-tags"));

        std::fs::remove_dir_all(&dir).ok();
    }
}
