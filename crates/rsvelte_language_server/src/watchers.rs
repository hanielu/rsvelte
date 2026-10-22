//! tsgo's file-watcher registrations, forwarded to the editor without the
//! ones a watcher the editor already has covers.
//!
//! tsgo registers watchers per program as each project builds: the files
//! under the workspace (`<root>/**/*`), and for auto-imports every
//! `node_modules` a project resolves from — the cache's mirror links among
//! them. With projects loading one at a time, each load registered more, and
//! tsgo replaces its auto-import watchers by unregistering and registering
//! the same patterns again. Once the editor watches `<root>/**/*` for every
//! kind of change, a watcher below it adds no event: it is answered here and
//! kept, and registered with the editor only if the covering one goes away.
//! A watcher into a `node_modules` or a dot directory always reaches the
//! editor, though: that is what makes an editor scan an ignored directory
//! and report its changes (a package install), as Zed does. One into a
//! `node_modules` mirror link in the cache is registered on the directory
//! the link points at: the editor reports changes under their real paths,
//! which tsgo takes for the link's too, and scans each directory once.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde_json::{Value, json};

use crate::tsgo_overlay::is_cache_path;
use crate::uri::uri_to_path;

const WATCHED_FILES: &str = "workspace/didChangeWatchedFiles";
/// `WatchKind.Create | Change | Delete`, a watcher's default.
const EVERY_KIND: u64 = 7;

#[derive(Debug)]
struct Registration {
    /// The registration as tsgo sent it.
    registration: Value,
    watchers: Vec<Value>,
    /// Whether the editor has it.
    forwarded: bool,
}

/// tsgo's watcher registrations, by id.
#[derive(Debug, Default)]
pub struct TsgoWatchers {
    registrations: BTreeMap<String, Registration>,
}

impl TsgoWatchers {
    /// tsgo's `client/registerCapability` params: what to forward to the
    /// editor, or `None` when nothing is new to it, in which case the caller
    /// answers tsgo itself.
    pub fn register(&mut self, params: &Value) -> Option<Value> {
        let mut forward = Vec::new();
        for registration in params
            .get("registrations")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let (Some(id), Some(method)) = (
                registration.get("id").and_then(Value::as_str),
                registration.get("method").and_then(Value::as_str),
            ) else {
                forward.push(registration.clone());
                continue;
            };
            if method != WATCHED_FILES {
                forward.push(registration.clone());
                continue;
            }
            let watchers = registration
                .pointer("/registerOptions/watchers")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|watcher| through_mirror_link(watcher).unwrap_or_else(|| watcher.clone()))
                .collect::<Vec<_>>();
            let mut registration = registration.clone();
            if let Some(options) = registration
                .pointer_mut("/registerOptions/watchers")
                .filter(|options| options.is_array())
            {
                *options = Value::Array(watchers.clone());
            }
            let forwarded = !watchers.iter().all(|watcher| self.covers(watcher));
            if forwarded {
                forward.push(registration.clone());
            }
            self.registrations.insert(
                id.to_string(),
                Registration {
                    registration,
                    watchers,
                    forwarded,
                },
            );
        }
        (!forward.is_empty()).then(|| json!({ "registrations": forward }))
    }

    /// tsgo's `client/unregisterCapability` params: what to forward to the
    /// editor (`None`: answer tsgo here), and the registrations the editor
    /// now needs because what covered them is gone.
    pub fn unregister(&mut self, params: &Value) -> (Option<Value>, Option<Value>) {
        let mut forward = Vec::new();
        for unregistration in params
            .get("unregisterations")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let id = unregistration.get("id").and_then(Value::as_str);
            match id.and_then(|id| self.registrations.remove(id)) {
                Some(registration) if !registration.forwarded => {}
                _ => forward.push(unregistration.clone()),
            }
        }
        let mut uncovered = self
            .registrations
            .iter()
            .filter(|(_, registration)| !registration.forwarded)
            .filter(|(_, registration)| {
                !registration
                    .watchers
                    .iter()
                    .all(|watcher| self.covers(watcher))
            })
            .map(|(id, registration)| {
                let depth = registration
                    .watchers
                    .iter()
                    .filter_map(literal_base)
                    .map(|base| base.len())
                    .min()
                    .unwrap_or(usize::MAX);
                (depth, id.clone())
            })
            .collect::<Vec<_>>();
        // The widest first: it may cover the rest.
        uncovered.sort();
        let mut register = Vec::new();
        for (_, id) in uncovered {
            // Re-checked one at a time: one that is registered again may
            // cover the next.
            let covered = self.registrations[&id]
                .watchers
                .iter()
                .all(|watcher| self.covers(watcher));
            if !covered && let Some(registration) = self.registrations.get_mut(&id) {
                registration.forwarded = true;
                register.push(registration.registration.clone());
            }
        }
        (
            (!forward.is_empty()).then(|| json!({ "unregisterations": forward })),
            (!register.is_empty()).then(|| json!({ "registrations": register })),
        )
    }

    /// tsgo restarted and will register its watchers again: the ones the
    /// editor has, to unregister.
    pub fn reset(&mut self) -> Option<Value> {
        let forwarded = std::mem::take(&mut self.registrations)
            .into_iter()
            .filter(|(_, registration)| registration.forwarded)
            .map(|(id, _)| json!({ "id": id, "method": WATCHED_FILES }))
            .collect::<Vec<_>>();
        (!forwarded.is_empty()).then(|| json!({ "unregisterations": forwarded }))
    }

    /// Whether a watcher the editor has reports every event `watcher` would.
    ///
    /// A recursive watcher covers what lies beneath it, except inside a
    /// `node_modules` or a dot directory (the cache's mirror links): an
    /// editor need not scan an ignored directory until a watcher's literal
    /// prefix points into it, and drops the events of one it has not
    /// scanned. Zed does both (`add_path_prefix_to_scan`), so without
    /// tsgo's own `node_modules` watchers it never reports an install.
    fn covers(&self, watcher: &Value) -> bool {
        let Some(base) = literal_base(watcher) else {
            return false;
        };
        self.registrations
            .values()
            .filter(|registration| registration.forwarded)
            .flat_map(|registration| &registration.watchers)
            .any(|forwarded| {
                forwarded == watcher
                    || recursive_base(forwarded).is_some_and(|recursive| {
                        relative_below(&base, &recursive)
                            .is_some_and(|relative| !enters_an_ignored_directory(relative))
                    })
            })
    }
}

