//! Document URI → filesystem path.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use lsp_types::Uri;

/// Return the filesystem path denoted by a document URI.
///
/// Non-`file:` URIs use the URI itself as the filename for extension-based
/// formatter and linter dispatch.
#[must_use]
pub fn uri_to_path(uri: &str) -> PathBuf {
    let Some(rest) = uri.strip_prefix("file://") else {
        return PathBuf::from(uri);
    };
    // Skip the authority component (`file://host/path`); the usual
    // `file:///path` has an empty one.
    let Some(slash) = rest.find('/') else {
        return PathBuf::from(uri);
    };
    let path = &rest[slash..];
    let path = path.find(['?', '#']).map_or(path, |cut| &path[..cut]);
    let decoded = percent_decode(path);
    // `file:///C:/dir/a.svelte` → `C:\dir\a.svelte`.
    if cfg!(windows) && has_drive_letter(&decoded) {
        return PathBuf::from(&decoded[1..]);
    }
    PathBuf::from(decoded)
}

/// [`std::fs::canonicalize`] without Windows' verbatim prefix. On Windows it
/// answers `\\?\C:\dir`, which an editor and a TypeScript server never send
/// and so never compare equal to, and which [`path_to_uri`] could only have
/// written as `file:////%3F/C:/dir`; elsewhere this is `fs::canonicalize`.
pub fn canonicalize(path: impl AsRef<Path>) -> std::io::Result<PathBuf> {
    std::fs::canonicalize(path).map(strip_verbatim)
}

/// `\\?\C:\dir` → `C:\dir` and `\\?\UNC\server\share\dir` →
/// `\\server\share\dir`, as long as the plain form is not longer than
/// Windows' path limit (which only the verbatim form lifts); any other path
/// unchanged.
#[must_use]
pub fn strip_verbatim(path: PathBuf) -> PathBuf {
    match path.to_str().and_then(plain_form) {
        Some(plain) if plain.len() < 260 => PathBuf::from(plain),
        _ => path,
    }
}

/// The plain form of a verbatim drive or UNC path.
pub(crate) fn plain_form(text: &str) -> Option<String> {
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return Some(format!(r"\\{rest}"));
    }
    let rest = text.strip_prefix(r"\\?\")?;
    has_drive_letter(&format!("/{rest}")).then(|| rest.to_string())
}

/// Return a percent-encoded file URI for a filesystem path.
#[must_use]
pub fn path_to_uri(path: &Path) -> Option<Uri> {
    // A URI has no verbatim form; a long path keeps its prefix on disk only.
    let path = path.to_string_lossy();
    let path = plain_form(&path)
        .unwrap_or_else(|| path.into_owned())
        .replace('\\', "/");
    let mut encoded = String::with_capacity(path.len() + 8);
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b':' | b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            write!(&mut encoded, "%{byte:02X}").ok()?;
        }
    }
    let prefix = if encoded.starts_with('/') {
        "file://"
    } else {
        "file:///"
    };
    Uri::from_str(&format!("{prefix}{encoded}")).ok()
}

fn has_drive_letter(path: &str) -> bool {
    let b = path.as_bytes();
    b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && (b[2] == b':' || b[2] == b'|')
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    if !bytes.contains(&b'%') {
        return s.to_string();
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push(hi * 16 + lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

const fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Windows' `fs::canonicalize` answers verbatim paths; the plain form is
    /// what editors and TypeScript send. Pure string handling, so it runs on
    /// every platform.
    #[test]
    fn a_verbatim_path_loses_its_prefix() {
        assert_eq!(
            strip_verbatim(PathBuf::from(r"\\?\C:\proj\App.svelte")),
            PathBuf::from(r"C:\proj\App.svelte")
        );
        assert_eq!(
            strip_verbatim(PathBuf::from(r"\\?\UNC\server\share\App.svelte")),
            PathBuf::from(r"\\server\share\App.svelte")
        );
        // Only a drive or UNC path has a plain form.
        for kept in [
            r"\\?\Volume{0}\App.svelte",
            r"C:\proj",
            "/home/u/App.svelte",
        ] {
            assert_eq!(strip_verbatim(PathBuf::from(kept)), PathBuf::from(kept));
        }
        // Past Windows' path limit only the verbatim form reaches the file.
        let long = format!(r"\\?\C:\{}", "a".repeat(300));
        assert_eq!(strip_verbatim(PathBuf::from(&long)), PathBuf::from(&long));
    }

    #[test]
    fn a_verbatim_path_is_a_plain_file_uri() {
        assert_eq!(
            path_to_uri(Path::new(r"\\?\C:\proj\my app\App.svelte"))
                .unwrap()
                .as_str(),
            "file:///C:/proj/my%20app/App.svelte"
        );
        assert_eq!(
            path_to_uri(Path::new(r"C:\proj\App.svelte"))
                .unwrap()
                .as_str(),
            "file:///C:/proj/App.svelte"
        );
    }

    /// The standard library's path, less the verbatim prefix it adds on
    /// Windows; elsewhere the two are the same.
    #[test]
    fn canonicalize_resolves_like_the_standard_library() {
        let here = std::env::current_dir().unwrap();
        let resolved = canonicalize(&here).unwrap();
        assert_eq!(
            resolved,
            strip_verbatim(std::fs::canonicalize(&here).unwrap())
        );
        assert!(!resolved.to_string_lossy().starts_with(r"\\?\"));
    }

    #[test]
    fn plain_file_uri() {
        assert_eq!(
            uri_to_path("file:///home/u/App.svelte"),
            PathBuf::from("/home/u/App.svelte")
        );
    }

    #[test]
    fn percent_escapes_are_decoded() {
        assert_eq!(
            uri_to_path("file:///home/u/my%20app/%E3%81%82.svelte"),
            PathBuf::from("/home/u/my app/あ.svelte")
        );
    }

    #[test]
    fn query_and_fragment_are_dropped() {
        assert_eq!(
            uri_to_path("file:///a/b.svelte?v=1#top"),
            PathBuf::from("/a/b.svelte")
        );
    }

    #[test]
    fn non_file_uris_are_passed_through() {
        assert_eq!(
            uri_to_path("untitled:Untitled-1"),
            PathBuf::from("untitled:Untitled-1")
        );
    }

    #[test]
    fn a_trailing_percent_is_left_alone() {
        assert_eq!(uri_to_path("file:///a/b%"), PathBuf::from("/a/b%"));
    }
}
