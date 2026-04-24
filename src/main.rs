/// statix-lsp: a minimal LSP adapter that wraps the `statix` CLI linter.
///
/// Zed (and any LSP client) communicates over stdin/stdout using JSON-RPC with
/// the LSP framing:
///
///   Content-Length: <N>\r\n
///   \r\n
///   <N bytes of JSON>
///
/// This adapter handles the handshake (initialize/initialized) and responds to
/// didOpen / didChange / didSave by running `statix check --stdin -o json`,
/// parsing the JSON output, and pushing `textDocument/publishDiagnostics`
/// notifications back to the client.
/// It also caches the output to efficiently provide `textDocument/codeAction`
/// responses without re-running the CLI.
use std::{
    collections::HashMap,
    io::{self, BufRead, Write},
    process::{Command, Stdio},
};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

// -- LSP wire types ------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RpcRequest {
    id: Option<Value>,
    method: String,
    params: Option<Value>,
}

#[derive(Debug, Serialize)]
struct RpcResponse {
    jsonrpc: &'static str,
    id: Value,
    result: Value,
}

// -- Statix JSON output types --------------------------------------------------

#[derive(Debug, Deserialize, Clone)]
struct StatixOutput {
    report: Vec<Report>,
}

#[derive(Debug, Deserialize, Clone)]
struct Report {
    note: String,
    code: u32,
    severity: String,
    diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Deserialize, Clone)]
struct Diagnostic {
    at: Span,
    message: String,
    suggestion: Option<Suggestion>,
}

#[derive(Debug, Deserialize, Clone)]
struct Suggestion {
    at: Span,
    fix: String,
}

#[derive(Debug, Deserialize, Clone)]
struct Span {
    from: Pos,
    to: Pos,
}

#[derive(Debug, Deserialize, Clone)]
struct Pos {
    line: u32,
    column: u32,
}

// -- LSP helpers ---------------------------------------------------------------

fn severity_to_lsp(s: &str) -> u32 {
    match s {
        "Error" => 1,
        "Warn" => 2,
        "Info" => 3,
        _ => 4,
    }
}

/// Convert statix JSON output into a JSON array of LSP Diagnostic objects.
fn statix_to_lsp_diagnostics(output: &StatixOutput) -> Value {
    let mut lsp_diags = Vec::new();

    for report in &output.report {
        let severity = severity_to_lsp(&report.severity);
        let code = report.code.to_string();

        for diag in &report.diagnostics {
            // Statix uses 1-based lines and columns; LSP uses 0-based.
            let start_line = diag.at.from.line.saturating_sub(1);
            let start_char = diag.at.from.column.saturating_sub(1);
            let end_line = diag.at.to.line.saturating_sub(1);
            let end_char = diag.at.to.column.saturating_sub(1);

            lsp_diags.push(json!({
                "range": {
                    "start": { "line": start_line, "character": start_char },
                    "end":   { "line": end_line,   "character": end_char   }
                },
                "severity": severity,
                "code": code,
                "source": "statix",
                // Combine the report-level note with the per-span message so
                // hovering any underlined span shows full context.
                "message": format!("{}: {}", report.note, diag.message),
            }));
        }
    }

    json!(lsp_diags)
}

// -- LSP I/O -------------------------------------------------------------------

fn read_message(stdin: &mut impl BufRead) -> Option<Value> {
    // Read headers until blank line
    let mut content_length: Option<usize> = None;
    loop {
        let mut header = String::new();
        stdin.read_line(&mut header).ok()?;
        let header = header.trim_end_matches(['\r', '\n']);
        if header.is_empty() {
            break;
        }
        if let Some(val) = header.strip_prefix("Content-Length: ") {
            content_length = val.trim().parse().ok();
        }
    }

    let len = content_length?;
    let mut buf = vec![0u8; len];
    stdin.read_exact(&mut buf).ok()?;
    serde_json::from_slice(&buf).ok()
}

fn send_message(stdout: &mut impl Write, msg: &Value) {
    let body = serde_json::to_string(msg).unwrap();
    write!(stdout, "Content-Length: {}\r\n\r\n{}", body.len(), body).unwrap();
    stdout.flush().unwrap();
}

