//! End-to-end tests for the MCP server's stdio JSON-RPC surface.
//!
//! These tests spawn the real `persona-journal-mcp mcp` binary and drive it
//! through stdio, exercising the same path that Claude / any MCP client takes.
//! They cover the layer the in-process unit tests bypass: the JSON-RPC wire
//! format and the tools advertised in `tools/list`.
//!
//! Origin: mcp-e2e-transparency-test (kind tags round-trip via wire layer).

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{json, Value};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_persona-journal-mcp");

/// JSON-RPC client speaking to a spawned `persona-journal-mcp mcp` over stdio.
///
/// The server emits one JSON document per line on stdout. Notifications
/// carry no `id`; method calls echo the request `id`. We dispatch on
/// `id` so that out-of-order notifications cannot desync the test.
struct McpClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl McpClient {
    fn spawn(root: &Path) -> Self {
        let mut child = Command::new(BIN)
            .arg("mcp")
            .arg("--root")
            .arg(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn persona-journal-mcp");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        let mut c = Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        };
        c.handshake();
        c
    }

    fn send_line(&mut self, msg: &Value) {
        let line = serde_json::to_string(msg).unwrap();
        self.stdin.write_all(line.as_bytes()).unwrap();
        self.stdin.write_all(b"\n").unwrap();
        self.stdin.flush().unwrap();
    }

    /// Read response lines until one matches `wanted_id`. Notifications
    /// (no `id`) are silently consumed.
    fn recv_for(&mut self, wanted_id: u64) -> Value {
        loop {
            let mut line = String::new();
            let n = self
                .stdout
                .read_line(&mut line)
                .expect("read mcp stdout line");
            assert!(n > 0, "mcp server closed stdout before response");
            let v: Value = serde_json::from_str(line.trim()).unwrap_or_else(|e| {
                panic!("mcp emitted non-JSON line: {line:?} ({e})");
            });
            match v.get("id").and_then(Value::as_u64) {
                Some(id) if id == wanted_id => return v,
                _ => continue,
            }
        }
    }

    fn handshake(&mut self) {
        let id = self.next_id();
        self.send_line(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "persona-journal-e2e", "version": "0"},
            }
        }));
        let resp = self.recv_for(id);
        assert!(resp.get("error").is_none(), "initialize failed: {resp:?}");
        self.send_line(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
        }));
    }

    fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn rpc(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id();
        self.send_line(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }));
        let resp = self.recv_for(id);
        assert!(resp.get("error").is_none(), "rpc {method} error: {resp:?}");
        resp.get("result").cloned().expect("result missing")
    }

    /// Invoke an MCP tool and decode its single text content payload as JSON.
    /// persona-journal-mcp returns `{"content":[{"type":"text","text":"<json>"}]}`.
    fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
        let result = self.rpc("tools/call", json!({"name": name, "arguments": arguments}));
        assert_eq!(
            result.get("isError").and_then(Value::as_bool),
            Some(false),
            "tool {name} returned error: {result:?}"
        );
        let text = result
            .pointer("/content/0/text")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("tool {name} produced no text content: {result:?}"));
        serde_json::from_str(text)
            .unwrap_or_else(|e| panic!("tool {name} text was not JSON: {text:?} ({e})"))
    }

    fn list_tools(&mut self) -> Vec<Value> {
        let result = self.rpc("tools/list", json!({}));
        result
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .expect("tools array")
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Minimal tempdir layout: just a root path. Journal init creates subdirs lazily.
struct Layout {
    _tmp: TempDir,
    root: PathBuf,
}

fn make_layout() -> Layout {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path().to_path_buf();
    Layout { _tmp: tmp, root }
}

// ---------------------------------------------------------------------------
// tools/list: all twelve tools must be advertised.
// ---------------------------------------------------------------------------

#[test]
fn tools_list_contains_all_twelve_tools() {
    let layout = make_layout();
    let mut client = McpClient::spawn(&layout.root);
    let tools = client.list_tools();
    let names: Vec<&str> = tools
        .iter()
        .map(|t| t.get("name").and_then(Value::as_str).unwrap())
        .collect();
    for expected in [
        "journal_say",
        "journal_query_latest",
        "journal_entry_read",
        "journal_kind_register",
        "journal_kind_list",
        "journal_projection_rebuild",
        "journal_reload_kinds",
        "journal_query_by_retrieval",
        "journal_filter",
        "journal_pin",
        "journal_unpin",
        "journal_boost_kind",
    ] {
        assert!(names.contains(&expected), "{expected} missing: {names:?}");
    }
}

// ---------------------------------------------------------------------------
// kind tags round-trip: register a kind with tags via JSON-RPC wire, then list
// and assert the tags survive the round-trip exactly.
// ---------------------------------------------------------------------------

#[test]
fn kind_register_and_list_round_trip_tags() {
    let layout = make_layout();
    let mut c = McpClient::spawn(&layout.root);

    let persona = "alice";
    let kind = "diary";
    let expected_tags = vec!["work", "personal"];

    // KindRegisterParams fields: persona / config_toml (TOML body) / root (optional).
    // tags are expressed inside the TOML body as `tags = [...]`.
    let config_toml = format!(
        "kind = \"{kind}\"\n\
         mode = \"entries\"\n\
         path_template = \"{{persona}}/{{kind}}/{{persona}}_{{kind}}_{{yyyy}}-{{mm}}_{{seq:05}}.md\"\n\
         versioning = true\n\
         indexed = true\n\
         tags = [\"work\", \"personal\"]\n"
    );

    let reg = c.call_tool(
        "journal_kind_register",
        json!({
            "persona": persona,
            "config_toml": config_toml,
        }),
    );
    assert_eq!(
        reg.get("kind").and_then(Value::as_str),
        Some(kind),
        "register response kind mismatch: {reg:?}"
    );

    // journal_kind_list returns a JSON array of KindConfig objects.
    let list_val = c.call_tool("journal_kind_list", json!({"persona": persona}));
    let list = list_val
        .as_array()
        .unwrap_or_else(|| panic!("journal_kind_list did not return an array: {list_val:?}"));
    let entry = list
        .iter()
        .find(|v| v.get("kind").and_then(Value::as_str) == Some(kind))
        .unwrap_or_else(|| panic!("registered kind '{kind}' missing from list: {list_val:?}"));
    let listed_tags: Vec<&str> = entry
        .get("tags")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("tags field missing or not array in {entry:?}"))
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(
        listed_tags, expected_tags,
        "tags round-trip mismatch: got {listed_tags:?}"
    );
}