/// A watcher whose literal base lies in an overlay cache, behind one of the
/// `node_modules` mirror links there, rewritten onto the directory the link
/// points at: an editor that scanned the link would scan the package
/// directory a second time and still report its changes under their real
/// paths, and two projects' links to one directory become one watcher.
fn through_mirror_link(watcher: &Value) -> Option<Value> {
    let (base, pattern) = glob_parts(watcher)?;
    let literal = &pattern[..pattern.find(['*', '?', '[', '{']).unwrap_or(pattern.len())];
    let split = literal.rfind('/').map_or(0, |slash| slash + 1);
    let directory = format!("{base}{}", &pattern[..split]);
    let directory = Path::new(directory.trim_end_matches('/'));
    if !is_cache_path(directory) {
        return None;
    }
    let target = fs::canonicalize(directory).ok()?;
    if is_cache_path(&target) {
        return None;
    }
    let mut translated = watcher.clone();
    translated["globPattern"] = Value::String(format!(
        "{}/{}",
        target.to_string_lossy().trim_end_matches('/'),
        &pattern[split..]
    ));
    Some(translated)
}

/// The directory a watcher's pattern starts from, up to its first wildcard,
/// with a trailing `/`.
fn literal_base(watcher: &Value) -> Option<String> {
    let (base, pattern) = glob_parts(watcher)?;
    let literal = &pattern[..pattern.find(['*', '?', '[', '{']).unwrap_or(pattern.len())];
    let directory = &literal[..literal.rfind('/').map_or(0, |slash| slash + 1)];
    Some(format!("{base}{directory}"))
}

/// The directory a watcher watches everything under, of every kind: one
/// whose pattern is `<dir>/**/*` (or `**`).
fn recursive_base(watcher: &Value) -> Option<String> {
    if watcher
        .get("kind")
        .and_then(Value::as_u64)
        .is_some_and(|kind| kind != EVERY_KIND)
    {
        return None;
    }
    let (base, pattern) = glob_parts(watcher)?;
    let directory = ["**/*", "**"]
        .into_iter()
        .find_map(|suffix| pattern.strip_suffix(suffix))?;
    if directory.contains(['*', '?', '[', '{'])
        || !(directory.is_empty() || directory.ends_with('/'))
    {
        return None;
    }
    Some(format!("{base}{directory}"))
}

