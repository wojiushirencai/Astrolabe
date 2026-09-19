use std::{
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    process::{Child, ChildStdin, Command, Stdio},
    time::{Duration, Instant},
};

use astrolabe_mcp::{TOOL_CATALOG_TTL_MS, TOOL_COUNT};
use serde_json::{json, Value};

fn unique_temp_dir() -> PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "astrolabe-mcp-stdio-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos(),
        id
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    std::fs::write(dir.join("hello.py"), "def greet():\n    return 'hi'\n").unwrap();
    dir.canonicalize().unwrap_or(dir)
}

struct Server {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
    root: PathBuf,
}

impl Server {
    fn spawn() -> Self {
        let root = unique_temp_dir();
        // Mirror Claude Code's global config: `args: ["."]` plus the client's cwd.
        let mut child = Command::new(env!("CARGO_BIN_EXE_astrolabe"))
            .arg(".")
            .current_dir(&root)
            .env("RUST_LOG", "warn")
            .env_remove("ASTROLABE_STRUCTURED")
            .env_remove("ASTROLABE_ROOT")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start astrolabe MCP server");
        let stdin = child.stdin.take().expect("child stdin");
        let stdout = BufReader::new(child.stdout.take().expect("child stdout"));
        Self {
            child,
            stdin,
            stdout,
            root,
        }
    }

    fn rpc_line(&mut self, line: &str) -> Value {
        writeln!(self.stdin, "{line}").expect("write JSON-RPC request");
        self.stdin.flush().expect("flush JSON-RPC request");
        let mut response = String::new();
        self.stdout
            .read_line(&mut response)
            .expect("read JSON-RPC response");
        assert!(
            !response.trim().is_empty(),
            "server returned no JSON-RPC line for {line}"
        );
        serde_json::from_str(&response).unwrap_or_else(|error| {
            panic!("invalid JSON-RPC response for {line}: {error}; line={response:?}")
        })
    }

    fn rpc(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.rpc_line(
            &json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params,
            })
            .to_string(),
        )
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn assert_tool_catalog(response: &Value) {
    assert_eq!(
        response.get("error"),
        None,
        "unexpected JSON-RPC error: {response}"
    );
    let tools = response["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools array missing: {response}"));
    assert_ne!(
        tools.len(),
        0,
        "tools/list must not silently return an empty catalog: {response}"
    );
    assert_eq!(tools.len(), TOOL_COUNT, "unexpected tool count: {response}");
    assert_eq!(response["result"]["ttlMs"], TOOL_CATALOG_TTL_MS);
}

#[test]
fn stdio_tools_list_without_initialize_returns_catalog() {
    let mut server = Server::spawn();
    // Bare request: no `initialize`, no `_meta`, no `params`. This is the
    // 2026-07-28 client path that previously produced a silent empty catalog.
    let response = server.rpc_line(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
    assert_eq!(response["id"], 1);
    assert_tool_catalog(&response);
}

#[test]
fn stdio_tools_list_with_inline_protocol_meta() {
    let mut server = Server::spawn();
    let response = server.rpc(
        1,
        "tools/list",
        json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientInfo": {
                    "name": "astrolabe-smoke-test",
                    "version": "1.0.0"
                },
                "io.modelcontextprotocol/clientCapabilities": {}
            }
        }),
    );
    assert_eq!(response["id"], 1);
    assert_tool_catalog(&response);
    assert_eq!(response["result"]["cacheScope"], "public");
}

#[test]
fn stdio_tools_list_after_initialize_includes_ttl() {
    let mut server = Server::spawn();
    let init = server.rpc(
        1,
        "initialize",
        json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "astrolabe-smoke-test", "version": "1.0.0"}
        }),
    );
    assert_eq!(init.get("error"), None, "initialize failed: {init}");
    let response = server.rpc(2, "tools/list", json!({}));
    assert_eq!(response["id"], 2);
    assert_tool_catalog(&response);
}

#[test]
fn stdio_get_languages_returns_nonempty_text() {
    let mut server = Server::spawn();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut last;
    let mut id = 1;
    // Ready gate: wait until the index leaves Building (not "text contains python").
    // An empty / non-Python workspace would otherwise spin until the deadline.
    loop {
        last = server.rpc(
            id,
            "tools/call",
            json!({
                "name": "get_languages",
                "arguments": {"budget_tokens": 2500}
            }),
        );
        id += 1;
        assert_eq!(last.get("error"), None, "tools/call protocol error: {last}");
        let text = last["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default();
        assert!(
            !text.trim().is_empty(),
            "content[0].text must be non-empty: {last}"
        );
        assert!(
            !text.contains("尚未实现"),
            "index path still reports unimplemented: {text}"
        );
        let still_building = text.contains("索引正在后台构建")
            || text.to_lowercase().contains("still building")
            || text.to_lowercase().contains("index is building");
        if still_building {
            if Instant::now() >= deadline {
                panic!("timed out waiting for index; last response: {last}");
            }
            std::thread::sleep(Duration::from_millis(200));
            continue;
        }

        // Index is ready (or failed). Assert on the fixture contents.
        assert!(
            text.to_lowercase().contains("python"),
            "expected python language after index ready: {text}"
        );
        assert!(
            text.contains("index_root:"),
            "tool text must declare the absolute index root: {text}"
        );
        let root = server
            .root
            .canonicalize()
            .unwrap_or_else(|_| server.root.clone());
        assert!(
            text.contains(&root.display().to_string()),
            "index_root should be the temp project root (unique_temp_dir creates .git): {text}"
        );
        assert!(
            last["result"].get("structuredContent").is_none()
                || last["result"]["structuredContent"].is_null(),
            "default wire format is text-only; leftover metadata hides hits in Claude Code: {last}"
        );
        return;
    }
}