// ---------------------------------------------------------------------------
// ensure_default_kinds: after registering a kind (or saying), journal_kind_list
// must return a non-empty array — verifying the auto-init path over the wire.
// ---------------------------------------------------------------------------

#[test]
fn kind_list_returns_default_kinds_after_register() {
    let layout = make_layout();
    let mut c = McpClient::spawn(&layout.root);

    let persona = "bob";

    // Register a minimal kind to trigger ensure_default_kinds on the server side.
    let config_toml = "kind = \"note\"\n\
                       mode = \"entries\"\n\
                       path_template = \"{persona}/{kind}/{persona}_{kind}_{yyyy}-{mm}_{seq:05}.md\"\n\
                       versioning = true\n\
                       indexed = true\n";
    let reg = c.call_tool(
        "journal_kind_register",
        json!({
            "persona": persona,
            "config_toml": config_toml,
        }),
    );
    assert!(
        reg.get("kind").is_some(),
        "register should return kind field: {reg:?}"
    );

    let list_val = c.call_tool("journal_kind_list", json!({"persona": persona}));
    let list = list_val
        .as_array()
        .unwrap_or_else(|| panic!("journal_kind_list did not return an array: {list_val:?}"));
    assert!(
        !list.is_empty(),
        "journal_kind_list returned empty array after register"
    );
}

// ---------------------------------------------------------------------------
// .journal.toml loader: auto-loads kinds on first journal_say call.
// ---------------------------------------------------------------------------

