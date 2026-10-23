use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn new(case: &str) -> Self {
        let unique = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "rsvelte-ls-robustness-{}-{unique}-{case}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    messages: Receiver<Result<Value, String>>,
    reader: Option<JoinHandle<()>>,
    buffered: HashMap<i64, Value>,
    next_id: i64,
    /// What `workspace/configuration` answers for the `rsvelte` section.
    rsvelte_settings: Value,
    /// The methods of the requests the server sent the editor, in order.
    server_requests: Vec<String>,
}

impl Server {
    fn start(root: Option<&Path>, environment: &[(&str, &Path)]) -> Self {
        Self::start_with_settings(root, environment, Value::Null)
    }

    fn start_with_settings(
        root: Option<&Path>,
        environment: &[(&str, &Path)],
        rsvelte_settings: Value,
    ) -> Self {
        Self::start_with_capabilities(root, environment, rsvelte_settings, &json!({}))
    }

    /// `capabilities` is merged into the editor's default ones, a section at
    /// a time (`textDocument`, `workspace`).
    fn start_with_capabilities(
        root: Option<&Path>,
        environment: &[(&str, &Path)],
        rsvelte_settings: Value,
        capabilities: &Value,
    ) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rsvelte-language-server"));
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        for (key, value) in environment {
            command.env(key, value);
        }
        let mut child = command.spawn().expect("spawn language server");
        let stdin = child.stdin.take().expect("language server stdin");
        let stdout = child.stdout.take().expect("language server stdout");
        let (sender, messages) = mpsc::channel();
        let reader = thread::spawn(move || {
            let mut stdout = BufReader::new(stdout);
            loop {
                match read_message(&mut stdout) {
                    Ok(Some(message)) => {
                        if sender.send(Ok(message)).is_err() {
                            return;
                        }
                    }
                    Ok(None) => {
                        let _ = sender.send(Err("language server closed stdout".to_string()));
                        return;
                    }
                    Err(error) => {
                        let _ = sender.send(Err(error));
                        return;
                    }
                }
            }
        });
        let mut server = Self {
            child,
            stdin: Some(stdin),
            messages,
            reader: Some(reader),
            buffered: HashMap::new(),
            next_id: 0,
            rsvelte_settings,
            server_requests: Vec::new(),
        };
        let root_uri = root.map(file_uri).map(Value::String).unwrap_or(Value::Null);
        let mut client_capabilities = json!({
            "general": { "positionEncodings": ["utf-8", "utf-16"] },
            "workspace": { "configuration": true },
        });
        for (section, value) in capabilities.as_object().into_iter().flatten() {
            let merged = client_capabilities
                .as_object_mut()
                .unwrap()
                .entry(section.clone())
                .or_insert_with(|| json!({}));
            for (key, value) in value.as_object().into_iter().flatten() {
                merged[key] = value.clone();
            }
        }
        let id = server.request(
            "initialize",
            json!({
                "processId": Value::Null,
                "rootUri": root_uri,
                "capabilities": client_capabilities,
            }),
        );
        let response = server.response(id, RESPONSE_TIMEOUT);
        assert_eq!(
            response["result"]["serverInfo"]["name"],
            "rsvelte-language-server"
        );
        server.notify("initialized", json!({}));
        server
    }

    fn write(&mut self, message: &Value) {
        let body = serde_json::to_vec(message).expect("serialize LSP message");
        self.write_frame(&body);
    }

    /// The framing stays correct whatever the body is, so a caller can send a
    /// payload the *decoder* rejects without leaving the reader unable to find
    /// where the next frame starts.
    fn write_frame(&mut self, body: &[u8]) {
        let stdin = self.stdin.as_mut().expect("language server stdin is open");
        write!(stdin, "Content-Length: {}\r\n\r\n", body.len()).expect("write LSP header");
        stdin.write_all(body).expect("write LSP body");
        stdin.flush().expect("flush LSP body");
    }

    fn request(&mut self, method: &str, params: Value) -> i64 {
        self.next_id += 1;
        let id = self.next_id;
        self.write(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        id
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.write(&json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    fn response(&mut self, id: i64, timeout: Duration) -> Value {
        if let Some(message) = self.buffered.remove(&id) {
            return message;
        }
        let deadline = Instant::now() + timeout;
        loop {
            let message = self.next_message(deadline);
            if message.get("method").is_some() && message.get("id").is_some() {
                self.answer_server_request(&message);
                continue;
            }
            if let Some(response_id) = message.get("id").and_then(Value::as_i64) {
                if response_id == id {
                    return message;
                }
                self.buffered.insert(response_id, message);
            }
        }
    }

    fn next_message(&self, deadline: Instant) -> Value {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match self.messages.recv_timeout(remaining) {
            Ok(Ok(message)) => message,
            Ok(Err(error)) => panic!("{error}"),
            Err(RecvTimeoutError::Timeout) => panic!("timed out waiting for language server"),
            Err(RecvTimeoutError::Disconnected) => panic!("language server reader stopped"),
        }
    }

    fn answer_server_request(&mut self, message: &Value) {
        let id = message["id"].clone();
        if let Some(method) = message["method"].as_str() {
            self.server_requests.push(method.to_string());
        }
        let result = if message["method"] == "workspace/configuration" {
            let values = message["params"]["items"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|item| {
                    if item["section"] == "rsvelte" {
                        self.rsvelte_settings.clone()
                    } else {
                        Value::Null
                    }
                })
                .collect();
            Value::Array(values)
        } else {
            Value::Null
        };
        self.write(&json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }

    fn open(&mut self, uri: &str, text: &str) {
        self.open_as(uri, "svelte", text);
    }

    fn open_as(&mut self, uri: &str, language_id: &str, text: &str) {
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": language_id,
                    "version": 1,
                    "text": text,
                },
            }),
        );
    }

    fn change(&mut self, uri: &str, version: i64, text: &str) {
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": [{ "text": text }],
            }),
        );
    }

    fn probe(&mut self) {
        let id = self.request("rsvelte/testLiveness", Value::Null);
        let response = self.response(id, RESPONSE_TIMEOUT);
        assert_eq!(response["error"]["code"], -32601);
    }

    fn shutdown(mut self) {
        let id = self.request("shutdown", Value::Null);
        let response = self.response(id, RESPONSE_TIMEOUT);
        assert!(
            response.get("result").is_some(),
            "shutdown failed: {response}"
        );
        self.notify("exit", Value::Null);
        self.stdin.take();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    assert!(status.success(), "language server exited with {status}");
                    break;
                }
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                Ok(None) => panic!("language server did not exit after shutdown"),
                Err(error) => panic!("could not wait for language server: {error}"),
            }
        }
        if let Some(reader) = self.reader.take() {
            reader.join().expect("join language server reader");
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stdin.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn read_message(reader: &mut impl BufRead) -> Result<Option<Value>, String> {
    let mut content_length = None;
    loop {
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .map_err(|error| error.to_string())?;
        if read == 0 {
            return Ok(None);
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            content_length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .map_err(|error| error.to_string())?,
            );
        }
    }
    let length = content_length.ok_or_else(|| "LSP message omitted Content-Length".to_string())?;
    let mut body = vec![0; length];
    reader
        .read_exact(&mut body)
        .map_err(|error| error.to_string())?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|error| error.to_string())
}

fn file_uri(path: &Path) -> String {
    url::Url::from_file_path(path)
        .expect("absolute file path")
        .to_string()
}

fn document_request(server: &mut Server, method: &str, uri: &str) -> Value {
    let id = server.request(method, json!({ "textDocument": { "uri": uri } }));
    server.response(id, RESPONSE_TIMEOUT)
}

#[test]
fn file_uris_encode_spaces_and_non_ascii_paths() {
    let dir = TestDir::new("uri space-雪");
    let uri = file_uri(&dir.0.join("Component name.svelte"));
    assert!(uri.contains("uri%20space-%E9%9B%AA"), "{uri}");
    assert!(uri.ends_with("Component%20name.svelte"), "{uri}");
}

/// Malformed at the *protocol* layer rather than the document layer: a frame
/// whose body will not deserialize into a JSON-RPC message. `lsp_server`'s own
/// stdio transport ends its reader thread on the first one, which closes the
/// connection and takes every open document's language features with it — so
/// this is the one robustness case the sibling test above cannot reach, because
/// its inputs are all valid messages carrying invalid Svelte.
#[test]
fn undecodable_protocol_messages_do_not_end_the_session() {
    let dir = TestDir::new("undecodable");
    let uri = file_uri(&dir.0.join("App.svelte"));
    let mut server = Server::start(None, &[]);
    server.open(&uri, "<script>let value = 1;</script>\n<p>{value}</p>\n");

    for body in [
        "{ this is not json",
        "[]",
        "null",
        "42",
        r#"{"jsonrpc":"2.0"}"#,
        r#"{"jsonrpc":"2.0","method":42}"#,
    ] {
        server.write_frame(body.as_bytes());
        server.probe();
    }

    let response = document_request(&mut server, "textDocument/foldingRange", &uri);
    assert!(
        response.get("result").is_some(),
        "no response after undecodable protocol messages: {response}"
    );
    server.shutdown();
}

#[test]
fn malformed_documents_do_not_end_the_session() {
    let dir = TestDir::new("malformed");
    let mut server = Server::start(None, &[]);
    for (index, source) in [
        "<script>let value = {</script><p>{value}</p>",
        "<style>.broken { color: red;</style><div />",
        "{#if ready}<div>{#each items as item}{item}{/if}",
        "<svelte:component this={Thing><span></svelte:component>",
        "<p>{`unterminated ${value}`</p>",
    ]
    .into_iter()
    .enumerate()
    {
        let uri = file_uri(&dir.0.join(format!("Malformed{index}.svelte")));
        server.open(&uri, source);
        server.probe();
    }
    let uri = file_uri(&dir.0.join("Recovered.svelte"));
    server.open(&uri, "<script>let value = 1;</script>\n<p>{value}</p>\n");
    let response = document_request(&mut server, "textDocument/foldingRange", &uri);
    assert!(
        response.get("result").is_some(),
        "no response after malformed input: {response}"
    );
    server.shutdown();
}

#[test]
fn a_mid_edit_parse_error_recovers_in_the_same_document() {
    let dir = TestDir::new("mid-edit");
    let uri = file_uri(&dir.0.join("App.svelte"));
    let mut server = Server::start(None, &[]);
    server.open(&uri, "<script>let answer = 42;</script>\n<p>{answer}</p>\n");
    assert!(
        document_request(&mut server, "textDocument/foldingRange", &uri)
            .get("result")
            .is_some()
    );

    server.change(&uri, 2, "<script>let answer = {</script>\n{#if answer}<p>");
    assert!(
        document_request(&mut server, "textDocument/documentSymbol", &uri)
            .get("result")
            .is_some()
    );

    server.change(
        &uri,
        3,
        "<script>let answer = 42;</script>\n<p>{answer}</p>\n",
    );
    assert!(
        document_request(&mut server, "textDocument/documentSymbol", &uri)
            .get("result")
            .is_some()
    );
    server.probe();
    server.shutdown();
}

#[test]
fn a_cancellation_storm_finishes_every_request_and_preserves_liveness() {
    let dir = TestDir::new("cancel-storm");
    let uri = file_uri(&dir.0.join("App.svelte"));
    let mut server = Server::start(None, &[]);
    server.open(&uri, &"<div class='item'>value</div>\n".repeat(1_000));

    let mut pending = HashSet::new();
    for _ in 0..24 {
        pending.insert(server.request(
            "textDocument/formatting",
            json!({
                "textDocument": { "uri": uri },
                "options": { "tabSize": 2, "insertSpaces": true },
            }),
        ));
    }
    for id in &pending {
        server.notify("$/cancelRequest", json!({ "id": id }));
    }

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut cancelled = 0;
    while !pending.is_empty() {
        let message = server.next_message(deadline);
        if message.get("method").is_some() && message.get("id").is_some() {
            server.answer_server_request(&message);
            continue;
        }
        let Some(id) = message.get("id").and_then(Value::as_i64) else {
            continue;
        };
        if pending.remove(&id) && message["error"]["code"] == -32800 {
            cancelled += 1;
        }
    }
    assert!(cancelled > 0, "the storm did not exercise cancellation");
    server.probe();
    server.shutdown();
}

#[cfg(unix)]
#[test]
fn a_crashed_tsgo_restarts_replays_the_buffer_and_serves_hover() {
    let dir = TestDir::new("tsgo-restart");
    let helper_source = dir.0.join("fake_tsgo.rs");
    let helper = dir.0.join("fake-tsgo");
    let state = dir.0.join("generation");
    let replayed = dir.0.join("replayed");
    fs::write(&helper_source, FAKE_TSGO).expect("write fake tsgo source");
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    assert!(
        Command::new(rustc)
            .arg("--edition=2024")
            .arg(&helper_source)
            .arg("-o")
            .arg(&helper)
            .status()
            .expect("run rustc")
            .success(),
        "compile fake tsgo"
    );
    fs::write(dir.0.join("package.json"), "{}").expect("write package.json");
    let source = "<script lang=\"ts\">\nlet answer = 42;\n</script>\n<p>{answer}</p>\n";
    let component = dir.0.join("App.svelte");
    fs::write(&component, source).expect("write component");
    let uri = file_uri(&component);
    let mut server = Server::start(
        Some(&dir.0),
        &[
            ("TSGO_BIN", helper.as_path()),
            ("RSVELTE_TEST_TSGO_STATE", state.as_path()),
        ],
    );
    server.open(&uri, source);

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut recovered = false;
    while Instant::now() < deadline && !recovered {
        let id = server.request(
            "textDocument/hover",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": 1, "character": 5 },
            }),
        );
        let response = server.response(id, Duration::from_secs(3));
        recovered = response.to_string().contains("generation-2");
        if !recovered {
            thread::sleep(Duration::from_millis(100));
        }
    }
    assert!(recovered, "hover did not recover after the tsgo crash");
    assert_eq!(fs::read_to_string(&state).unwrap().trim(), "2");
    assert_eq!(fs::read_to_string(&replayed).unwrap().trim(), "2");
    server.shutdown();
}

#[cfg(unix)]
const FAKE_TSGO: &str = r###"
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