/// A watcher's glob as a base directory (empty, or ending in `/`) and a
/// pattern relative to it.
fn glob_parts(watcher: &Value) -> Option<(String, String)> {
    match watcher.get("globPattern")? {
        Value::String(pattern) => Some((String::new(), pattern.clone())),
        Value::Object(relative) => {
            let base = match relative.get("baseUri")? {
                Value::String(uri) => uri.clone(),
                Value::Object(folder) => folder.get("uri")?.as_str()?.to_string(),
                _ => return None,
            };
            let mut base = uri_to_path(&base).to_string_lossy().into_owned();
            if !base.ends_with('/') {
                base.push('/');
            }
            Some((base, relative.get("pattern")?.as_str()?.to_string()))
        }
        _ => None,
    }
}

/// `path` relative to `directory` when it lies under it, comparing as the
/// file system does: tsgo spells some paths in lower case, which on a
/// case-insensitive system name the same directory.
fn relative_below<'a>(path: &'a str, directory: &str) -> Option<&'a str> {
    let head = path.get(..directory.len())?;
    let same = if cfg!(any(target_os = "macos", target_os = "windows")) {
        head == directory || head.to_lowercase() == directory.to_lowercase()
    } else {
        head == directory
    };
    same.then(|| &path[directory.len()..])
}