#[test]
fn journal_toml_loader_auto_loads_kinds() {
    let layout = make_layout();

    // Write .journal.toml with 3 kinds before spawning the server.
    let persona = "alice";
    let persona_dir = layout.root.join(persona);
    std::fs::create_dir_all(&persona_dir).expect("create persona dir");
    std::fs::write(
        persona_dir.join(".journal.toml"),
        r#"
[[kinds]]
kind = "emo"
mode = "entries"
path_template = "{persona}/{kind}/{persona}_{kind}_{yyyy}-{mm}_{seq:05}.md"
versioning = true
indexed = true
tags = []

[[kinds]]
kind = "autoload_b"
mode = "entries"
path_template = "{persona}/{kind}/{persona}_{kind}_{yyyy}-{mm}_{seq:05}.md"
versioning = true
indexed = true
tags = []

[[kinds]]
kind = "autoload_c"
mode = "entries"
path_template = "{persona}/{kind}/{persona}_{kind}_{yyyy}-{mm}_{seq:05}.md"
versioning = false
indexed = true
tags = []
"#
        .trim(),
    )
    .expect("write .journal.toml");

    let mut c = McpClient::spawn(&layout.root);

    // journal_say triggers ensure_loaded (open_db path).
    let say_result = c.call_tool(
        "journal_say",
        json!({
            "persona": persona,
            "kind": "emo",
            "text": "hello from loader test",
            "tags": [],
        }),
    );
    let say_id = say_result["id"]
        .as_str()
        .expect("journal_say must return id as string");
    assert!(
        say_id.starts_with("emo/"),
        "journal_say id must be uname with kind prefix: {say_id}"
    );

    // All 3 kinds from .journal.toml should now be registered.
    let list_val = c.call_tool("journal_kind_list", json!({"persona": persona}));
    let list = list_val
        .as_array()
        .unwrap_or_else(|| panic!("journal_kind_list did not return an array: {list_val:?}"));
    let names: Vec<&str> = list
        .iter()
        .filter_map(|v| v.get("kind").and_then(Value::as_str))
        .collect();

    assert!(names.contains(&"emo"), "emo missing: {names:?}");
    assert!(
        names.contains(&"autoload_b"),
        "autoload_b missing: {names:?}"
    );
    assert!(
        names.contains(&"autoload_c"),
        "autoload_c missing: {names:?}"
    );
}

// ---------------------------------------------------------------------------
// journal_reload_kinds: explicit reload inserts new kinds, returns count.
// ---------------------------------------------------------------------------

#[test]
fn journal_reload_kinds_returns_reloaded_count() {
    let layout = make_layout();

    let persona = "carol";
    let persona_dir = layout.root.join(persona);
    std::fs::create_dir_all(&persona_dir).expect("create persona dir");

    // Initial .journal.toml with one kind.
    std::fs::write(
        persona_dir.join(".journal.toml"),
        r#"
[[kinds]]
kind = "emo"
mode = "entries"
path_template = "{persona}/{kind}/{persona}_{kind}_{yyyy}-{mm}_{seq:05}.md"
versioning = true
indexed = true
tags = []
"#
        .trim(),
    )
    .expect("write initial .journal.toml");

    let mut c = McpClient::spawn(&layout.root);

    // Trigger initial load via journal_say.
    let _ = c.call_tool(
        "journal_say",
        json!({
            "persona": persona,
            "kind": "emo",
            "text": "initial entry",
            "tags": [],
        }),
    );

    // Now update .journal.toml to add a new kind.
    std::fs::write(
        persona_dir.join(".journal.toml"),
        r#"
[[kinds]]
kind = "emo"
mode = "entries"
path_template = "{persona}/{kind}/{persona}_{kind}_{yyyy}-{mm}_{seq:05}.md"
versioning = true
indexed = true
tags = []

[[kinds]]
kind = "reload_extra"
mode = "entries"
path_template = "{persona}/{kind}/{persona}_{kind}_{yyyy}-{mm}_{seq:05}.md"
versioning = true
indexed = true
tags = []
"#
        .trim(),
    )
    .expect("write updated .journal.toml");

    // Call journal_reload_kinds — should return reloaded=1 (reload_extra is new, emo is skipped).
    let result = c.call_tool("journal_reload_kinds", json!({ "persona": persona }));
    let reloaded = result
        .get("reloaded")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("reloaded field missing: {result:?}"));
    assert!(
        reloaded >= 1,
        "expected reloaded >= 1 (reload_extra is new), got {reloaded}"
    );
}