fn main() {
    let state = PathBuf::from(std::env::var_os("RSVELTE_TEST_TSGO_STATE").unwrap());
    let generation = fs::read_to_string(&state)
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok())
        .map_or(1, |value| value + 1);
    fs::write(&state, generation.to_string()).unwrap();
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let stdout = io::stdout();
    let mut output = stdout.lock();
    loop {
        let Some(message) = read_message(&mut input).unwrap() else { return };
        if message.contains(r#""method":"initialize""#) {
            let id = json_id(&message);
            write_message(&mut output, &format!(
                r#"{{"jsonrpc":"2.0","id":{id},"result":{{"capabilities":{{"positionEncoding":"utf-8","hoverProvider":true}}}}}}"#
            )).unwrap();
        } else if message.contains(r#""method":"textDocument/didOpen""#) {
            if generation == 1 {
                std::process::exit(7);
            }
            fs::write(state.with_file_name("replayed"), generation.to_string()).unwrap();
        } else if message.contains(r#""method":"textDocument/hover""#) {
            let id = json_id(&message);
            write_message(&mut output, &format!(
                r#"{{"jsonrpc":"2.0","id":{id},"result":{{"contents":{{"kind":"plaintext","value":"generation-{generation}"}}}}}}"#
            )).unwrap();
        } else if message.contains(r#""method":"shutdown""#) {
            let id = json_id(&message);
            write_message(&mut output, &format!(r#"{{"jsonrpc":"2.0","id":{id},"result":null}}"#)).unwrap();
        } else if message.contains(r#""method":"exit""#) {
            return;
        } else if message.contains(r#""method":""#) && message.contains(r#""id":"#) {
            let id = json_id(&message);
            write_message(&mut output, &format!(r#"{{"jsonrpc":"2.0","id":{id},"result":null}}"#)).unwrap();
        }
    }
}

fn read_message(reader: &mut impl BufRead) -> io::Result<Option<String>> {
    let mut content_length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 { return Ok(None) }
        if line == "\r\n" { break }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            content_length = Some(value.trim().parse::<usize>().unwrap());
        }
    }
    let mut body = vec![0; content_length.unwrap()];
    reader.read_exact(&mut body)?;
    Ok(Some(String::from_utf8(body).unwrap()))
}

fn write_message(writer: &mut impl Write, body: &str) -> io::Result<()> {
    write!(writer, "Content-Length: {}\r\n\r\n{body}", body.len())?;
    writer.flush()
}

fn json_id(message: &str) -> &str {
    let rest = message.split_once(r#""id":"#).unwrap().1;
    if let Some(rest) = rest.strip_prefix('"') {
        let end = rest.find('"').unwrap();
        &message[message.len() - rest.len() - 1..message.len() - rest.len() + end + 1]
    } else {
        let end = rest.find(|character: char| !character.is_ascii_digit() && character != '-').unwrap_or(rest.len());
        &rest[..end]
    }
}
"###;

/// Projects load when a document they own is opened, and tsgo only receives
/// that document as a buffer: every other component is a body on disk, under
/// the project's own cache directory.
#[cfg(unix)]
#[test]
fn a_document_loads_only_its_own_project_and_the_rest_is_read_from_disk() {
    let dir = TestDir::new("lazy-projects");
    let helper_source = dir.0.join("fake_tsgo.rs");
    let helper = dir.0.join("fake-tsgo");
    let log = dir.0.join("tsgo.log");
    fs::write(&helper_source, FAKE_TSGO_RECORDER).expect("write fake tsgo source");
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    assert!(
        Command::new(rustc)
            .arg("--edition=2024")
            .arg(&helper_source)
            .arg("-o")
            .arg(&helper)
            .status()
            .expect("run rustc")
            .success(),
        "compile fake tsgo"
    );
    let root = rsvelte_language_server::uri::canonicalize(&dir.0).unwrap();
    fs::write(root.join("package.json"), "{}").unwrap();
    for project in ["a", "b"] {
        fs::create_dir_all(root.join(project)).unwrap();
        fs::write(root.join(project).join("tsconfig.json"), "{}").unwrap();
    }
    let source = "<script lang=\"ts\">\nlet answer = 42;\n</script>\n<p>{answer}</p>\n";
    let x_source =
        "<script lang=\"ts\">\nimport Legacy from './Legacy.svelte';\n</script>\n<Legacy />\n";
    fs::write(root.join("a/X.svelte"), x_source).unwrap();
    fs::write(
        root.join("a/Sibling.svelte"),
        "<script lang=\"ts\"></script><p />",
    )
    .unwrap();
    // A JavaScript component the document reaches is the exception: its
    // script kind comes from the buffer's language id, so its project opens
    // it before the document. One nothing open reaches stays on disk.
    fs::write(root.join("a/Legacy.svelte"), "<p />").unwrap();
    fs::write(root.join("a/Unreached.svelte"), "<p />").unwrap();
    fs::write(root.join("b/Y.svelte"), source).unwrap();
    let mut server = Server::start_with_settings(
        Some(&root),
        &[
            ("TSGO_BIN", helper.as_path()),
            ("RSVELTE_TEST_TSGO_LOG", log.as_path()),
        ],
        json!({ "backgroundProjects": { "enable": false } }),
    );
    let opened = |log: &Path| {
        fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.strip_prefix("didOpen "))
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    let shadow = |project: &str, file: &str| {
        root.join(".svelte-language-server-rs/projects")
            .join(project)
            .join("tsgo/svelte")
            .join(project)
            .join(format!("{file}.svelte.tsx"))
    };
    let hover = |server: &mut Server, uri: &str| {
        let id = server.request(
            "textDocument/hover",
            json!({ "textDocument": { "uri": uri }, "position": { "line": 1, "character": 5 } }),
        );
        server.response(id, Duration::from_secs(5))
    };

    let x = file_uri(&root.join("a/X.svelte"));
    server.open(&x, x_source);
    assert!(hover(&mut server, &x).to_string().contains("recorded"));
    assert_eq!(
        opened(&log),
        vec![
            file_uri(&shadow("a", "Legacy")),
            file_uri(&shadow("a", "X"))
        ],
        "only the editor's document (and a JavaScript component it imports) is a tsgo buffer"
    );
    assert!(
        shadow("a", "Unreached").is_file(),
        "a JavaScript component no open document reaches is read from disk"
    );
    assert!(
        shadow("a", "Sibling").is_file(),
        "the project's other component is on disk"
    );
    assert!(
        !shadow("b", "Y").exists(),
        "an unopened project is not loaded"
    );

    let y = file_uri(&root.join("b/Y.svelte"));
    server.open(&y, source);
    assert!(hover(&mut server, &y).to_string().contains("recorded"));
    assert_eq!(
        opened(&log),
        vec![
            file_uri(&shadow("a", "Legacy")),
            file_uri(&shadow("a", "X")),
            file_uri(&shadow("b", "Y"))
        ]
    );
    server.shutdown();
}

#[cfg(unix)]
const FAKE_TSGO_RECORDER: &str = r###"
use std::fs::OpenOptions;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

fn main() {
    let log = PathBuf::from(std::env::var_os("RSVELTE_TEST_TSGO_LOG").unwrap());
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let stdout = io::stdout();
    let mut output = stdout.lock();
    loop {
        let Some(message) = read_message(&mut input).unwrap() else { return };
        if message.contains(r#""method":"initialize""#) {
            let id = json_id(&message);
            write_message(&mut output, &format!(
                r#"{{"jsonrpc":"2.0","id":{id},"result":{{"capabilities":{{"positionEncoding":"utf-8","hoverProvider":true}}}}}}"#
            )).unwrap();
        } else if message.contains(r#""method":"textDocument/didOpen""#) {
            let uri = message.split_once(r#""uri":""#).unwrap().1.split('"').next().unwrap().to_string();
            let mut file = OpenOptions::new().create(true).append(true).open(&log).unwrap();
            // One write per line: a restarted tsgo appends to the same log.
            file.write_all(format!("didOpen {uri}\n").as_bytes()).unwrap();
        } else if message.contains(r#""method":"textDocument/hover""#) {
            let id = json_id(&message);
            write_message(&mut output, &format!(
                r#"{{"jsonrpc":"2.0","id":{id},"result":{{"contents":{{"kind":"plaintext","value":"recorded"}}}}}}"#
            )).unwrap();
        } else if message.contains(r#""method":"shutdown""#) {
            let id = json_id(&message);
            write_message(&mut output, &format!(r#"{{"jsonrpc":"2.0","id":{id},"result":null}}"#)).unwrap();
        } else if message.contains(r#""method":"exit""#) {
            return;
        } else if message.contains(r#""method":""#) && message.contains(r#""id":"#) {
            let id = json_id(&message);
            write_message(&mut output, &format!(r#"{{"jsonrpc":"2.0","id":{id},"result":null}}"#)).unwrap();
        }
    }
}

fn read_message(reader: &mut impl BufRead) -> io::Result<Option<String>> {
    let mut content_length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 { return Ok(None) }
        if line == "\r\n" { break }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            content_length = Some(value.trim().parse::<usize>().unwrap());
        }
    }
    let mut body = vec![0; content_length.unwrap()];
    reader.read_exact(&mut body)?;
    Ok(Some(String::from_utf8(body).unwrap()))
}

fn write_message(writer: &mut impl Write, body: &str) -> io::Result<()> {
    write!(writer, "Content-Length: {}\r\n\r\n{body}", body.len())?;
    writer.flush()
}

fn json_id(message: &str) -> &str {
    let rest = message.split_once(r#""id":"#).unwrap().1;
    if let Some(rest) = rest.strip_prefix('"') {
        let end = rest.find('"').unwrap();
        &message[message.len() - rest.len() - 1..message.len() - rest.len() + end + 1]
    } else {
        let end = rest.find(|character: char| !character.is_ascii_digit() && character != '-').unwrap_or(rest.len());
        &rest[..end]
    }
}
"###;

/// Compile a fake tsgo that logs every buffer message and request it gets.
fn traffic_recorder(dir: &Path) -> PathBuf {
    let source = dir.join("fake_tsgo_traffic.rs");
    // Windows starts only a name with an executable's extension.
    let helper = dir.join(format!("fake-tsgo-traffic{}", std::env::consts::EXE_SUFFIX));
    fs::write(&source, FAKE_TSGO_TRAFFIC).expect("write fake tsgo source");
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    assert!(
        Command::new(rustc)
            .arg("--edition=2024")
            .arg(&source)
            .arg("-o")
            .arg(&helper)
            .status()
            .expect("run rustc")
            .success(),
        "compile fake tsgo"
    );
    helper
}

/// The recorded traffic: one `<method> <uri>[ marked]` line per message, where
/// `marked` says the text carried `RSVELTE_TEST_MARKER`.
fn traffic(log: &Path) -> Vec<String> {
    fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

fn hover(server: &mut Server, uri: &str) -> Value {
    let id = server.request(
        "textDocument/hover",
        json!({ "textDocument": { "uri": uri }, "position": { "line": 1, "character": 5 } }),
    );
    server.response(id, Duration::from_secs(5))
}

/// A preprocessor's output is for the compiler: tsgo checks the component as
/// written, as upstream's TypeScript plugin does (`DocumentSnapshot.ts`,
/// `preprocessSvelteFile`: svelte2tsx runs on `document.getText()`).
/// Projected from `vitePreprocess({ script: true })`'s output, a TypeScript
/// component had lost every type before tsgo saw it: annotated parameters were
/// "implicitly any", and an error typed into it was never reported. Here the
/// preprocessor drops the script's annotation and adds an `<img>` without
/// `alt`, which only the compiler, reading its output, can report.
#[test]
fn tsgo_checks_the_component_as_written_and_the_compiler_as_preprocessed() {
    if Command::new("node").arg("--version").output().is_err() {
        return;
    }
    let dir = TestDir::new("preprocessed-script");
    let helper = traffic_recorder(&dir.0);
    let log = dir.0.join("tsgo.log");
    let root = rsvelte_language_server::uri::canonicalize(&dir.0)
        .unwrap()
        .join("ws");
    let svelte = root.join("node_modules/svelte");
    fs::create_dir_all(&svelte).unwrap();
    fs::write(root.join("package.json"), "{}").unwrap();
    fs::write(root.join("tsconfig.json"), "{}").unwrap();
    fs::write(
        svelte.join("package.json"),
        r#"{"name":"svelte","type":"module","exports":{"./compiler":"./compiler.js"}}"#,
    )
    .unwrap();
    fs::write(
        svelte.join("compiler.js"),
        "export async function preprocess(source, { markup }, options) {\n\
         \tconst { code, map } = await markup({ content: source, filename: options?.filename });\n\
         \treturn { code, map, dependencies: [] };\n\
         }\n",
    )
    .unwrap();
    fs::write(
        root.join("svelte.config.mjs"),
        "export default { preprocess: { markup: ({ content }) => ({\n\
         \tcode: content.replace(': TypedAway', '').replace('<p>', '<img><p>'),\n\
         \tmap: { version: 3, sources: ['App.svelte'], names: [], mappings: 'AAAA;AACA;AACA;AACA' },\n\
         }) } };\n",
    )
    .unwrap();
    let source = "<script lang=\"ts\">\n\tlet value: TypedAway = 1;\n</script>\n<p>{value}</p>\n";
    fs::write(root.join("App.svelte"), source).unwrap();
    let mut server = Server::start_with_capabilities(
        Some(&root),
        &[
            ("TSGO_BIN", helper.as_path()),
            ("RSVELTE_TEST_TSGO_LOG", log.as_path()),
            ("RSVELTE_TEST_MARKER", Path::new("TypedAway")),
        ],
        json!({ "backgroundProjects": { "enable": false } }),
        &json!({ "textDocument": { "diagnostic": {} } }),
    );
    let uri = file_uri(&root.join("App.svelte"));
    server.open(&uri, source);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let pull = server.request(
            "textDocument/diagnostic",
            json!({ "textDocument": { "uri": uri } }),
        );
        let report = server.response(pull, Duration::from_secs(20));
        let items = report["result"]["items"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if items
            .iter()
            .any(|item| item["code"] == "a11y_missing_attribute")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the compiler never read the preprocessed component: {report}"
        );
        thread::sleep(Duration::from_millis(100));
    }
    hover(&mut server, &uri);
    let sent = traffic(&log)
        .into_iter()
        .filter(|line| {
            line.contains("App.svelte.tsx")
                && (line.starts_with("textDocument/didOpen ")
                    || line.starts_with("textDocument/didChange "))
        })
        .collect::<Vec<_>>();
    assert!(
        !sent.is_empty() && sent.iter().all(|line| line.ends_with(" marked")),
        "tsgo was sent the preprocessed component: {sent:?}"
    );
    server.shutdown();
}

/// A `Cannot find name` on a component's tag in the markup gets the
/// component's import as a quick fix. TypeScript's own fix cannot find a
/// component, whose default export carries svelte2tsx's suffix, so, as
/// upstream does, the fix is made from the auto-import completion at the tag,
/// resolved for its edits; the import lands on the line after the script's
/// last import, not in the `<script` tag.
#[cfg(unix)]
#[test]
fn a_component_tag_without_an_import_is_offered_one() {
    let dir = TestDir::new("component-import-fix");
    let helper = traffic_recorder(&dir.0);
    let log = dir.0.join("tsgo.log");
    let root = rsvelte_language_server::uri::canonicalize(&dir.0)
        .unwrap()
        .join("ws");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("package.json"), "{}").unwrap();
    fs::write(root.join("tsconfig.json"), "{}").unwrap();
    fs::write(root.join("MyComponent.svelte"), "<p>x</p>\n").unwrap();
    fs::write(root.join("OtherWidget.svelte"), "<p>y</p>\n").unwrap();
    let page = "<script lang=\"ts\">\n\timport MyComponent from './MyComponent.svelte';\n</script>\n\n<MyComponent />\n<OtherWidget />\n";
    fs::write(root.join("Page.svelte"), page).unwrap();
    let mut server = Server::start(
        Some(&root),
        &[
            ("TSGO_BIN", helper.as_path()),
            ("RSVELTE_TEST_TSGO_LOG", log.as_path()),
            ("RSVELTE_TEST_COMPONENT_IMPORT", Path::new("OtherWidget")),
        ],
    );
    let uri = file_uri(&root.join("Page.svelte"));
    server.open(&uri, page);
    hover(&mut server, &uri);
    let diagnostic = json!({
        "range": { "start": { "line": 5, "character": 1 }, "end": { "line": 5, "character": 12 } },
        "severity": 1,
        "code": 2304,
        "source": "ts",
        "message": "Cannot find name 'OtherWidget'.",
    });
    let id = server.request(
        "textDocument/codeAction",
        json!({
            "textDocument": { "uri": uri },
            "range": diagnostic["range"],
            "context": { "diagnostics": [diagnostic], "only": ["quickfix"] },
        }),
    );
    let response = server.response(id, Duration::from_secs(10));
    let actions = response["result"].as_array().expect("code actions");
    assert_eq!(actions.len(), 1, "{response}");
    assert_eq!(
        actions[0]["title"],
        "Add import from \"./OtherWidget.svelte\""
    );
    assert_eq!(
        actions[0]["edit"]["changes"][uri.as_str()],
        json!([{
            "range": { "start": { "line": 2, "character": 0 }, "end": { "line": 2, "character": 0 } },
            "newText": "import OtherWidget from \"./OtherWidget.svelte\";\n",
        }]),
        "{response}"
    );
    let traffic = traffic(&log);
    for method in [
        "textDocument/codeAction",
        "textDocument/completion",
        "completionItem/resolve",
    ] {
        assert_eq!(
            traffic
                .iter()
                .filter(|line| line.starts_with(&format!("{method} ")))
                .count(),
            1,
            "{method}: {traffic:?}"
        );
    }
    server.shutdown();
}

/// A project that loads after an editor buffer changed holds a copy of that
/// component: the copy must carry the buffer, not the file (review s1).
#[cfg(unix)]
#[test]
fn a_project_loaded_later_gets_the_unsaved_buffers_it_copies() {
    let dir = TestDir::new("late-copy");
    let helper = traffic_recorder(&dir.0);
    let log = dir.0.join("tsgo.log");
    let root = rsvelte_language_server::uri::canonicalize(&dir.0)
        .unwrap()
        .join("ws");
    fs::create_dir_all(root.join("a")).unwrap();
    fs::create_dir_all(root.join("b")).unwrap();
    fs::write(root.join("package.json"), "{}").unwrap();
    fs::write(root.join("a/tsconfig.json"), "{}").unwrap();
    fs::write(root.join("b/tsconfig.json"), "{}").unwrap();
    let parent = "<script lang=\"ts\">\nimport Child from '../b/Child.svelte';\n</script>\n<Child count={1} />\n";
    let child = |kind: &str| {
        format!(
            "<script lang=\"ts\">\nlet {{ count }}: {{ count: {kind} }} = $props();\n</script>\n<p>{{count}}</p>\n"
        )
    };
    fs::write(root.join("a/Parent.svelte"), parent).unwrap();
    fs::write(root.join("b/Child.svelte"), child("number")).unwrap();
    let mut server = Server::start(
        Some(&root),
        &[
            ("TSGO_BIN", helper.as_path()),
            ("RSVELTE_TEST_TSGO_LOG", log.as_path()),
            ("RSVELTE_TEST_MARKER", Path::new("UnsavedMarker")),
        ],
    );
    let child_uri = file_uri(&root.join("b/Child.svelte"));
    server.open(&child_uri, &child("number"));
    server.change(&child_uri, 2, &child("UnsavedMarker"));
    hover(&mut server, &child_uri);
    let copy = file_uri(
        &root.join(".svelte-language-server-rs/projects/a/tsgo/svelte/b/Child.svelte.tsx"),
    );
    assert!(
        !traffic(&log).iter().any(|line| line.contains(&copy)),
        "project a is not loaded yet"
    );

    let parent_uri = file_uri(&root.join("a/Parent.svelte"));
    server.open(&parent_uri, parent);
    hover(&mut server, &parent_uri);
    let copied = traffic(&log)
        .into_iter()
        .filter(|line| line.contains(&copy))
        .collect::<Vec<_>>();
    assert!(
        copied.iter().any(|line| line.ends_with(" marked")),
        "a's copy of the child is the unsaved buffer: {copied:?}"
    );
    server.shutdown();
}

/// Component-attribute completion asks tsgo about the component through the
/// project that owns the document, even when another project holding a copy
/// of it loaded first (review s9).
#[cfg(unix)]
#[test]
fn component_attribute_completion_asks_the_owning_project() {
    let dir = TestDir::new("component-query-owner");
    let helper = traffic_recorder(&dir.0);
    let log = dir.0.join("tsgo.log");
    let root = rsvelte_language_server::uri::canonicalize(&dir.0)
        .unwrap()
        .join("ws");
    fs::create_dir_all(root.join("a")).unwrap();
    fs::create_dir_all(root.join("b")).unwrap();
    fs::write(root.join("package.json"), "{}").unwrap();
    fs::write(root.join("a/tsconfig.json"), "{}").unwrap();
    fs::write(root.join("b/tsconfig.json"), "{}").unwrap();
    let parent =
        "<script lang=\"ts\">\n\timport Child from './Child.svelte';\n</script>\n\n<Child  />\n";
    fs::write(root.join("b/Parent.svelte"), parent).unwrap();
    fs::write(
        root.join("b/Child.svelte"),
        "<script lang=\"ts\">\n\tlet { alpha }: { alpha: string } = $props();\n</script>\n",
    )
    .unwrap();
    let uses = "<script lang=\"ts\">\n\timport Parent from '../b/Parent.svelte';\n</script>\n\n<Parent />\n";
    fs::write(root.join("a/Uses.svelte"), uses).unwrap();
    let mut server = Server::start(
        Some(&root),
        &[
            ("TSGO_BIN", helper.as_path()),
            ("RSVELTE_TEST_TSGO_LOG", log.as_path()),
        ],
    );
    // The project holding a copy of Parent loads first.
    let uses_uri = file_uri(&root.join("a/Uses.svelte"));
    server.open(&uses_uri, uses);
    hover(&mut server, &uses_uri);
    let parent_uri = file_uri(&root.join("b/Parent.svelte"));
    server.open(&parent_uri, parent);
    let id = server.request(
        "textDocument/completion",
        json!({
            "textDocument": { "uri": parent_uri },
            "position": { "line": 4, "character": 7 },
            "context": { "triggerKind": 1 },
        }),
    );
    let response = server.response(id, Duration::from_secs(10));
    assert!(response.get("result").is_some(), "{response}");
    let completions = traffic(&log)
        .into_iter()
        .filter(|line| line.starts_with("textDocument/completion "))
        .collect::<Vec<_>>();
    assert!(
        completions.len() >= 2,
        "the editor's completion and the component query: {completions:?}"
    );
    assert!(
        completions
            .iter()
            .all(|line| line.contains("/.svelte-language-server-rs/projects/b/")),
        "{completions:?}"
    );
    server.shutdown();
}

/// Component-attribute completion probes in the shadow's own buffer, which
/// tsgo reuses the program for, instead of opening a document beside it
/// (a new root file: two full program builds per completion). Nothing else
/// reads the shadow until the probe is swapped back out.
#[cfg(unix)]
#[test]
fn component_attribute_completion_probes_in_the_shadow_and_holds_other_requests() {
    let dir = TestDir::new("component-query-in-place");
    let helper = traffic_recorder(&dir.0);
    let log = dir.0.join("tsgo.log");
    let root = rsvelte_language_server::uri::canonicalize(&dir.0)
        .unwrap()
        .join("ws");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("package.json"), "{}").unwrap();
    fs::write(root.join("tsconfig.json"), "{}").unwrap();
    let parent =
        "<script lang=\"ts\">\n\timport Child from './Child.svelte';\n</script>\n\n<Child  />\n";
    fs::write(root.join("Parent.svelte"), parent).unwrap();
    fs::write(
        root.join("Child.svelte"),
        "<script lang=\"ts\">\n\tlet { alpha }: { alpha: string } = $props();\n</script>\n",
    )
    .unwrap();
    let mut server = Server::start_with_settings(
        Some(&root),
        &[
            ("TSGO_BIN", helper.as_path()),
            ("RSVELTE_TEST_TSGO_LOG", log.as_path()),
            (
                "RSVELTE_TEST_MARKER",
                Path::new("__rsvelte_component_info_probe"),
            ),
            ("RSVELTE_TEST_SLOW_COMPLETION_MS", Path::new("150")),
        ],
        json!({ "backgroundProjects": { "enable": false } }),
    );
    let parent_uri = file_uri(&root.join("Parent.svelte"));
    server.open(&parent_uri, parent);
    hover(&mut server, &parent_uri);
    let shadow = file_uri(&root.join(".svelte-language-server-rs/tsgo/svelte/Parent.svelte.tsx"));
    let completion = server.request(
        "textDocument/completion",
        json!({
            "textDocument": { "uri": parent_uri },
            "position": { "line": 4, "character": 7 },
            "context": { "triggerKind": 1 },
        }),
    );
    // Once the probe is in the shadow, ask about the document.
    let probing = format!("textDocument/didChange {shadow} marked");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !traffic(&log).contains(&probing) {
        assert!(Instant::now() < deadline, "no probe: {:?}", traffic(&log));
        thread::sleep(Duration::from_millis(5));
    }
    let held = server.request(
        "textDocument/hover",
        json!({ "textDocument": { "uri": parent_uri }, "position": { "line": 1, "character": 5 } }),
    );
    assert!(
        server
            .response(completion, Duration::from_secs(10))
            .get("result")
            .is_some()
    );
    assert!(
        server
            .response(held, Duration::from_secs(10))
            .to_string()
            .contains("recorded")
    );
    let lines = traffic(&log);
    assert!(
        !lines
            .iter()
            .any(|line| line.contains(".rsvelte-component-info")),
        "no probe document: {lines:?}"
    );
    let restored = lines
        .iter()
        .rposition(|line| line == &format!("textDocument/didChange {shadow}"))
        .expect("the shadow's own text comes back");
    let last_probe = lines.iter().rposition(|line| line == &probing).unwrap();
    assert!(last_probe < restored);
    let hover_after = lines
        .iter()
        .rposition(|line| line == &format!("textDocument/hover {shadow}"))
        .unwrap();
    assert!(
        restored < hover_after,
        "the hover waits for the shadow's own text: {lines:?}"
    );
    server.shutdown();
}

/// Requests held while a probe is in the shadow arrived before the edit that
/// ends it, so they are answered against the text they were sent for: the
/// editor applies a reply's edits to the version it asked about (round-2
/// review: the formatting reply was an edit for the next version, and tsgo
/// answered a held request while the shadow still held the probe).
#[cfg(unix)]
#[test]
fn an_edit_during_a_probe_answers_the_held_requests_against_the_text_before_it() {
    let dir = TestDir::new("component-query-held-before-edit");
    let helper = traffic_recorder(&dir.0);
    let log = dir.0.join("tsgo.log");
    let root = rsvelte_language_server::uri::canonicalize(&dir.0)
        .unwrap()
        .join("ws");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("package.json"), "{}").unwrap();
    fs::write(root.join("tsconfig.json"), "{}").unwrap();
    let parent = "<script lang=\"ts\">\n\timport Child from './Child.svelte';\n\tconst later: number   =   1;\n</script>\n\n<Child  />\n<p>{later}</p>\n";
    fs::write(root.join("Parent.svelte"), parent).unwrap();
    fs::write(
        root.join("Child.svelte"),
        "<script lang=\"ts\">\n\tlet { alpha }: { alpha: string } = $props();\n</script>\n",
    )
    .unwrap();
    let mut server = Server::start_with_settings(
        Some(&root),
        &[
            ("TSGO_BIN", helper.as_path()),
            ("RSVELTE_TEST_TSGO_LOG", log.as_path()),
            (
                "RSVELTE_TEST_MARKER",
                Path::new("__rsvelte_component_info_probe"),
            ),
            ("RSVELTE_TEST_SLOW_COMPLETION_MS", Path::new("300")),
        ],
        json!({ "backgroundProjects": { "enable": false } }),
    );
    let parent_uri = file_uri(&root.join("Parent.svelte"));
    server.open(&parent_uri, parent);
    hover(&mut server, &parent_uri);
    let formatting = |server: &mut Server| {
        server.request(
            "textDocument/formatting",
            json!({
                "textDocument": { "uri": parent_uri },
                "options": { "tabSize": 2, "insertSpaces": false },
            }),
        )
    };
    let quiet = formatting(&mut server);
    let expected = server.response(quiet, Duration::from_secs(10))["result"].clone();
    assert!(
        expected.as_array().is_some_and(|edits| !edits.is_empty()),
        "the document needs formatting: {expected}"
    );

    let shadow = file_uri(&root.join(".svelte-language-server-rs/tsgo/svelte/Parent.svelte.tsx"));
    let completion = server.request(
        "textDocument/completion",
        json!({
            "textDocument": { "uri": parent_uri },
            "position": { "line": 5, "character": 7 },
            "context": { "triggerKind": 1 },
        }),
    );
    let probing = format!("textDocument/didChange {shadow} marked");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !traffic(&log).contains(&probing) {
        assert!(Instant::now() < deadline, "no probe: {:?}", traffic(&log));
        thread::sleep(Duration::from_millis(5));
    }
    // While the probe is in: a formatting request and a request tsgo answers
    // (references go to tsgo directly), then an edit.
    let held_formatting = formatting(&mut server);
    let held_references = server.request(
        "textDocument/references",
        json!({
            "textDocument": { "uri": parent_uri },
            "position": { "line": 2, "character": 8 },
            "context": { "includeDeclaration": true },
        }),
    );
    // A hover in the script: the native worker has nothing, so tsgo answers.
    let held_hover = server.request(
        "textDocument/hover",
        json!({ "textDocument": { "uri": parent_uri }, "position": { "line": 2, "character": 8 } }),
    );
    server.change(&parent_uri, 2, &format!("<!-- top -->\n{parent}"));

    assert!(
        server
            .response(completion, Duration::from_secs(10))
            .get("result")
            .is_some()
    );
    let reply = server.response(held_formatting, Duration::from_secs(10));
    assert_eq!(
        reply["result"], expected,
        "formatting answers for the version it was sent for"
    );
    assert!(
        server
            .response(held_references, Duration::from_secs(10))
            .get("result")
            .is_some()
    );
    let lines = traffic(&log);
    let last_probe = lines.iter().rposition(|line| line == &probing).unwrap();
    let references = lines
        .iter()
        .position(|line| line == &format!("textDocument/references {shadow}"))
        .expect("forwarded");
    assert!(last_probe < references, "{lines:?}");
    let changes_between = lines[last_probe + 1..references]
        .iter()
        .filter(|line| *line == &format!("textDocument/didChange {shadow}"))
        .count();
    assert_eq!(
        changes_between, 1,
        "the shadow's own text, and not the edit, comes back before tsgo hears the held request: {lines:?}"
    );
    assert!(
        server
            .response(held_hover, Duration::from_secs(10))
            .to_string()
            .contains("recorded")
    );
    // tsgo reads the edit after it answered the hover, if the hover came first.
    let deadline = Instant::now() + Duration::from_secs(10);
    let (lines, hover_after, edit) = loop {
        let lines = traffic(&log);
        let hover_after = lines
            .iter()
            .rposition(|line| line == &format!("textDocument/hover {shadow}"))
            .unwrap();
        let edit = lines
            .iter()
            .rposition(|line| line == &format!("textDocument/didChange {shadow}"))
            .unwrap();
        if hover_after < edit || Instant::now() > deadline {
            break (lines, hover_after, edit);
        }
        thread::sleep(Duration::from_millis(5));
    };
    assert!(
        references < hover_after && hover_after < edit,
        "the held hover's tsgo half reaches tsgo before the edit: {lines:?}"
    );
    server.shutdown();
}

/// A request that takes the native worker's answer first and asks tsgo only
/// when it has none (a hover in the script) arrived before the edit that
/// follows it, so its tsgo half reaches tsgo before the edit does. Otherwise
/// the main loop could take the edit first, and tsgo would describe whatever
/// the next version has at that position: a hover on one line answered with
/// the symbol on the line below.
#[cfg(unix)]
#[test]
fn a_request_sent_before_an_edit_reaches_tsgo_before_it() {
    let dir = TestDir::new("native-half-before-edit");
    let helper = traffic_recorder(&dir.0);
    let log = dir.0.join("tsgo.log");
    let root = rsvelte_language_server::uri::canonicalize(&dir.0)
        .unwrap()
        .join("ws");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("package.json"), "{}").unwrap();
    fs::write(root.join("tsconfig.json"), "{}").unwrap();
    let mut text = "<script lang=\"ts\">\n\tconst later: number = 1;\n\tconst other: string = 'x';\n</script>\n<p>{later}{other}</p>\n".to_string();
    fs::write(root.join("P.svelte"), &text).unwrap();
    let mut server = Server::start_with_settings(
        Some(&root),
        &[
            ("TSGO_BIN", helper.as_path()),
            ("RSVELTE_TEST_TSGO_LOG", log.as_path()),
        ],
        json!({ "backgroundProjects": { "enable": false } }),
    );
    let uri = file_uri(&root.join("P.svelte"));
    server.open(&uri, &text);
    hover(&mut server, &uri);
    let shadow = file_uri(&root.join(".svelte-language-server-rs/tsgo/svelte/P.svelte.tsx"));
    let hover_line = format!("textDocument/hover {shadow}");
    let change_line = format!("textDocument/didChange {shadow}");
    for round in 0..5_usize {
        let hovers = (0..4)
            .map(|_| {
                server.request(
                    "textDocument/hover",
                    json!({ "textDocument": { "uri": uri }, "position": { "line": 1 + round, "character": 8 } }),
                )
            })
            .collect::<Vec<_>>();
        text.insert_str(0, "<!-- top -->\n");
        server.change(&uri, i64::try_from(round).unwrap() + 2, &text);
        for id in hovers {
            assert!(
                server
                    .response(id, Duration::from_secs(10))
                    .to_string()
                    .contains("recorded")
            );
        }
        // tsgo reads the edit after it answered the hovers.
        let deadline = Instant::now() + Duration::from_secs(10);
        let lines = loop {
            let lines = traffic(&log);
            if lines.iter().filter(|line| *line == &change_line).count() == round + 1 {
                break lines;
            }
            assert!(Instant::now() < deadline, "the edit never reached tsgo");
            thread::sleep(Duration::from_millis(5));
        };
        let edit = lines
            .iter()
            .rposition(|line| line == &change_line)
            .expect("the edit reached tsgo");
        let hovers_before = lines[..edit]
            .iter()
            .filter(|line| *line == &hover_line)
            .count();
        let hovers_in_total = lines.iter().filter(|line| *line == &hover_line).count();
        assert_eq!(
            hovers_in_total,
            1 + 4 * (round + 1),
            "tsgo answers each hover"
        );
        assert_eq!(
            hovers_before, hovers_in_total,
            "round {round}: every hover reaches tsgo before the edit: {lines:?}"
        );
    }
    server.shutdown();
}

/// A diagnostic pull asks tsgo when it arrives, while the worker analyses the
/// component, and the answer merges both. An edit that follows it does not
/// wait for that analysis (a large component's takes tens of milliseconds in
/// a release build), and a hover sent after the edit is not queued behind
/// it: typing in a large component used to wait for the previous
/// keystroke's analysis at every keystroke.
#[cfg(unix)]
#[test]
fn an_edit_after_a_diagnostic_pull_waits_for_neither_of_its_halves() {
    let dir = TestDir::new("pull-joins-its-halves");
    let helper = traffic_recorder(&dir.0);
    let log = dir.0.join("tsgo.log");
    let root = rsvelte_language_server::uri::canonicalize(&dir.0)
        .unwrap()
        .join("ws");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("package.json"), "{}").unwrap();
    fs::write(root.join("tsconfig.json"), "{}").unwrap();
    let mut text = "<script lang=\"ts\">\n\tconst later: number = 1;\n".to_string();
    let mut template = "<img src=\"a.png\">\n".to_string();
    for index in 0..1_500 {
        writeln!(text, "\tlet v{index} = $state({index});").unwrap();
        writeln!(template, "<div class=\"r{index}\">{{v{index}}}</div>").unwrap();
    }
    text.push_str("</script>\n");
    text.push_str(&template);
    fs::write(root.join("P.svelte"), &text).unwrap();
    let mut server = Server::start_with_settings(
        Some(&root),
        &[
            ("TSGO_BIN", helper.as_path()),
            ("RSVELTE_TEST_TSGO_LOG", log.as_path()),
        ],
        json!({ "backgroundProjects": { "enable": false } }),
    );
    let uri = file_uri(&root.join("P.svelte"));
    server.open(&uri, &text);
    hover(&mut server, &uri);
    let shadow = file_uri(&root.join(".svelte-language-server-rs/tsgo/svelte/P.svelte.tsx"));

    let pull = server.request(
        "textDocument/diagnostic",
        json!({ "textDocument": { "uri": uri } }),
    );
    text.insert_str(0, "<!-- top -->\n");
    server.change(&uri, 2, &text);
    let hovered = server.request(
        "textDocument/hover",
        json!({ "textDocument": { "uri": uri }, "position": { "line": 2, "character": 8 } }),
    );
    assert!(
        server
            .response(hovered, Duration::from_secs(30))
            .to_string()
            .contains("recorded")
    );
    assert!(
        !server.buffered.contains_key(&pull),
        "the hover after the edit is answered before the pull's analysis is done"
    );
    let report = server.response(pull, Duration::from_secs(60));
    assert!(
        report["result"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["message"].as_str().unwrap().contains("alt")),
        "the worker's diagnostics are in the answer: {report}"
    );
    let lines = traffic(&log);
    let asked = lines
        .iter()
        .position(|line| line == &format!("textDocument/diagnostic {shadow}"))
        .expect("tsgo was asked");
    let edit = lines
        .iter()
        .rposition(|line| line == &format!("textDocument/didChange {shadow}"))
        .expect("the edit reached tsgo");
    assert!(asked < edit, "{lines:?}");
    server.shutdown();
}

/// A TypeScript buffer whose text makes tsrs loop on a signature help or
/// completion at the returned position (after `c`: every `}` there jumps to
/// the last `{`, which comes after it; see `tsgo_loop`).
#[cfg(unix)]
fn looping_buffer(dir: &Path) -> (PathBuf, &'static str, Value) {
    (
        dir.join("lost.ts"),
        "export const q = w<x>;\n}c{\n",
        json!({ "line": 1, "character": 2 }),
    )
}

/// tsrs occasionally never answers a request: it loops on it for good. The
/// server answers it without tsgo once it is overdue (the empty answer here:
/// the request has no native half) and restarts tsgo, which replays the
/// buffers it holds; and the background, which waits for tsgo to answer the
/// editor's requests, stops waiting for one after a couple of seconds, so it
/// still loads the other projects.
#[cfg(unix)]
#[test]
fn a_request_tsgo_never_answers_is_answered_without_it_and_holds_nothing_back() {
    let dir = TestDir::new("lost-tsgo-request");
    let helper = traffic_recorder(&dir.0);
    let log = dir.0.join("tsgo.log");
    let root = three_projects(&dir.0);
    let mut server = Server::start(
        Some(&root),
        &[
            ("TSGO_BIN", helper.as_path()),
            ("RSVELTE_TEST_TSGO_LOG", log.as_path()),
            (
                "RSVELTE_TEST_TSGO_IGNORE",
                Path::new("textDocument/signatureHelp"),
            ),
            ("RSVELTE_TSGO_REQUEST_TIMEOUT_MS", Path::new("6000")),
        ],
    );
    let x = file_uri(&root.join("a/X.svelte"));
    server.open(&x, &fs::read_to_string(root.join("a/X.svelte")).unwrap());
    hover(&mut server, &x);
    let (lost_path, lost_text, at) = looping_buffer(&root.join("a"));
    let lost_uri = file_uri(&lost_path);
    server.open_as(&lost_uri, "typescript", lost_text);
    hover(&mut server, &lost_uri);
    let sent = Instant::now();
    let lost = server.request(
        "textDocument/signatureHelp",
        json!({ "textDocument": { "uri": lost_uri }, "position": at }),
    );
    let kept = |project: &str, file: &str| {
        file_uri(
            &root
                .join(".svelte-language-server-rs/projects")
                .join(project)
                .join("tsgo/svelte")
                .join(project)
                .join(format!("{file}.svelte.tsx")),
        )
    };
    let deadline = sent + Duration::from_secs(5);
    loop {
        let lines = traffic(&log);
        if lines.contains(&format!("textDocument/didOpen {}", kept("b", "Y")))
            && lines.contains(&format!("textDocument/didOpen {}", kept("c", "Z")))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the lost request held the background: {lines:?}"
        );
        thread::sleep(Duration::from_millis(100));
    }
    let answer = server.response(lost, Duration::from_secs(20));
    assert!(
        sent.elapsed() >= Duration::from_secs(6),
        "answered before it was overdue"
    );
    assert_eq!(answer["result"], Value::Null, "{answer}");
    let opened = format!("textDocument/didOpen {}", kept("a", "X"));
    let deadline = Instant::now() + Duration::from_secs(10);
    while traffic(&log).iter().filter(|line| **line == opened).count() < 2 {
        assert!(
            Instant::now() < deadline,
            "tsgo was not restarted: {:?}",
            traffic(&log)
        );
        thread::sleep(Duration::from_millis(100));
    }
    assert!(hover(&mut server, &x).to_string().contains("recorded"));
    server.shutdown();
}