/// Whether a relative directory path enters a `node_modules` or a dot
/// directory, which editors leave unscanned until asked (gitignored).
fn enters_an_ignored_directory(relative: &str) -> bool {
    relative
        .split('/')
        .any(|component| component == "node_modules" || component.starts_with('.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watch(id: &str, globs: &[&str]) -> Value {
        json!({ "registrations": [{
            "id": id,
            "method": WATCHED_FILES,
            "registerOptions": { "watchers": globs.iter().map(|glob| json!({ "globPattern": glob })).collect::<Vec<_>>() },
        }] })
    }

    fn unwatch(id: &str) -> Value {
        json!({ "unregisterations": [{ "id": id, "method": WATCHED_FILES }] })
    }

    #[test]
    fn a_watcher_below_a_recursive_one_stays_with_the_server() {
        let mut watchers = TsgoWatchers::default();
        // What tsgo registers at the monorepo root as projects load.
        assert!(
            watchers
                .register(&watch("root files 8.0", &["/ws/**/*"]))
                .is_some()
        );
        assert!(
            watchers
                .register(&watch("failed lookups 11.0", &["/ws/admin/src/**/*"]))
                .is_none()
        );
        assert!(
            watchers
                .register(&watch("failed lookups 11.2", &["/ws/packages/ui/*.json"]))
                .is_none()
        );
        assert_eq!(
            watchers.unregister(&unwatch("failed lookups 11.0")),
            (None, None)
        );
        assert!(
            watchers
                .register(&watch("program files 12.1", &["/ws/**/*"]))
                .is_none(),
            "the same pattern"
        );
        assert!(
            watchers
                .register(&watch(
                    "program files 12.0",
                    &["/opt/tsrs/node_modules/**/*"]
                ))
                .is_some(),
            "outside it"
        );
        let configuration = json!({ "registrations": [{ "id": "config", "method": "workspace/didChangeConfiguration" }] });
        assert_eq!(
            watchers.register(&configuration),
            Some(configuration.clone())
        );

        // The covering watcher goes: the ones it covered reach the editor.
        let (forward, register) = watchers.unregister(&unwatch("root files 8.0"));
        assert_eq!(forward, Some(unwatch("root files 8.0")));
        let register = register.expect("uncovered");
        let ids = register["registrations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|registration| registration["id"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert!(ids.contains(&"program files 12.1"));
        assert_eq!(ids.len(), 1, "it covers the other one again: {ids:?}");

        assert_eq!(
            watchers.reset(),
            Some(json!({ "unregisterations": [
                { "id": "program files 12.0", "method": WATCHED_FILES },
                { "id": "program files 12.1", "method": WATCHED_FILES },
            ] }))
        );
    }

    #[test]
    fn a_watcher_into_node_modules_or_a_dot_directory_reaches_the_editor() {
        // Zed scans a gitignored directory only when a watcher's literal
        // prefix points into it, and drops events in one it has not scanned:
        // without these, a `bun add` is never reported (round-2 review).
        let mut watchers = TsgoWatchers::default();
        assert!(watchers.register(&watch("root", &["/ws/**/*"])).is_some());
        for (id, glob) in [
            ("auto-import 10.0", "/ws/node_modules/**/*"),
            ("nested package", "/ws/app/node_modules/**/*"),
            ("a package elsewhere", "/ws/lib/node_modules/dep/*.json"),
            (
                "mirror link",
                "/ws/.svelte-rs/projects/admin/node_modules/**/*",
            ),
            ("dot directory", "/ws/.config/**/*"),
        ] {
            assert!(watchers.register(&watch(id, &[glob])).is_some(), "{id}");
        }
        // An exact repeat of what the editor has still stays here.
        assert!(
            watchers
                .register(&watch("auto-import 11.0", &["/ws/node_modules/**/*"]))
                .is_none()
        );
        // And the editor's own `node_modules` watcher covers what is in it.
        assert!(
            watchers
                .register(&watch("under it", &["/ws/node_modules/dep/**/*"]))
                .is_none()
        );
        // Only a directory *named* `node_modules` or starting with a dot.
        assert!(
            watchers
                .register(&watch(
                    "lookalikes",
                    &["/ws/my_node_modules.d/x/**/*", "/ws/a.b/**/*"]
                ))
                .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_watcher_into_a_mirror_link_watches_the_directory_it_points_at() {
        let root = std::env::temp_dir().join(format!(
            "rsvelte-watchers-mirror-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let root = {
            fs::create_dir_all(root.join("app/node_modules")).unwrap();
            fs::canonicalize(&root).unwrap()
        };
        let links = [
            root.join(".svelte-rs/projects/app/node_modules"),
            root.join(".svelte-rs/projects/app/tsgo/svelte/app/node_modules"),
        ];
        for link in &links {
            fs::create_dir_all(link.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(root.join("app/node_modules"), link).unwrap();
        }
        let glob = |path: &Path| format!("{}/**/*", path.display());
        let mut watchers = TsgoWatchers::default();
        assert!(
            watchers
                .register(&watch("root", &[&format!("{}/**/*", root.display())]))
                .is_some()
        );
        let forwarded = watchers
            .register(&watch("auto-import 11.0", &[&glob(&links[0])]))
            .expect("a package directory reaches the editor");
        assert_eq!(
            forwarded["registrations"][0]["registerOptions"]["watchers"][0]["globPattern"],
            glob(&root.join("app/node_modules")),
            "on the directory the link points at"
        );
        assert!(
            watchers
                .register(&watch("auto-import 11.1", &[&glob(&links[1])]))
                .is_none(),
            "the other project's link is the same directory"
        );
        // A cache directory that is not a link stays as it is.
        let cache = root.join(".svelte-rs/tsgo");
        fs::create_dir_all(&cache).unwrap();
        let kept = watchers
            .register(&watch("cache", &[&glob(&cache)]))
            .expect("a dot directory reaches the editor");
        assert_eq!(
            kept["registrations"][0]["registerOptions"]["watchers"][0]["globPattern"],
            glob(&cache)
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn only_a_watcher_of_every_kind_and_everything_beneath_covers() {
        let mut watchers = TsgoWatchers::default();
        watchers.register(&json!({ "registrations": [{
            "id": "deletes",
            "method": WATCHED_FILES,
            "registerOptions": { "watchers": [{ "globPattern": "/ws/**/*", "kind": 4 }] },
        }] }));
        assert!(watchers.register(&watch("a", &["/ws/src/**/*"])).is_some());
        assert!(watchers.register(&watch("b", &["/ws/src/*.ts"])).is_none());
        assert!(watchers.register(&watch("c", &["/ws/srcs/**/*"])).is_some());
        assert!(
            watchers
                .register(&watch("d", &["/ws/src/**/*.json", "/elsewhere/x"]))
                .is_some()
        );
        // A relative pattern is its base plus its pattern.
        assert!(
            watchers
                .register(&json!({ "registrations": [{
                    "id": "e",
                    "method": WATCHED_FILES,
                    "registerOptions": { "watchers": [{ "globPattern": { "baseUri": "file:///ws/src/lib", "pattern": "**/*.ts" } }] },
                }] }))
                .is_none()
        );
    }
}