// -- Statix runner -------------------------------------------------------------

/// Run `statix check --stdin --format json` on the given source text.
/// Returns the parsed JSON output, or None on failure.
fn run_statix(source: &str) -> Option<StatixOutput> {
    let mut child = Command::new("statix")
        .args(["check", "--stdin", "--format", "json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    child.stdin.take()?.write_all(source.as_bytes()).ok()?;
    let output = child.wait_with_output().ok()?;

    // statix exits 1 when it finds issues — that's normal, not an error.
    serde_json::from_slice(&output.stdout).ok()
}

/// Create an LSP `publishDiagnostics` notification from the statix output.
fn publish_diagnostics(uri: &str, output: Option<&StatixOutput>) -> Value {
    let diagnostics = output
        .map(statix_to_lsp_diagnostics)
        .unwrap_or_else(|| json!([]));

    json!({
        "jsonrpc": "2.0",
        "method": "textDocument/publishDiagnostics",
        "params": {
            "uri": uri,
            "diagnostics": diagnostics
        }
    })
}

// -- Document State ------------------------------------------------------------

/// We cache the latest text and the parsed statix output for each document.
/// This avoids running the `statix` CLI again just to fulfill a codeAction request.
struct Document {
    text: String,
    output: Option<StatixOutput>,
}

// -- Document update helper ----------------------------------------------------

fn process_document_update(
    uri: String,
    text: String,
    documents: &mut HashMap<String, Document>,
    stdout: &mut impl std::io::Write,
) {
    let output = run_statix(&text);
    let notification = publish_diagnostics(&uri, output.as_ref());
    documents.insert(uri, Document { text, output });
    send_message(stdout, &notification);
}

// -- Handlers ------------------------------------------------------------------

/// Generates code actions based on the cached statix diagnostic output and fix suggestions.
fn handle_code_action(params: Value, documents: &HashMap<String, Document>) -> Vec<Value> {
    let mut actions = Vec::new();

    let uri = params["textDocument"]["uri"].as_str().unwrap_or("");
    let range = &params["range"];
    let req_start_line = range["start"]["line"].as_u64().unwrap_or(0) as u32;
    let req_end_line = range["end"]["line"].as_u64().unwrap_or(0) as u32;

    let doc = match documents.get(uri) {
        Some(d) => d,
        None => return actions,
    };

    // Quick fixes from the JSON output
    if let Some(output) = &doc.output {
        actions.extend(
            output
                .report
                .iter()
                .flat_map(|report| &report.diagnostics)
                .filter_map(|diag| diag.suggestion.as_ref().map(|sugg| (diag, sugg)))
                .filter_map(|(diag, sugg)| {
                    let from_line = sugg.at.from.line.saturating_sub(1);
                    let to_line = sugg.at.to.line.saturating_sub(1);

                    // Check if request range overlaps with the suggestion span
                    if req_start_line <= to_line && req_end_line >= from_line {
                        let from_col = sugg.at.from.column.saturating_sub(1);
                        let to_col = sugg.at.to.column.saturating_sub(1);

                        Some(json!({
                            "title": format!("Fix: {}", diag.message),
                            "kind": "quickfix",
                            "isPreferred": true,
                            "edit": {
                                "changes": {
                                    uri: [{
                                        "range": {
                                            "start": { "line": from_line, "character": from_col },
                                            "end": { "line": to_line, "character": to_col }
                                        },
                                        "newText": sugg.fix
                                    }]
                                }
                            }
                        }))
                    } else {
                        None
                    }
                }),
        );
    }

    actions
}

/// Called when a document is opened. Runs statix and publishes diagnostics.
fn handle_did_open(
    params: Value,
    documents: &mut HashMap<String, Document>,
    stdout: &mut impl std::io::Write,
) {
    let uri = params["textDocument"]["uri"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let text = params["textDocument"]["text"]
        .as_str()
        .unwrap_or("")
        .to_string();
    process_document_update(uri, text, documents, stdout);
}

/// Called when a document is modified. Re-runs statix and updates diagnostics.
fn handle_did_change(
    params: Value,
    documents: &mut HashMap<String, Document>,
    stdout: &mut impl std::io::Write,
) {
    let uri = params["textDocument"]["uri"]
        .as_str()
        .unwrap_or("")
        .to_string();
    if let Some(text) = params["contentChanges"][0]["text"].as_str() {
        process_document_update(uri, text.to_string(), documents, stdout);
    }
}

/// Called when a document is saved. Re-runs statix on the cached content.
fn handle_did_save(
    params: Value,
    documents: &mut HashMap<String, Document>,
    stdout: &mut impl std::io::Write,
) {
    let uri = params["textDocument"]["uri"]
        .as_str()
        .unwrap_or("")
        .to_string();
    if let Some(text) = documents.get(&uri).map(|d| d.text.clone()) {
        process_document_update(uri, text, documents, stdout);
    }
}

/// Called when a document is closed. Removes it from cache and clears diagnostics.
fn handle_did_close(
    params: Value,
    documents: &mut HashMap<String, Document>,
    stdout: &mut impl std::io::Write,
) {
    let uri = params["textDocument"]["uri"]
        .as_str()
        .unwrap_or("")
        .to_string();
    documents.remove(&uri);
    send_message(
        stdout,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": { "uri": uri, "diagnostics": [] }
        }),
    );
}

// -- Main loop -----------------------------------------------------------------

fn main() {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut stdin = io::BufReader::new(stdin.lock());
    let mut stdout = stdout.lock();

    // Track open documents: URI -> Document state
    let mut documents: HashMap<String, Document> = HashMap::new();

    while let Some(msg) = read_message(&mut stdin) {
        let req: RpcRequest = match serde_json::from_value(msg) {
            Ok(r) => r,
            Err(_) => continue,
        };

        match req.method.as_str() {
            // -- Handshake --------------------------------------------------
            "initialize" => {
                let response = RpcResponse {
                    jsonrpc: "2.0",
                    id: req.id.unwrap_or(Value::Null),
                    result: json!({
                        "capabilities": {
                            "textDocumentSync": {
                                "openClose": true,
                                "change": 1 // TextDocumentSyncKind.Full
                            },
                            "codeActionProvider": true
                        }
                    }),
                };
                send_message(&mut stdout, &serde_json::to_value(response).unwrap());
            }

            "initialized" => {}

            "shutdown" => {
                let response = RpcResponse {
                    jsonrpc: "2.0",
                    id: req.id.unwrap_or(Value::Null),
                    result: json!(null),
                };
                send_message(&mut stdout, &serde_json::to_value(response).unwrap());
            }

            "exit" => break,

            // -- Document lifecycle -----------------------------------------
            "textDocument/didOpen" => {
                if let Some(params) = req.params {
                    handle_did_open(params, &mut documents, &mut stdout);
                }
            }

            "textDocument/didChange" => {
                if let Some(params) = req.params {
                    handle_did_change(params, &mut documents, &mut stdout);
                }
            }

            "textDocument/didSave" => {
                if let Some(params) = req.params {
                    handle_did_save(params, &mut documents, &mut stdout);
                }
            }

            "textDocument/didClose" => {
                if let Some(params) = req.params {
                    handle_did_close(params, &mut documents, &mut stdout);
                }
            }

            // -- Code Actions -----------------------------------------------
            "textDocument/codeAction" => {
                let id = match req.id {
                    Some(id) => id,
                    None => continue,
                };

                let actions = req
                    .params
                    .map_or_else(Vec::new, |p| handle_code_action(p, &documents));

                send_message(
                    &mut stdout,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": actions
                    }),
                );
            }

            // Ignore everything else ($/cancelRequest, workspace/*, etc.)
            _ => {
                // We must reply to requests (messages with an id) to prevent client hangs.
                if let Some(id) = req.id {
                    send_message(
                        &mut stdout,
                        &json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": { "code": -32601, "message": "method not found" }
                        }),
                    );
                }
            }
        }
    }
}