/// One project at `<dir>/ws` holding `X.svelte`; returns the workspace root.
#[cfg(unix)]
fn one_project(dir: &Path) -> PathBuf {
    let root = rsvelte_language_server::uri::canonicalize(dir)
        .unwrap()
        .join("ws");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("package.json"), "{}").unwrap();
    fs::write(root.join("tsconfig.json"), "{}").unwrap();
    fs::write(
        root.join("X.svelte"),
        "<script lang=\"ts\">\nlet answer = 42;\n</script>\n<p>{answer}</p>\n",
    )
    .unwrap();
    root
}

/// A diagnostic pull tsgo takes longer than the request timeout to answer
/// is a slow check, not a lost request (review 4: a component reading a
/// module whose check takes 41 s). The editor gets tsgo's answer when it
/// comes, and tsgo is neither cancelled nor restarted, which would throw
/// the check away and leave the document without its TypeScript
/// diagnostics; other requests are answered meanwhile.
#[cfg(unix)]
#[test]
fn a_slow_diagnostic_pull_waits_for_tsgo_and_does_not_restart_it() {
    let dir = TestDir::new("slow-diagnostic-pull");
    let helper = traffic_recorder(&dir.0);
    let log = dir.0.join("tsgo.log");
    let root = one_project(&dir.0);
    let mut server = Server::start_with_capabilities(
        Some(&root),
        &[
            ("TSGO_BIN", helper.as_path()),
            ("RSVELTE_TEST_TSGO_LOG", log.as_path()),
            ("RSVELTE_TEST_SLOW_DIAGNOSTIC_MS", Path::new("4000")),
            ("RSVELTE_TSGO_REQUEST_TIMEOUT_MS", Path::new("1000")),
        ],
        json!({ "backgroundProjects": { "enable": false } }),
        &json!({
            "textDocument": { "diagnostic": {} },
            "workspace": { "diagnostics": { "refreshSupport": true } },
        }),
    );
    let x = file_uri(&root.join("X.svelte"));
    let shadow = file_uri(&root.join(".svelte-language-server-rs/tsgo/svelte/X.svelte.tsx"));
    server.open(&x, &fs::read_to_string(root.join("X.svelte")).unwrap());
    hover(&mut server, &x);
    for round in 0..2 {
        let sent = Instant::now();
        let pull = server.request(
            "textDocument/diagnostic",
            json!({ "textDocument": { "uri": x } }),
        );
        assert!(hover(&mut server, &x).to_string().contains("recorded"));
        let report = server.response(pull, Duration::from_secs(20));
        assert_eq!(report["result"]["kind"], "full", "{report}");
        assert!(
            sent.elapsed() >= Duration::from_millis(3_500),
            "pull {round} was answered without tsgo after {:?}",
            sent.elapsed()
        );
        if round == 0 {
            server.change(
                &x,
                2,
                "<script lang=\"ts\">\nlet answer = 43;\n</script>\n<p>{answer}</p>\n",
            );
        }
    }
    let lines = traffic(&log);
    assert_eq!(
        lines
            .iter()
            .filter(|line| **line == format!("textDocument/didOpen {shadow}"))
            .count(),
        1,
        "tsgo was restarted: {lines:?}"
    );
    assert!(
        !lines.iter().any(|line| line.starts_with("$/cancelRequest")),
        "a pull was cancelled: {lines:?}"
    );
    server.shutdown();
}

