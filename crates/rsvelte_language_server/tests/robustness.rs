use std::collections::{HashMap, HashSet};
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
        };
        let root_uri = root.map(file_uri).map(Value::String).unwrap_or(Value::Null);
        let id = server.request(
            "initialize",
            json!({
                "processId": Value::Null,
                "rootUri": root_uri,
                "capabilities": {
                    "general": { "positionEncodings": ["utf-8", "utf-16"] },
                    "workspace": { "configuration": true },
                },
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
    let root = fs::canonicalize(&dir.0).unwrap();
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
        root.join(".svelte-rs/projects")
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
            writeln!(file, "didOpen {uri}").unwrap();
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
#[cfg(unix)]
fn traffic_recorder(dir: &Path) -> PathBuf {
    let source = dir.join("fake_tsgo_traffic.rs");
    let helper = dir.join("fake-tsgo-traffic");
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
#[cfg(unix)]
fn traffic(log: &Path) -> Vec<String> {
    fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

#[cfg(unix)]
fn hover(server: &mut Server, uri: &str) -> Value {
    let id = server.request(
        "textDocument/hover",
        json!({ "textDocument": { "uri": uri }, "position": { "line": 1, "character": 5 } }),
    );
    server.response(id, Duration::from_secs(5))
}

/// A project that loads after an editor buffer changed holds a copy of that
/// component: the copy must carry the buffer, not the file (review s1).
#[cfg(unix)]
#[test]
fn a_project_loaded_later_gets_the_unsaved_buffers_it_copies() {
    let dir = TestDir::new("late-copy");
    let helper = traffic_recorder(&dir.0);
    let log = dir.0.join("tsgo.log");
    let root = fs::canonicalize(&dir.0).unwrap().join("ws");
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
    let copy = file_uri(&root.join(".svelte-rs/projects/a/tsgo/svelte/b/Child.svelte.tsx"));
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
    let root = fs::canonicalize(&dir.0).unwrap().join("ws");
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
            .all(|line| line.contains("/.svelte-rs/projects/b/")),
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
    let root = fs::canonicalize(&dir.0).unwrap().join("ws");
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
    let shadow = file_uri(&root.join(".svelte-rs/tsgo/svelte/Parent.svelte.tsx"));
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

/// A workspace with three projects that own components and one that owns
/// none, for the background-loading tests.
#[cfg(unix)]
fn three_projects(dir: &Path) -> PathBuf {
    let root = fs::canonicalize(dir).unwrap().join("ws");
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
                .join(".svelte-rs/projects")
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
        !root.join(".svelte-rs/projects/tests").exists(),
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
                .join(".svelte-rs/projects")
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

#[cfg(unix)]
const FAKE_TSGO_TRAFFIC: &str = r###"
use std::fs::OpenOptions;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

fn main() {
    let log = PathBuf::from(std::env::var_os("RSVELTE_TEST_TSGO_LOG").unwrap());
    let marker = std::env::var("RSVELTE_TEST_MARKER").unwrap_or_else(|_| "\u{0}".to_string());
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let stdout = io::stdout();
    let mut output = stdout.lock();
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
            writeln!(file, "{method} {uri}{marked}").unwrap();
        }
        if method == "initialize" {
            let id = json_id(&message);
            write_message(&mut output, &format!(
                r#"{{"jsonrpc":"2.0","id":{id},"result":{{"capabilities":{{"positionEncoding":"utf-8","hoverProvider":true,"workspaceSymbolProvider":true,"documentSymbolProvider":true,"referencesProvider":true}}}}}}"#
            )).unwrap();
        } else if method == "textDocument/completion" {
            if let Some(delay) = std::env::var("RSVELTE_TEST_SLOW_COMPLETION_MS").ok().and_then(|ms| ms.parse().ok()) {
                std::thread::sleep(std::time::Duration::from_millis(delay));
            }
            let id = json_id(&message);
            write_message(&mut output, &format!(r#"{{"jsonrpc":"2.0","id":{id},"result":null}}"#)).unwrap();
        } else if method == "textDocument/hover" {
            let id = json_id(&message);
            write_message(&mut output, &format!(
                r#"{{"jsonrpc":"2.0","id":{id},"result":{{"contents":{{"kind":"plaintext","value":"recorded"}}}}}}"#
            )).unwrap();
        } else if method == "exit" {
            return;
        } else if !method.is_empty() && message.contains(r#""id":"#) {
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