// ---------------------------------------------------------------------------
// journal_query_by_retrieval: register kind, say 1 entry, query and assert
// the response array contains 1 item with a retrieval_strength field.
// ---------------------------------------------------------------------------

#[test]
fn journal_query_by_retrieval_smoke() {
    let layout = make_layout();
    let mut c = McpClient::spawn(&layout.root);

    let persona = "dora";
    let kind = "mem";
    let config_toml = format!(
        "kind = \"{kind}\"\n\
         mode = \"entries\"\n\
         path_template = \"{{persona}}/{{kind}}/{{persona}}_{{kind}}_{{yyyy}}-{{mm}}_{{seq:05}}.md\"\n\
         versioning = true\n\
         indexed = true\n\
         tags = []\n"
    );

    // Register kind.
    c.call_tool(
        "journal_kind_register",
        json!({ "persona": persona, "config_toml": config_toml }),
    );

    // Append one entry.
    c.call_tool(
        "journal_say",
        json!({ "persona": persona, "kind": kind, "text": "retrieval smoke entry", "tags": [] }),
    );

    // Query by retrieval — should return at least 1 row with retrieval_strength field.
    let result = c.call_tool(
        "journal_query_by_retrieval",
        json!({ "persona": persona, "kind": kind, "n": 10 }),
    );
    let arr = result
        .as_array()
        .unwrap_or_else(|| panic!("journal_query_by_retrieval should return array: {result:?}"));
    assert!(
        !arr.is_empty(),
        "journal_query_by_retrieval returned empty array"
    );
    assert!(
        arr[0].get("retrieval_strength").is_some(),
        "retrieval_strength field missing from response row: {:?}",
        arr[0]
    );
}

// ---------------------------------------------------------------------------
// journal_filter: register kind, say 1 entry, filter with Visible mode.
// ---------------------------------------------------------------------------

#[test]
fn journal_filter_smoke() {
    let layout = make_layout();
    let mut c = McpClient::spawn(&layout.root);

    let persona = "dora";
    let kind = "filter-mem";
    let config_toml = format!(
        "kind = \"{kind}\"\n\
         mode = \"entries\"\n\
         path_template = \"{{persona}}/{{kind}}/{{persona}}_{{kind}}_{{yyyy}}-{{mm}}_{{seq:05}}.md\"\n\
         versioning = true\n\
         indexed = true\n\
         tags = []\n"
    );

    c.call_tool(
        "journal_kind_register",
        json!({ "persona": persona, "config_toml": config_toml }),
    );
    c.call_tool(
        "journal_say",
        json!({ "persona": persona, "kind": kind, "text": "filter smoke entry", "tags": [] }),
    );

    let result = c.call_tool(
        "journal_filter",
        json!({
            "persona": persona,
            "kind": kind,
            "mode": { "type": "visible", "threshold": 0.0 },
        }),
    );
    let arr = result
        .as_array()
        .unwrap_or_else(|| panic!("journal_filter should return array: {result:?}"));
    assert!(!arr.is_empty(), "journal_filter returned empty array");
}

// ---------------------------------------------------------------------------
// journal_pin + journal_unpin round-trip: pin to 0.5, verify, unpin, verify.
// ---------------------------------------------------------------------------