/// A signature help or a definition tsgo answers only after the request
/// timeout, while it answers hovers at once, is slow, not lost (review 5:
/// tsrs answers them once a check the document's pull started is done, 52 s
/// later). Each is answered without tsgo at the timeout and cancelled, but
/// tsgo is not restarted, which would throw away the check and leave the
/// pull, answered meanwhile, without its TypeScript half.
#[cfg(unix)]
#[test]
fn a_slow_signature_help_or_definition_does_not_restart_tsgo() {
    let dir = TestDir::new("slow-not-lost");
    let helper = traffic_recorder(&dir.0);
    let log = dir.0.join("tsgo.log");
    let root = one_project(&dir.0);
    let mut server = Server::start_with_capabilities(
        Some(&root),
        &[
            ("TSGO_BIN", helper.as_path()),
            ("RSVELTE_TEST_TSGO_LOG", log.as_path()),
            (
                "RSVELTE_TEST_SLOW_METHODS",
                Path::new("textDocument/signatureHelp,textDocument/definition"),
            ),
            ("RSVELTE_TEST_SLOW_MS", Path::new("2000")),
            ("RSVELTE_TEST_SLOW_DIAGNOSTIC_MS", Path::new("2500")),
            ("RSVELTE_TEST_DIAGNOSTIC_WORD", Path::new("answer")),
            ("RSVELTE_TSGO_REQUEST_TIMEOUT_MS", Path::new("1000")),
        ],
        json!({ "backgroundProjects": { "enable": false } }),
        &json!({ "textDocument": { "diagnostic": {} } }),
    );
    let x = file_uri(&root.join("X.svelte"));
    let shadow = file_uri(&root.join(".svelte-language-server-rs/tsgo/svelte/X.svelte.tsx"));
    server.open(&x, &fs::read_to_string(root.join("X.svelte")).unwrap());
    hover(&mut server, &x);
    let sent = Instant::now();
    let pull = server.request(
        "textDocument/diagnostic",
        json!({ "textDocument": { "uri": x } }),
    );
    let slow = ["textDocument/signatureHelp", "textDocument/definition"].map(|method| {
        server.request(
            method,
            json!({ "textDocument": { "uri": x }, "position": { "line": 1, "character": 5 } }),
        )
    });
    while sent.elapsed() < Duration::from_millis(3_000) {
        let asked = Instant::now();
        assert!(hover(&mut server, &x).to_string().contains("recorded"));
        assert!(
            asked.elapsed() < Duration::from_millis(500),
            "a hover waited {:?}",
            asked.elapsed()
        );
        thread::sleep(Duration::from_millis(100));
    }
    for id in slow {
        let answer = server.response(id, Duration::from_secs(10));
        assert!(answer.get("result").is_some(), "{answer}");
    }
    let report = server.response(pull, Duration::from_secs(10));
    assert_eq!(
        report["result"]["items"][0]["code"], 2322,
        "the pull lost its TypeScript half: {report}"
    );
    let lines = traffic(&log);
    assert_eq!(
        lines
            .iter()
            .filter(|line| **line == format!("textDocument/didOpen {shadow}"))
            .count(),
        1,
        "tsgo was restarted: {lines:?}"
    );
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.starts_with("$/cancelRequest"))
            .count(),
        2,
        "the two overdue requests, and nothing else, are cancelled: {lines:?}"
    );
    server.shutdown();
}

/// tsrs answers a request about a document whose check is running only once
/// the check is done (review 5), so a request it would loop on does not
/// restart tsgo while a pull of its document is with tsgo: the pull keeps
/// its TypeScript half, and tsgo restarts once the pull is answered.
#[cfg(unix)]
#[test]
fn a_looping_request_restarts_tsgo_only_after_a_pull_of_its_document() {
    let dir = TestDir::new("loop-during-pull");
    let helper = traffic_recorder(&dir.0);
    let log = dir.0.join("tsgo.log");
    let root = one_project(&dir.0);
    let mut server = Server::start_with_capabilities(
        Some(&root),
        &[
            ("TSGO_BIN", helper.as_path()),
            ("RSVELTE_TEST_TSGO_LOG", log.as_path()),
            (
                "RSVELTE_TEST_TSGO_IGNORE",
                Path::new("textDocument/signatureHelp"),
            ),
            ("RSVELTE_TEST_SLOW_DIAGNOSTIC_MS", Path::new("3000")),
            ("RSVELTE_TEST_DIAGNOSTIC_WORD", Path::new("q")),
            ("RSVELTE_TSGO_REQUEST_TIMEOUT_MS", Path::new("1000")),
        ],
        json!({ "backgroundProjects": { "enable": false } }),
        &json!({ "textDocument": { "diagnostic": {} } }),
    );
    let x = file_uri(&root.join("X.svelte"));
    let shadow = file_uri(&root.join(".svelte-language-server-rs/tsgo/svelte/X.svelte.tsx"));
    server.open(&x, &fs::read_to_string(root.join("X.svelte")).unwrap());
    hover(&mut server, &x);
    let (lost_path, lost_text, at) = looping_buffer(&root);
    let lost_uri = file_uri(&lost_path);
    server.open_as(&lost_uri, "typescript", lost_text);
    hover(&mut server, &lost_uri);
    let opened = |log: &Path| {
        traffic(log)
            .iter()
            .filter(|line| **line == format!("textDocument/didOpen {shadow}"))
            .count()
    };
    let sent = Instant::now();
    let pull = server.request(
        "textDocument/diagnostic",
        json!({ "textDocument": { "uri": lost_uri } }),
    );
    let lost = server.request(
        "textDocument/signatureHelp",
        json!({ "textDocument": { "uri": lost_uri }, "position": at }),
    );
    assert_eq!(
        server.response(lost, Duration::from_secs(10))["result"],
        Value::Null
    );
    // Past the timeout: without the pull, tsgo would have restarted by now.
    thread::sleep(Duration::from_millis(2_000).saturating_sub(sent.elapsed()));
    server.probe();
    assert_eq!(opened(&log), 1, "restarted while the pull was with tsgo");
    let report = server.response(pull, Duration::from_secs(10));
    assert!(
        sent.elapsed() >= Duration::from_millis(2_500),
        "the pull was answered without tsgo after {:?}",
        sent.elapsed()
    );
    assert_eq!(
        report["result"]["items"][0]["code"], 2322,
        "the pull lost its TypeScript half: {report}"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while opened(&log) < 2 {
        assert!(
            Instant::now() < deadline,
            "tsgo was not restarted once the pull was answered: {:?}",
            traffic(&log)
        );
        server.probe();
        thread::sleep(Duration::from_millis(100));
    }
    server.shutdown();
}

/// tsrs never answers a request it loops on, and each one holds one of its
/// few workers for good (review 5, loopstorm.mjs: with a signature help
/// every 2 s at such a state, tsrs answered nothing else after three). While
/// one such request about a document is with tsgo, the editor's others it
/// would loop on there are answered at once without tsgo; other requests
/// still reach tsgo; and once tsgo restarts, the next one is sent again.
#[cfg(unix)]
#[test]
fn a_second_request_tsgo_would_loop_on_is_answered_without_it() {
    let dir = TestDir::new("loop-once");
    let helper = traffic_recorder(&dir.0);
    let log = dir.0.join("tsgo.log");
    let root = one_project(&dir.0);
    let mut server = Server::start_with_settings(
        Some(&root),
        &[
            ("TSGO_BIN", helper.as_path()),
            ("RSVELTE_TEST_TSGO_LOG", log.as_path()),
            (
                "RSVELTE_TEST_TSGO_IGNORE",
                Path::new("textDocument/signatureHelp"),
            ),
            ("RSVELTE_TSGO_REQUEST_TIMEOUT_MS", Path::new("2000")),
        ],
        json!({ "backgroundProjects": { "enable": false } }),
    );
    let x = file_uri(&root.join("X.svelte"));
    let shadow = file_uri(&root.join(".svelte-language-server-rs/tsgo/svelte/X.svelte.tsx"));
    server.open(&x, &fs::read_to_string(root.join("X.svelte")).unwrap());
    hover(&mut server, &x);
    let (lost_path, lost_text, at) = looping_buffer(&root);
    let lost_uri = file_uri(&lost_path);
    server.open_as(&lost_uri, "typescript", lost_text);
    hover(&mut server, &lost_uri);
    let signature_help = |server: &mut Server| {
        server.request(
            "textDocument/signatureHelp",
            json!({ "textDocument": { "uri": lost_uri }, "position": at }),
        )
    };
    let sent_to_tsgo = |log: &Path| {
        traffic(log)
            .iter()
            .filter(|line| line.starts_with("textDocument/signatureHelp "))
            .count()
    };
    let opened = |log: &Path| {
        traffic(log)
            .iter()
            .filter(|line| **line == format!("textDocument/didOpen {shadow}"))
            .count()
    };
    let first = signature_help(&mut server);
    server.probe();
    for _ in 0..3 {
        let asked = Instant::now();
        let again = signature_help(&mut server);
        assert_eq!(
            server.response(again, Duration::from_secs(10))["result"],
            Value::Null
        );
        assert!(
            asked.elapsed() < Duration::from_millis(1_000),
            "a request tsgo would loop on again waited {:?}",
            asked.elapsed()
        );
    }
    assert!(
        hover(&mut server, &lost_uri)
            .to_string()
            .contains("recorded")
    );
    assert_eq!(sent_to_tsgo(&log), 1, "{:?}", traffic(&log));
    assert_eq!(
        server.response(first, Duration::from_secs(10))["result"],
        Value::Null
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while opened(&log) < 2 {
        assert!(
            Instant::now() < deadline,
            "tsgo was not restarted: {:?}",
            traffic(&log)
        );
        server.probe();
        thread::sleep(Duration::from_millis(100));
    }
    let after_restart = signature_help(&mut server);
    server.probe();
    let deadline = Instant::now() + Duration::from_secs(5);
    while sent_to_tsgo(&log) < 2 {
        assert!(
            Instant::now() < deadline,
            "not sent after the restart: {:?}",
            traffic(&log)
        );
        thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        server.response(after_restart, Duration::from_secs(10))["result"],
        Value::Null
    );
    server.shutdown();
}

/// How many times tsgo was started for `shadow`: each start opens it again.
#[cfg(unix)]
fn shadow_opens(log: &Path, shadow: &str) -> usize {
    traffic(log)
        .iter()
        .filter(|line| **line == format!("textDocument/didOpen {shadow}"))
        .count()
}

/// Wait until tsgo has been started `count` times for `shadow`.
#[cfg(unix)]
fn wait_for_shadow_opens(
    server: &mut Server,
    log: &Path,
    shadow: &str,
    count: usize,
    within: Duration,
) {
    let deadline = Instant::now() + within;
    while shadow_opens(log, shadow) < count {
        assert!(
            Instant::now() < deadline,
            "tsgo was not restarted: {:?}",
            traffic(log)
        );
        server.probe();
        thread::sleep(Duration::from_millis(100));
    }
}

/// A completion at a position the model does not say tsrs loops on
/// (`X.svelte`'s script has no `<`), in the editor's document.
#[cfg(unix)]
fn completion(server: &mut Server, uri: &str) -> i64 {
    server.request(
        "textDocument/completion",
        json!({ "textDocument": { "uri": uri }, "position": { "line": 1, "character": 5 } }),
    )
}

/// A tsrs loop the model misses (review 6: a regular expression on the
/// walk's line) takes a worker for good, and with every worker looping tsrs
/// answers nothing at all. tsgo is restarted once it has answered nothing
/// for a timeout since a request it was sent, whatever the model says (the
/// fake tsgo answers nothing after a completion the model clears), and
/// answers again.
#[cfg(unix)]
#[test]
fn a_tsgo_that_answers_nothing_is_restarted_whatever_the_model_says() {
    let dir = TestDir::new("wedged-tsgo");
    let helper = traffic_recorder(&dir.0);
    let log = dir.0.join("tsgo.log");
    let root = one_project(&dir.0);
    let mut server = Server::start_with_settings(
        Some(&root),
        &[
            ("TSGO_BIN", helper.as_path()),
            ("RSVELTE_TEST_TSGO_LOG", log.as_path()),
            (
                "RSVELTE_TEST_TSGO_WEDGE",
                Path::new("textDocument/completion"),
            ),
            ("RSVELTE_TSGO_REQUEST_TIMEOUT_MS", Path::new("1000")),
        ],
        json!({ "backgroundProjects": { "enable": false } }),
    );
    let x = file_uri(&root.join("X.svelte"));
    let shadow = file_uri(&root.join(".svelte-language-server-rs/tsgo/svelte/X.svelte.tsx"));
    server.open(&x, &fs::read_to_string(root.join("X.svelte")).unwrap());
    assert!(hover(&mut server, &x).to_string().contains("recorded"));
    let sent = Instant::now();
    let lost = completion(&mut server, &x);
    let unanswered = server.request(
        "textDocument/hover",
        json!({ "textDocument": { "uri": x }, "position": { "line": 1, "character": 5 } }),
    );
    server.response(lost, Duration::from_secs(10));
    server.response(unanswered, Duration::from_secs(10));
    wait_for_shadow_opens(&mut server, &log, &shadow, 2, Duration::from_secs(10));
    assert!(
        sent.elapsed() >= Duration::from_millis(900),
        "restarted before a timeout passed: {:?}",
        sent.elapsed()
    );
    assert!(hover(&mut server, &x).to_string().contains("recorded"));
    server.shutdown();
}

/// tsrs answers a slow completion or signature help once it is done, even
/// cancelled, and never one it loops on, which keeps a core busy for good.
/// One the model does not say it loops on is answered without tsgo at the
/// timeout and kept: while it is overdue, the editor's next completions in
/// that document are answered without tsgo rather than sent (each would
/// take another worker), and if tsgo has still not answered it after four
/// timeouts, tsgo is restarted, though tsgo answers everything else.
#[cfg(unix)]
#[test]
fn a_completion_tsgo_never_answers_restarts_it_after_four_timeouts() {
    let dir = TestDir::new("suspect-completion");
    let helper = traffic_recorder(&dir.0);
    let log = dir.0.join("tsgo.log");
    let root = one_project(&dir.0);
    let mut server = Server::start_with_settings(
        Some(&root),
        &[
            ("TSGO_BIN", helper.as_path()),
            ("RSVELTE_TEST_TSGO_LOG", log.as_path()),
            (
                "RSVELTE_TEST_TSGO_IGNORE",
                Path::new("textDocument/completion"),
            ),
            ("RSVELTE_TSGO_REQUEST_TIMEOUT_MS", Path::new("1000")),
        ],
        json!({ "backgroundProjects": { "enable": false } }),
    );
    let x = file_uri(&root.join("X.svelte"));
    let shadow = file_uri(&root.join(".svelte-language-server-rs/tsgo/svelte/X.svelte.tsx"));
    server.open(&x, &fs::read_to_string(root.join("X.svelte")).unwrap());
    assert!(hover(&mut server, &x).to_string().contains("recorded"));
    let sent_to_tsgo = |log: &Path| {
        traffic(log)
            .iter()
            .filter(|line| line.starts_with("textDocument/completion "))
            .count()
    };
    let sent = Instant::now();
    let lost = completion(&mut server, &x);
    // tsgo answers what it is asked after it.
    assert!(hover(&mut server, &x).to_string().contains("recorded"));
    server.response(lost, Duration::from_secs(10));
    let asked = Instant::now();
    let again = completion(&mut server, &x);
    server.response(again, Duration::from_secs(10));
    assert!(
        asked.elapsed() < Duration::from_millis(500),
        "a completion waited {:?} behind one tsgo has not answered",
        asked.elapsed()
    );
    assert_eq!(sent_to_tsgo(&log), 1, "{:?}", traffic(&log));
    while sent.elapsed() < Duration::from_millis(3_500) {
        assert!(hover(&mut server, &x).to_string().contains("recorded"));
        thread::sleep(Duration::from_millis(200));
    }
    assert_eq!(
        shadow_opens(&log, &shadow),
        1,
        "restarted within four timeouts, while tsgo answered hovers"
    );
    wait_for_shadow_opens(&mut server, &log, &shadow, 2, Duration::from_secs(10));
    let after_restart = completion(&mut server, &x);
    server.response(after_restart, Duration::from_secs(10));
    assert_eq!(sent_to_tsgo(&log), 2, "not sent after the restart");
    server.shutdown();
}

/// Each request tsrs loops on holds one of the three checkers a project has
/// for them, so with three overdue at once tsrs answers nothing else about
/// the project. Three overdue requests restart tsgo at their timeout, whatever
/// the model says and though tsgo still answers other requests.
#[cfg(unix)]
#[test]
fn as_many_overdue_requests_as_tsrs_runs_restart_tsgo() {
    let dir = TestDir::new("saturated-tsgo");
    let helper = traffic_recorder(&dir.0);
    let log = dir.0.join("tsgo.log");
    let root = one_project(&dir.0);
    let mut server = Server::start_with_settings(
        Some(&root),
        &[
            ("TSGO_BIN", helper.as_path()),
            ("RSVELTE_TEST_TSGO_LOG", log.as_path()),
            (
                "RSVELTE_TEST_TSGO_IGNORE",
                Path::new("textDocument/completion"),
            ),
            ("RSVELTE_TSGO_REQUEST_TIMEOUT_MS", Path::new("1000")),
        ],
        json!({ "backgroundProjects": { "enable": false } }),
    );
    let x = file_uri(&root.join("X.svelte"));
    let shadow = file_uri(&root.join(".svelte-language-server-rs/tsgo/svelte/X.svelte.tsx"));
    server.open(&x, &fs::read_to_string(root.join("X.svelte")).unwrap());
    assert!(hover(&mut server, &x).to_string().contains("recorded"));
    let sent = Instant::now();
    let lost = [0, 1, 2].map(|_| completion(&mut server, &x));
    while sent.elapsed() < Duration::from_millis(800) {
        assert!(hover(&mut server, &x).to_string().contains("recorded"));
        thread::sleep(Duration::from_millis(100));
    }
    for id in lost {
        server.response(id, Duration::from_secs(10));
    }
    wait_for_shadow_opens(&mut server, &log, &shadow, 2, Duration::from_secs(10));
    assert!(
        sent.elapsed() < Duration::from_millis(3_500),
        "restarted only after {:?}",
        sent.elapsed()
    );
    server.shutdown();
}

/// Requests a check holds back are slow, not lost, however many there are,
/// however long the check takes and whether or not the editor asks anything
/// else meanwhile (review 5's slowmix.mjs: five; tsrs answers them just after
/// the pull; on a busy machine its 7 s check took over 12 s, four of
/// runall.sh's timeouts). Three overdue requests about a document whose pull
/// is with tsgo for five timeouts, answered just after it, do not restart
/// tsgo.
#[cfg(unix)]
#[test]
fn requests_a_check_holds_back_do_not_restart_tsgo() {
    let dir = TestDir::new("held-back-by-check");
    let helper = traffic_recorder(&dir.0);
    let log = dir.0.join("tsgo.log");
    let root = one_project(&dir.0);
    let mut server = Server::start_with_capabilities(
        Some(&root),
        &[
            ("TSGO_BIN", helper.as_path()),
            ("RSVELTE_TEST_TSGO_LOG", log.as_path()),
            (
                "RSVELTE_TEST_SLOW_METHODS",
                Path::new(
                    "textDocument/signatureHelp,textDocument/definition,textDocument/completion",
                ),
            ),
            ("RSVELTE_TEST_SLOW_MS", Path::new("5200")),
            ("RSVELTE_TEST_SLOW_DIAGNOSTIC_MS", Path::new("5000")),
            ("RSVELTE_TEST_DIAGNOSTIC_WORD", Path::new("answer")),
            ("RSVELTE_TSGO_REQUEST_TIMEOUT_MS", Path::new("1000")),
        ],
        json!({ "backgroundProjects": { "enable": false } }),
        &json!({ "textDocument": { "diagnostic": {} } }),
    );
    let x = file_uri(&root.join("X.svelte"));
    let shadow = file_uri(&root.join(".svelte-language-server-rs/tsgo/svelte/X.svelte.tsx"));
    server.open(&x, &fs::read_to_string(root.join("X.svelte")).unwrap());
    assert!(hover(&mut server, &x).to_string().contains("recorded"));
    let pull = server.request(
        "textDocument/diagnostic",
        json!({ "textDocument": { "uri": x } }),
    );
    let slow = [
        "textDocument/signatureHelp",
        "textDocument/definition",
        "textDocument/completion",
    ]
    .map(|method| {
        server.request(
            method,
            json!({ "textDocument": { "uri": x }, "position": { "line": 1, "character": 5 } }),
        )
    });
    for id in slow {
        let answer = server.response(id, Duration::from_secs(10));
        assert!(answer.get("result").is_some(), "{answer}");
    }
    let report = server.response(pull, Duration::from_secs(10));
    assert_eq!(
        report["result"]["items"][0]["code"], 2322,
        "the pull lost its TypeScript half: {report}"
    );
    // Past the slow requests' answers and the timeout after the pull.
    thread::sleep(Duration::from_millis(1_500));
    server.probe();
    assert!(hover(&mut server, &x).to_string().contains("recorded"));
    assert_eq!(
        shadow_opens(&log, &shadow),
        1,
        "tsgo was restarted: {:?}",
        traffic(&log)
    );
    server.shutdown();
}

/// tsgo sees a store and its `$store` value as two symbols, the second
/// declared in generated code (`let $count = __sveltets_2_store_get(count);`),
/// but a store's references are its value's too, and the other way round, as
/// upstream lists them (review 6: from `const count` only the declaration was
/// listed, from a `$count` use only the uses). The fake tsgo answers each
/// symbol's references with the occurrences of its word.
#[cfg(unix)]
#[test]
fn the_references_of_a_store_and_of_its_value_are_listed_together() {
    let dir = TestDir::new("store-references");
    let helper = traffic_recorder(&dir.0);
    let log = dir.0.join("tsgo.log");
    let root = one_project(&dir.0);
    let source = "<script lang=\"ts\">\n\timport { writable } from \"svelte/store\";\n\tconst count = writable(0);\n\tfunction bump() { $count++; }\n</script>\n<p>{$count}</p>\n";
    fs::write(root.join("X.svelte"), source).unwrap();
    let mut server = Server::start_with_settings(
        Some(&root),
        &[
            ("TSGO_BIN", helper.as_path()),
            ("RSVELTE_TEST_TSGO_LOG", log.as_path()),
            ("RSVELTE_TEST_TEXT_REFERENCES", Path::new("1")),
        ],
        json!({ "backgroundProjects": { "enable": false } }),
    );
    let x = file_uri(&root.join("X.svelte"));
    server.open(&x, source);
    hover(&mut server, &x);
    let references = |server: &mut Server, line: u32, character: u32| {
        let id = server.request(
            "textDocument/references",
            json!({
                "textDocument": { "uri": x },
                "position": { "line": line, "character": character },
                "context": { "includeDeclaration": true },
            }),
        );
        let answer = server.response(id, Duration::from_secs(10));
        let mut ranges = answer["result"]
            .as_array()
            .unwrap_or_else(|| panic!("{answer}"))
            .iter()
            .map(|location| {
                let range = &location["range"];
                format!(
                    "{}:{}-{}:{}",
                    range["start"]["line"],
                    range["start"]["character"],
                    range["end"]["line"],
                    range["end"]["character"]
                )
            })
            .collect::<Vec<_>>();
        ranges.sort();
        ranges
    };
    let expected = ["2:7-2:12", "3:19-3:25", "5:4-5:10"];
    assert_eq!(references(&mut server, 2, 8), expected, "from the store");
    assert_eq!(
        references(&mut server, 5, 6),
        expected,
        "from a use of its value"
    );
    server.shutdown();
}

/// After tsgo restarts, the pulls it was asked for before were answered
/// without it, so the editor is asked to pull again (review 4).
#[cfg(unix)]
#[test]
fn a_restarted_tsgo_has_the_editor_pull_its_diagnostics_again() {
    let dir = TestDir::new("restart-refresh");
    let helper = traffic_recorder(&dir.0);
    let log = dir.0.join("tsgo.log");
    let root = one_project(&dir.0);
    let mut server = Server::start_with_capabilities(
        Some(&root),
        &[
            ("TSGO_BIN", helper.as_path()),
            ("RSVELTE_TEST_TSGO_LOG", log.as_path()),
            (
                "RSVELTE_TEST_TSGO_IGNORE",
                Path::new("textDocument/signatureHelp"),
            ),
            ("RSVELTE_TSGO_REQUEST_TIMEOUT_MS", Path::new("1000")),
        ],
        json!({ "backgroundProjects": { "enable": false } }),
        &json!({
            "textDocument": { "diagnostic": {} },
            "workspace": { "diagnostics": { "refreshSupport": true } },
        }),
    );
    let x = file_uri(&root.join("X.svelte"));
    let shadow = file_uri(&root.join(".svelte-language-server-rs/tsgo/svelte/X.svelte.tsx"));
    server.open(&x, &fs::read_to_string(root.join("X.svelte")).unwrap());
    hover(&mut server, &x);
    let (lost_path, lost_text, at) = looping_buffer(&root);
    let lost_uri = file_uri(&lost_path);
    server.open_as(&lost_uri, "typescript", lost_text);
    hover(&mut server, &lost_uri);
    let refreshes = |server: &Server| {
        server
            .server_requests
            .iter()
            .filter(|method| *method == "workspace/diagnostic/refresh")
            .count()
    };
    let before = refreshes(&server);
    let lost = server.request(
        "textDocument/signatureHelp",
        json!({ "textDocument": { "uri": lost_uri }, "position": at }),
    );
    assert_eq!(
        server.response(lost, Duration::from_secs(10))["result"],
        Value::Null
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let restarted = traffic(&log)
            .iter()
            .filter(|line| **line == format!("textDocument/didOpen {shadow}"))
            .count()
            == 2;
        if restarted && refreshes(&server) > before {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "no refresh after the restart (restarted: {restarted}): {:?}",
            server.server_requests
        );
        server.probe();
        thread::sleep(Duration::from_millis(100));
    }
    server.shutdown();
}

/// tsgo's answers wait in a channel while the server's main loop is busy.
/// One already received is not lost however long the loop was busy: the
/// server answers the editor with it, and does not restart a tsgo that
/// answered everything (review 4: mapping a burst of folding ranges kept
/// the loop busy for more than the request timeout).
#[cfg(unix)]
#[test]
fn answers_tsgo_sent_while_the_server_was_busy_are_not_counted_as_lost() {
    let dir = TestDir::new("busy-loop-answers");
    let helper = traffic_recorder(&dir.0);
    let log = dir.0.join("tsgo.log");
    let root = one_project(&dir.0);
    // A client that pulls diagnostics, so that opening the large component
    // below does not also start an analysis of it.
    let mut server = Server::start_with_capabilities(
        Some(&root),
        &[
            ("TSGO_BIN", helper.as_path()),
            ("RSVELTE_TEST_TSGO_LOG", log.as_path()),
            ("RSVELTE_TEST_HOVER_DELAY_MS", Path::new("200")),
            ("RSVELTE_TSGO_REQUEST_TIMEOUT_MS", Path::new("1000")),
        ],
        json!({ "backgroundProjects": { "enable": false } }),
        &json!({ "textDocument": { "diagnostic": {} } }),
    );
    let x = file_uri(&root.join("X.svelte"));
    let shadow = file_uri(&root.join(".svelte-language-server-rs/tsgo/svelte/X.svelte.tsx"));
    server.open(&x, &fs::read_to_string(root.join("X.svelte")).unwrap());
    hover(&mut server, &x);
    // A component large enough that projecting it when it opens keeps the
    // main loop busy past the timeout; tsgo answers the hovers meanwhile.
    let mut big = "<script lang=\"ts\">\n".to_string();
    let mut template = String::new();
    for index in 0..20_000 {
        writeln!(big, "\tlet v{index} = $state({index});").unwrap();
        writeln!(
            template,
            "<div class=\"r{index}\" title={{String(v{index})}}>{{v{index} * 2}}</div>"
        )
        .unwrap();
    }
    big.push_str("</script>\n");
    big.push_str(&template);
    let sent = Instant::now();
    let hovers = (0..10)
        .map(|_| {
            server.request(
                "textDocument/hover",
                json!({ "textDocument": { "uri": x }, "position": { "line": 1, "character": 5 } }),
            )
        })
        .collect::<Vec<_>>();
    server.open(&file_uri(&root.join("Big.svelte")), &big);
    for id in hovers {
        let answer = server.response(id, Duration::from_secs(60));
        assert!(answer.to_string().contains("recorded"), "{answer}");
    }
    assert!(
        sent.elapsed() >= Duration::from_millis(1_000),
        "the main loop was not busy past the timeout ({:?}): make the component larger",
        sent.elapsed()
    );
    let lines = traffic(&log);
    assert_eq!(
        lines
            .iter()
            .filter(|line| **line == format!("textDocument/didOpen {shadow}"))
            .count(),
        1,
        "tsgo was restarted: {lines:?}"
    );
    server.shutdown();
}

/// A workspace with three projects that own components and one that owns
/// none, for the background-loading tests.
#[cfg(unix)]
fn three_projects(dir: &Path) -> PathBuf {
    let root = rsvelte_language_server::uri::canonicalize(dir)
        .unwrap()
        .join("ws");
    for project in ["a", "b", "c", "tests"] {
        fs::create_dir_all(root.join(project)).unwrap();
        fs::write(root.join(project).join("tsconfig.json"), "{}").unwrap();
    }
    fs::write(root.join("package.json"), "{}").unwrap();
    let component = "<script lang=\"ts\">\nlet answer = 42;\n</script>\n<p>{answer}</p>\n";
    fs::write(root.join("a/X.svelte"), component).unwrap();
    fs::write(root.join("b/Y.svelte"), component).unwrap();
    fs::write(root.join("c/Z.svelte"), component).unwrap();
    fs::write(root.join("tests/only.test.ts"), "export {};\n").unwrap();
    root
}

/// Once the editor's first project is served and the editor is quiet, the
/// other projects that own a component load in the background, and tsgo
/// gets one buffer for each, which keeps the project, and a request that
/// has it built now rather than at the editor's next request.
#[cfg(unix)]
#[test]
fn the_other_projects_load_in_the_background_once_the_editor_is_quiet() {
    let dir = TestDir::new("background-projects");
    let helper = traffic_recorder(&dir.0);
    let log = dir.0.join("tsgo.log");
    let root = three_projects(&dir.0);
    let mut server = Server::start(
        Some(&root),
        &[
            ("TSGO_BIN", helper.as_path()),
            ("RSVELTE_TEST_TSGO_LOG", log.as_path()),
        ],
    );
    let x = file_uri(&root.join("a/X.svelte"));
    server.open(&x, &fs::read_to_string(root.join("a/X.svelte")).unwrap());
    hover(&mut server, &x);
    let kept = |project: &str, file: &str| {
        file_uri(
            &root
                .join(".svelte-language-server-rs/projects")
                .join(project)
                .join("tsgo/svelte")
                .join(project)
                .join(format!("{file}.svelte.tsx")),
        )
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let lines = traffic(&log);
        let warmed = |uri: &str| {
            lines.contains(&format!("textDocument/didOpen {uri}"))
                && lines.contains(&format!("textDocument/documentSymbol {uri}"))
        };
        if warmed(&kept("b", "Y")) && warmed(&kept("c", "Z")) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the other projects never loaded: {lines:?}"
        );
        thread::sleep(Duration::from_millis(200));
    }
    assert!(
        !root
            .join(".svelte-language-server-rs/projects/tests")
            .exists(),
        "a project that owns no component waits for a document of its own"
    );
    // The editor's own document still answers, and nothing else is open.
    assert!(hover(&mut server, &x).to_string().contains("recorded"));
    let opened = traffic(&log)
        .into_iter()
        .filter(|line| line.starts_with("textDocument/didOpen "))
        .count();
    assert_eq!(opened, 3, "the document and one buffer per other project");
    server.shutdown();
}

/// The editor's answers to the server's own requests (the watcher
/// registrations tsgo makes as each background project builds) are not
/// editor activity: they must not keep postponing the background.
#[cfg(unix)]
#[test]
fn the_editors_answers_do_not_hold_the_background_back() {
    let dir = TestDir::new("background-answers");
    let helper = traffic_recorder(&dir.0);
    let log = dir.0.join("tsgo.log");
    let root = three_projects(&dir.0);
    let mut server = Server::start(
        Some(&root),
        &[
            ("TSGO_BIN", helper.as_path()),
            ("RSVELTE_TEST_TSGO_LOG", log.as_path()),
        ],
    );
    let x = file_uri(&root.join("a/X.svelte"));
    server.open(&x, &fs::read_to_string(root.join("a/X.svelte")).unwrap());
    hover(&mut server, &x);
    let kept = |project: &str, file: &str| {
        file_uri(
            &root
                .join(".svelte-language-server-rs/projects")
                .join(project)
                .join("tsgo/svelte")
                .join(project)
                .join(format!("{file}.svelte.tsx")),
        )
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut answer = 1_000_000;
    loop {
        let lines = traffic(&log);
        if lines.contains(&format!("textDocument/didOpen {}", kept("b", "Y")))
            && lines.contains(&format!("textDocument/didOpen {}", kept("c", "Z")))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "answers every 200 ms kept the other projects from loading: {lines:?}"
        );
        // An answer to a request of the server's, more often than its idle
        // time.
        answer += 1;
        server.write(&json!({ "jsonrpc": "2.0", "id": answer, "result": null }));
        thread::sleep(Duration::from_millis(200));
    }
    server.shutdown();
}

/// With the background off, a workspace-wide request loads every project
/// that owns a component before tsgo answers it.
#[cfg(unix)]
#[test]
fn a_workspace_wide_request_loads_every_project_first() {
    let dir = TestDir::new("on-demand-projects");
    let helper = traffic_recorder(&dir.0);
    let log = dir.0.join("tsgo.log");
    let root = three_projects(&dir.0);
    let mut server = Server::start_with_settings(
        Some(&root),
        &[
            ("TSGO_BIN", helper.as_path()),
            ("RSVELTE_TEST_TSGO_LOG", log.as_path()),
        ],
        json!({ "backgroundProjects": { "enable": false } }),
    );
    let x = file_uri(&root.join("a/X.svelte"));
    server.open(&x, &fs::read_to_string(root.join("a/X.svelte")).unwrap());
    hover(&mut server, &x);
    let id = server.request("workspace/symbol", json!({ "query": "answer" }));
    let response = server.response(id, Duration::from_secs(10));
    assert!(response.get("result").is_some(), "{response}");
    let lines = traffic(&log);
    let symbols = lines
        .iter()
        .position(|line| line.starts_with("workspace/symbol"))
        .expect("forwarded");
    for (project, file) in [("b", "Y"), ("c", "Z")] {
        let kept = file_uri(
            &root
                .join(".svelte-language-server-rs/projects")
                .join(project)
                .join("tsgo/svelte")
                .join(project)
                .join(format!("{file}.svelte.tsx")),
        );
        let opened = lines
            .iter()
            .position(|line| line == &format!("textDocument/didOpen {kept}"))
            .unwrap_or_else(|| panic!("{project} is not open in tsgo: {lines:?}"));
        assert!(opened < symbols, "{project} loads before the request");
    }
    server.shutdown();
}

/// States a component passes through while it is typed (Zed auto-closes `{`),
/// whose projection used to panic on the server's main loop and end the whole
/// server (review round 2). Each must reach tsgo as a shadow of that version,
/// and the server must keep answering.
#[cfg(unix)]
#[test]
fn keystroke_states_that_panicked_the_projection_keep_the_server_answering() {
    let dir = TestDir::new("keystroke-panics");
    let helper = traffic_recorder(&dir.0);
    let log = dir.0.join("tsgo.log");
    let root = rsvelte_language_server::uri::canonicalize(&dir.0)
        .unwrap()
        .join("ws");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("package.json"), "{}").unwrap();
    fs::write(root.join("tsconfig.json"), "{}").unwrap();
    let head = "<script lang=\"ts\">\n\tlet items = [1, 2];\n\tlet p = Promise.resolve(KeystrokeMarker);\n</script>\n\n";
    let clean = "<script lang=\"ts\">\n\tlet items = [1, 2];\n</script>\n\n<p>{items}</p>\n";
    fs::write(root.join("App.svelte"), clean).unwrap();
    let mut server = Server::start(
        Some(&root),
        &[
            ("TSGO_BIN", helper.as_path()),
            ("RSVELTE_TEST_TSGO_LOG", log.as_path()),
            ("RSVELTE_TEST_MARKER", Path::new("KeystrokeMarker")),
        ],
    );
    let uri = file_uri(&root.join("App.svelte"));
    server.open(&uri, clean);
    assert!(hover(&mut server, &uri).to_string().contains("recorded"));

    let states = [
        [head, "{#each items as }\n<p>after</p>\n"].concat(),
        [head, "{#await p}\n\t<p>loading</p>\n{:then }\n\t<p>done</p>\n{/await}\n"].concat(),
        [head, "{#await p}\n\t<p>loading</p>\n{:catch }\n\t<p>done</p>\n{/await}\n"].concat(),
        "<script lang=\"ts\">\n\texport let { a = $bindable(KeystrokeMarker) } = $props();\n</script>\n"
            .to_string(),
    ];
    for (index, state) in states.iter().enumerate() {
        server.change(&uri, 2 + index as i64, state);
        let response = hover(&mut server, &uri);
        assert!(
            response.get("result").is_some(),
            "no hover after state {index}: {response}"
        );
        server.probe();
    }
    let changed = traffic(&log)
        .into_iter()
        .filter(|line| {
            line.starts_with("textDocument/didChange ")
                && line.contains("App.svelte.tsx")
                && line.ends_with(" marked")
        })
        .count();
    assert_eq!(changed, states.len(), "every state reaches tsgo");
    server.shutdown();
}