#[test]
fn journal_pin_unpin_round_trip_smoke() {
    let layout = make_layout();
    let mut c = McpClient::spawn(&layout.root);

    let persona = "dora";
    let kind = "pin-mem";
    let config_toml = format!(
        "kind = \"{kind}\"\n\
         mode = \"entries\"\n\
         path_template = \"{{persona}}/{{kind}}/{{persona}}_{{kind}}_{{yyyy}}-{{mm}}_{{seq:05}}.md\"\n\
         versioning = true\n\
         indexed = true\n\
         tags = []\n"
    );

    c.call_tool(
        "journal_kind_register",
        json!({ "persona": persona, "config_toml": config_toml }),
    );
    let say_result = c.call_tool(
        "journal_say",
        json!({ "persona": persona, "kind": kind, "text": "pin smoke entry", "tags": [] }),
    );
    let entry_id = say_result["id"]
        .as_str()
        .expect("journal_say must return id as string");

    // Pin to 0.5.
    let pin_result = c.call_tool(
        "journal_pin",
        json!({ "persona": persona, "entry_id": entry_id, "strength": 0.5 }),
    );
    assert_eq!(
        pin_result.get("ok").and_then(Value::as_bool),
        Some(true),
        "journal_pin should return ok:true: {pin_result:?}"
    );

    // Verify retrieval_strength = 0.5 via query_by_retrieval.
    let after_pin = c.call_tool(
        "journal_query_by_retrieval",
        json!({ "persona": persona, "kind": kind, "n": 10 }),
    );
    let arr = after_pin
        .as_array()
        .unwrap_or_else(|| panic!("query_by_retrieval should return array: {after_pin:?}"));
    let strength_after_pin = arr[0]
        .get("retrieval_strength")
        .and_then(Value::as_f64)
        .unwrap_or_else(|| panic!("retrieval_strength missing after pin: {:?}", arr[0]));
    assert!(
        (strength_after_pin - 0.5).abs() < 1e-9,
        "expected retrieval_strength ≈ 0.5, got {strength_after_pin}"
    );

    // Unpin — should reset to 1.0.
    let unpin_result = c.call_tool(
        "journal_unpin",
        json!({ "persona": persona, "entry_id": entry_id }),
    );
    assert_eq!(
        unpin_result.get("ok").and_then(Value::as_bool),
        Some(true),
        "journal_unpin should return ok:true: {unpin_result:?}"
    );

    // Verify retrieval_strength = 1.0 after unpin.
    let after_unpin = c.call_tool(
        "journal_query_by_retrieval",
        json!({ "persona": persona, "kind": kind, "n": 10 }),
    );
    let arr2 = after_unpin
        .as_array()
        .unwrap_or_else(|| panic!("query_by_retrieval should return array: {after_unpin:?}"));
    let strength_after_unpin = arr2[0]
        .get("retrieval_strength")
        .and_then(Value::as_f64)
        .unwrap_or_else(|| panic!("retrieval_strength missing after unpin: {:?}", arr2[0]));
    assert!(
        (strength_after_unpin - 1.0).abs() < 1e-9,
        "expected retrieval_strength ≈ 1.0 after unpin, got {strength_after_unpin}"
    );
}

// ---------------------------------------------------------------------------
// journal_boost_kind: register kind, set boost, verify ok response.
// ---------------------------------------------------------------------------

#[test]
fn journal_boost_kind_smoke() {
    let layout = make_layout();
    let mut c = McpClient::spawn(&layout.root);

    let persona = "dora";
    let kind = "boost-mem";
    let config_toml = format!(
        "kind = \"{kind}\"\n\
         mode = \"entries\"\n\
         path_template = \"{{persona}}/{{kind}}/{{persona}}_{{kind}}_{{yyyy}}-{{mm}}_{{seq:05}}.md\"\n\
         versioning = true\n\
         indexed = true\n\
         tags = []\n"
    );

    c.call_tool(
        "journal_kind_register",
        json!({ "persona": persona, "config_toml": config_toml }),
    );

    let result = c.call_tool(
        "journal_boost_kind",
        json!({ "persona": persona, "kind": kind, "factor": 2.0 }),
    );
    assert_eq!(
        result.get("ok").and_then(Value::as_bool),
        Some(true),
        "journal_boost_kind should return ok:true: {result:?}"
    );
}