const FAKE_TSGO_TRAFFIC: &str = r###"
use std::fs::OpenOptions;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn main() {
    let log = PathBuf::from(std::env::var_os("RSVELTE_TEST_TSGO_LOG").unwrap());
    let marker = std::env::var("RSVELTE_TEST_MARKER").unwrap_or_else(|_| "\u{0}".to_string());
    let delay = |name: &str| {
        std::env::var(name).ok().and_then(|ms| ms.parse().ok()).map(Duration::from_millis)
    };
    // `RSVELTE_TEST_SLOW_METHODS` (comma-separated) are answered after
    // `RSVELTE_TEST_SLOW_MS`, while every other request is answered at once.
    let slow_methods = std::env::var("RSVELTE_TEST_SLOW_METHODS").unwrap_or_default();
    // A pull reports one error on the first `RSVELTE_TEST_DIAGNOSTIC_WORD` of
    // the document, as opened.
    let diagnostic_word = std::env::var("RSVELTE_TEST_DIAGNOSTIC_WORD").ok();
    // Once asked an `RSVELTE_TEST_TSGO_WEDGE` request, nothing is answered any
    // more: tsrs with every worker looping.
    let wedge = std::env::var("RSVELTE_TEST_TSGO_WEDGE").ok();
    let mut wedged = false;
    // `RSVELTE_TEST_COMPONENT_IMPORT` names a component completions offer as
    // an auto-import (labelled with svelte2tsx's suffix, as tsgo does), and
    // whose resolve imports it on the line after the document's last import.
    let component_import = std::env::var("RSVELTE_TEST_COMPONENT_IMPORT").ok();
    let mut import_line = 0;
    let mut texts = std::collections::HashMap::new();
    let stdin = io::stdin();
    let mut input = stdin.lock();
    // Answers can come from other threads (`answer_later`), a whole message at a time.
    let output = Arc::new(Mutex::new(io::stdout()));
    let answer = |body: String| write_message(&mut *output.lock().unwrap(), &body).unwrap();
    let answer_later = |delay: Duration, body: String| {
        let output = Arc::clone(&output);
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            write_message(&mut *output.lock().unwrap(), &body).unwrap();
        });
    };
    loop {
        let Some(message) = read_message(&mut input).unwrap() else { return };
        let method = message
            .split_once(r#""method":""#)
            .and_then(|(_, rest)| rest.split('"').next())
            .unwrap_or("")
            .to_string();
        let uri = message
            .split_once(r#""uri":""#)
            .and_then(|(_, rest)| rest.split('"').next())
            .unwrap_or("")
            .to_string();
        if !method.is_empty() && method != "initialize" && method != "initialized" {
            let mut file = OpenOptions::new().create(true).append(true).open(&log).unwrap();
            let marked = if message.contains(&marker) { " marked" } else { "" };
            // One write per line (`writeln!` writes each piece on its own): a
            // restarted tsgo appends to the same log while the old one ends.
            file.write_all(format!("{method} {uri}{marked}\n").as_bytes()).unwrap();
        }
        if method == "textDocument/didOpen" {
            if let Some(text) = json_string_field(&message, "text") {
                texts.insert(uri.clone(), text);
            }
        }
        if method == "exit" {
            return;
        }
        wedged |= wedge.as_deref() == Some(method.as_str());
        // A request tsgo loses: never answered, not even once cancelled.
        if wedged || std::env::var("RSVELTE_TEST_TSGO_IGNORE").is_ok_and(|ignored| ignored == method) {
            continue;
        }
        if method == "initialize" {
            let id = json_id(&message);
            answer(format!(
                r#"{{"jsonrpc":"2.0","id":{id},"result":{{"capabilities":{{"positionEncoding":"utf-8","hoverProvider":true,"workspaceSymbolProvider":true,"documentSymbolProvider":true,"referencesProvider":true}}}}}}"#
            ));
        } else if !method.is_empty() && slow_methods.split(',').any(|slow| slow == method) {
            let id = json_id(&message);
            let result = if method == "textDocument/signatureHelp" {
                r#"{"signatures":[{"label":"slow(): void","parameters":[]}],"activeSignature":0}"#
            } else {
                "[]"
            };
            answer_later(
                delay("RSVELTE_TEST_SLOW_MS").unwrap(),
                format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{result}}}"#),
            );
        } else if method == "textDocument/completion" && component_import.is_some() {
            let id = json_id(&message);
            let name = component_import.as_deref().unwrap();
            import_line = texts
                .get(&uri)
                .and_then(|text: &String| {
                    text.lines().enumerate().filter(|(_, line)| line.starts_with("import ")).last()
                })
                .map(|(line, _)| line)
                .map_or(0, |line| line + 1);
            answer(format!(
                r#"{{"jsonrpc":"2.0","id":{id},"result":{{"isIncomplete":false,"items":[{{"label":"{name}__SvelteComponent_","kind":6,"data":{{"name":"{name}"}}}},{{"label":"Unrelated","kind":6}}]}}}}"#
            ));
        } else if method == "completionItem/resolve" && component_import.is_some() {
            let id = json_id(&message);
            let name = component_import.as_deref().unwrap();
            let line = import_line;
            answer(format!(
                r#"{{"jsonrpc":"2.0","id":{id},"result":{{"label":"{name}__SvelteComponent_","kind":6,"detail":"Add import from \"./{name}.svelte\"","additionalTextEdits":[{{"range":{{"start":{{"line":{line},"character":0}},"end":{{"line":{line},"character":0}}}},"newText":"import {name} from \"./{name}.svelte\";\n"}}]}}}}"#
            ));
        } else if method == "textDocument/completion" {
            if let Some(delay) = delay("RSVELTE_TEST_SLOW_COMPLETION_MS") {
                std::thread::sleep(delay);
            }
            let id = json_id(&message);
            answer(format!(r#"{{"jsonrpc":"2.0","id":{id},"result":null}}"#));
        } else if method == "textDocument/hover" {
            let id = json_id(&message);
            let body = format!(
                r#"{{"jsonrpc":"2.0","id":{id},"result":{{"contents":{{"kind":"plaintext","value":"recorded"}}}}}}"#
            );
            // Answered from another thread: a later message is read meanwhile.
            match delay("RSVELTE_TEST_HOVER_DELAY_MS") {
                Some(delay) => answer_later(delay, body),
                None => answer(body),
            }
        } else if method == "textDocument/diagnostic"
            && (delay("RSVELTE_TEST_SLOW_DIAGNOSTIC_MS").is_some() || diagnostic_word.is_some())
        {
            // A check that takes long, while every other request is answered.
            let id = json_id(&message);
            let items = diagnostic_word
                .as_deref()
                .and_then(|word| {
                    let text = texts.get(&uri)?;
                    let at = text.find(word)?;
                    let line = text[..at].matches('\n').count();
                    let character = at - text[..at].rfind('\n').map_or(0, |newline| newline + 1);
                    let end = character + word.len();
                    Some(format!(
                        r#"{{"range":{{"start":{{"line":{line},"character":{character}}},"end":{{"line":{line},"character":{end}}}}},"severity":1,"code":2322,"source":"ts","message":"recorded diagnostic"}}"#
                    ))
                })
                .unwrap_or_default();
            answer_later(
                delay("RSVELTE_TEST_SLOW_DIAGNOSTIC_MS").unwrap_or_default(),
                format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{"kind":"full","items":[{items}]}}}}"#),
            );
        } else if method == "textDocument/references"
            && std::env::var_os("RSVELTE_TEST_TEXT_REFERENCES").is_some()
        {
            // Every occurrence of the word at the position, as opened: a
            // store `count` and its value `$count` are two words, as they are
            // two symbols for tsgo.
            let id = json_id(&message);
            let number = |name: &str| -> Option<usize> {
                let rest = message.split_once(&format!(r#""{name}":"#))?.1;
                rest[..rest.find(|c: char| !c.is_ascii_digit())?].parse().ok()
            };
            let word = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$';
            let mut locations = Vec::new();
            if let (Some(text), Some(line), Some(character)) =
                (texts.get(&uri), number("line"), number("character"))
            {
                let line_start: usize = text.split('\n').take(line).map(|l| l.len() + 1).sum();
                let bytes = text.as_bytes();
                let at = line_start + character;
                let mut start = at;
                while start > 0 && word(bytes[start - 1]) {
                    start -= 1;
                }
                let mut end = at;
                while end < bytes.len() && word(bytes[end]) {
                    end += 1;
                }
                let name = &text[start..end];
                let mut from = 0;
                while let Some(found) = (!name.is_empty()).then(|| text[from..].find(name)).flatten() {
                    let found = from + found;
                    from = found + name.len();
                    if (found > 0 && word(bytes[found - 1])) || bytes.get(from).is_some_and(|b| word(*b)) {
                        continue;
                    }
                    let line = text[..found].matches('\n').count();
                    let character = found - text[..found].rfind('\n').map_or(0, |newline| newline + 1);
                    let end = character + name.len();
                    locations.push(format!(
                        r#"{{"uri":"{uri}","range":{{"start":{{"line":{line},"character":{character}}},"end":{{"line":{line},"character":{end}}}}}}}"#
                    ));
                }
            }
            answer(format!(r#"{{"jsonrpc":"2.0","id":{id},"result":[{}]}}"#, locations.join(",")));
        } else if method == "exit" {
            return;
        } else if method == "$/cancelRequest" {
            // A notification (its `id` names the request): an answered request
            // stays answered, a lost one stays lost.
        } else if !method.is_empty() && message.contains(r#""id":"#) {
            let id = json_id(&message);
            answer(format!(r#"{{"jsonrpc":"2.0","id":{id},"result":null}}"#));
        }
    }
}

fn read_message(reader: &mut impl BufRead) -> io::Result<Option<String>> {
    let mut content_length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 { return Ok(None) }
        if line == "\r\n" { break }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            content_length = Some(value.trim().parse::<usize>().unwrap());
        }
    }
    let mut body = vec![0; content_length.unwrap()];
    reader.read_exact(&mut body)?;
    Ok(Some(String::from_utf8(body).unwrap()))
}

fn write_message(writer: &mut impl Write, body: &str) -> io::Result<()> {
    write!(writer, "Content-Length: {}\r\n\r\n{body}", body.len())?;
    writer.flush()
}

fn json_id(message: &str) -> &str {
    let rest = message.split_once(r#""id":"#).unwrap().1;
    if let Some(rest) = rest.strip_prefix('"') {
        let end = rest.find('"').unwrap();
        &message[message.len() - rest.len() - 1..message.len() - rest.len() + end + 1]
    } else {
        let end = rest.find(|character: char| !character.is_ascii_digit() && character != '-').unwrap_or(rest.len());
        &rest[..end]
    }
}

/// The first string field `name` of a message, decoded.
fn json_string_field(message: &str, name: &str) -> Option<String> {
    let start = message.find(&format!(r#""{name}":""#))? + name.len() + 4;
    let mut value = String::new();
    let mut characters = message[start..].chars();
    while let Some(character) = characters.next() {
        match character {
            '"' => return Some(value),
            '\\' => match characters.next()? {
                'n' => value.push('\n'),
                't' => value.push('\t'),
                'r' => value.push('\r'),
                'u' => {
                    let hex = characters.by_ref().take(4).collect::<String>();
                    value.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?).unwrap_or('?'));
                }
                other => value.push(other),
            },
            other => value.push(other),
        }
    }
    None
}
"###;

/// `drop_degenerate_folding_ranges` is called on the tsgo half of
/// `textDocument/foldingRange`, and the unit test beside it calls the function
/// directly — so removing the call site leaves that suite green. A range whose
/// end precedes its start is what the call site drops in either folding mode.
#[cfg(unix)]
#[test]
fn a_reversed_folding_range_from_tsgo_never_reaches_the_client() {
    let dir = TestDir::new("folding-degenerate");
    let helper_source = dir.0.join("fake_tsgo.rs");
    let helper = dir.0.join("fake-tsgo");
    let state = dir.0.join("generation");
    fs::write(&helper_source, FAKE_TSGO_FOLDING).expect("write fake tsgo source");
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    assert!(
        Command::new(rustc)
            .arg("--edition=2024")
            .arg(&helper_source)
            .arg("-o")
            .arg(&helper)
            .status()
            .expect("run rustc")
            .success(),
        "compile fake tsgo"
    );
    fs::write(dir.0.join("package.json"), "{}").expect("write package.json");
    // A `.ts` document is proxied to tsgo outright, so its response is the one
    // the call site filters. It has to be long enough to hold the ranges the
    // fake reports, which are otherwise clamped away.
    let source = &"export const answer = 42;\n".repeat(50);
    let module = dir.0.join("main.ts");
    fs::write(&module, source).expect("write module");
    let uri = file_uri(&module);
    let component = dir.0.join("App.svelte");
    let component_source = "<script lang=\"ts\">\nlet answer = 42;\n</script>\n";
    fs::write(&component, component_source).expect("write component");
    let mut server = Server::start(
        Some(&dir.0),
        &[
            ("TSGO_BIN", helper.as_path()),
            ("RSVELTE_TEST_TSGO_STATE", state.as_path()),
        ],
    );
    server.open(&file_uri(&component), component_source);
    server.open_as(&uri, "typescript", source);

    // tsgo answers only once its overlay is up, so the well-formed range is what
    // says the proxied half reached this response at all.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let id = server.request(
            "textDocument/foldingRange",
            json!({ "textDocument": { "uri": uri } }),
        );
        let response = server.response(id, RESPONSE_TIMEOUT);
        let found: Vec<(i64, i64)> = response["result"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|range| {
                (
                    range["startLine"].as_i64().unwrap(),
                    range["endLine"].as_i64().unwrap(),
                )
            })
            .collect();
        if found.contains(&(30, 40)) {
            assert!(
                !found.iter().any(|(start, end)| end < start),
                "a reversed range reached the client: {found:?}"
            );
            server.shutdown();
            return;
        }
        assert!(
            Instant::now() < deadline,
            "tsgo never contributed a folding range: {found:?}"
        );
        thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(unix)]
const FAKE_TSGO_FOLDING: &str = r###"
use std::io::{self, BufRead, Write};

fn main() {
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let stdout = io::stdout();
    let mut output = stdout.lock();
    loop {
        let Some(message) = read_message(&mut input).unwrap() else { return };
        if message.contains(r#""method":"initialize""#) {
            let id = json_id(&message);
            write_message(&mut output, &format!(
                r#"{{"jsonrpc":"2.0","id":{id},"result":{{"capabilities":{{"positionEncoding":"utf-8","hoverProvider":true,"foldingRangeProvider":true}}}}}}"#
            )).unwrap();
        } else if message.contains(r#""method":"textDocument/foldingRange""#) {
            let id = json_id(&message);
            write_message(&mut output, &format!(
                r#"{{"jsonrpc":"2.0","id":{id},"result":[{{"startLine":25,"endLine":24}},{{"startLine":30,"endLine":40}}]}}"#
            )).unwrap();
        } else if message.contains(r#""method":"shutdown""#) {
            let id = json_id(&message);
            write_message(&mut output, &format!(r#"{{"jsonrpc":"2.0","id":{id},"result":null}}"#)).unwrap();
        } else if message.contains(r#""method":"exit""#) {
            return;
        } else if message.contains(r#""method":""#) && message.contains(r#""id":"#) {
            let id = json_id(&message);
            write_message(&mut output, &format!(r#"{{"jsonrpc":"2.0","id":{id},"result":null}}"#)).unwrap();
        }
    }
}

fn read_message(reader: &mut impl BufRead) -> io::Result<Option<String>> {
    let mut content_length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 { return Ok(None) }
        if line == "\r\n" { break }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            content_length = Some(value.trim().parse::<usize>().unwrap());
        }
    }
    let mut body = vec![0; content_length.unwrap()];
    reader.read_exact(&mut body)?;
    Ok(Some(String::from_utf8(body).unwrap()))
}

fn write_message(writer: &mut impl Write, body: &str) -> io::Result<()> {
    write!(writer, "Content-Length: {}\r\n\r\n{body}", body.len())?;
    writer.flush()
}

/// The id is echoed as the JSON literal it arrived as: tsgo's ids are strings,
/// and stripping the quotes makes every response unparseable.
fn json_id(message: &str) -> &str {
    let rest = message.split_once(r#""id":"#).unwrap().1;
    if rest.starts_with('"') {
        let end = rest[1..].find('"').unwrap() + 2;
        &rest[..end]
    } else {
        let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
        &rest[..end]
    }
}
"###;
