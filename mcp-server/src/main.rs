use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::process::Command;

fn main() {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut stdout = stdout.lock();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };

        // MCP uses JSON-RPC over newline-delimited JSON on stdio
        let request: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let method = request["method"].as_str().unwrap_or("");
        let id = request.get("id").cloned();
        let params = request.get("params").cloned().unwrap_or(json!({}));

        let response = match method {
            "initialize" => handle_initialize(id),
            "notifications/initialized" => continue, // no response needed
            "tools/list" => handle_tools_list(id),
            "tools/call" => handle_tools_call(id, &params),
            "ping" => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
            _ => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("Method not found: {method}") }
            }),
        };

        let msg = serde_json::to_string(&response).unwrap();
        let _ = writeln!(stdout, "{msg}");
        let _ = stdout.flush();
    }
}

fn handle_initialize(id: Option<Value>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "tools": {}
            },
            "serverInfo": {
                "name": "git-notes-mcp",
                "version": "0.0.1"
            }
        }
    })
}

fn handle_tools_list(id: Option<Value>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "tools": [
                {
                    "name": "git_notes_list",
                    "description": "List all git notes in the repository. Returns each note with its commit SHA, commit subject, and note content.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "repo_path": {
                                "type": "string",
                                "description": "Path to the git repository (defaults to current directory)"
                            }
                        }
                    }
                },
                {
                    "name": "git_notes_for_file",
                    "description": "Show git notes for commits that touched a specific file. Uses git blame to map file lines to commits, then shows any notes attached to those commits.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "repo_path": {
                                "type": "string",
                                "description": "Path to the git repository"
                            },
                            "file_path": {
                                "type": "string",
                                "description": "Path to the file (relative to repo root)"
                            }
                        },
                        "required": ["file_path"]
                    }
                },
                {
                    "name": "git_notes_show",
                    "description": "Show the git note attached to a specific commit.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "repo_path": {
                                "type": "string",
                                "description": "Path to the git repository"
                            },
                            "commit": {
                                "type": "string",
                                "description": "Commit SHA or ref to show the note for"
                            }
                        },
                        "required": ["commit"]
                    }
                }
            ]
        }
    })
}

fn handle_tools_call(id: Option<Value>, params: &Value) -> Value {
    let tool_name = params["name"].as_str().unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    let repo_path = args["repo_path"].as_str().unwrap_or(".");

    let result = match tool_name {
        "git_notes_list" => git_notes_list(repo_path),
        "git_notes_for_file" => {
            let file_path = args["file_path"].as_str().unwrap_or("");
            git_notes_for_file(repo_path, file_path)
        }
        "git_notes_show" => {
            let commit = args["commit"].as_str().unwrap_or("");
            git_notes_show(repo_path, commit)
        }
        _ => Err(format!("Unknown tool: {tool_name}")),
    };

    match result {
        Ok(text) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "content": [{ "type": "text", "text": text }]
            }
        }),
        Err(e) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "content": [{ "type": "text", "text": format!("Error: {e}") }],
                "isError": true
            }
        }),
    }
}

fn git_notes_list(repo_path: &str) -> Result<String, String> {
    let output = Command::new("git")
        .args(["-C", repo_path, "notes", "list"])
        .output()
        .map_err(|e| format!("Failed to run git: {e}"))?;

    if !output.status.success() || output.stdout.is_empty() {
        return Ok("No git notes found in this repository.".to_string());
    }

    let list = String::from_utf8_lossy(&output.stdout);
    let mut result = String::new();

    for line in list.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 2 {
            continue;
        }
        let object = parts[1];
        // Get note content
        let note = run_git(repo_path, &["notes", "show", object]).unwrap_or_default();
        // Get commit subject
        let subject = run_git(repo_path, &["log", "--format=%h %an: %s", "-1", object])
            .unwrap_or_default();

        result.push_str(&format!("### {subject}\n\n{}\n\n---\n\n", note.trim()));
    }

    if result.is_empty() {
        result = "No git notes found.".to_string();
    }

    Ok(result)
}

fn git_notes_for_file(repo_path: &str, file_path: &str) -> Result<String, String> {
    let blame_out = Command::new("git")
        .args(["-C", repo_path, "blame", "--porcelain", "--", file_path])
        .output()
        .map_err(|e| format!("Failed to run git blame: {e}"))?;

    if !blame_out.status.success() {
        return Err(format!(
            "git blame failed: {}",
            String::from_utf8_lossy(&blame_out.stderr)
        ));
    }

    let blame = String::from_utf8_lossy(&blame_out.stdout);
    let mut commits: Vec<String> = Vec::new();

    for line in blame.lines() {
        if line.len() >= 40 && line.chars().next().map_or(false, |c| c.is_ascii_hexdigit()) {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 3 {
                let sha = parts[0].to_string();
                if !commits.contains(&sha)
                    && sha != "0000000000000000000000000000000000000000"
                {
                    commits.push(sha);
                }
            }
        }
    }

    let mut result = String::new();

    for commit in &commits {
        let note = match run_git(repo_path, &["notes", "show", commit]) {
            Some(n) => n,
            None => continue,
        };
        let subject = run_git(repo_path, &["log", "--format=%h %an: %s", "-1", commit])
            .unwrap_or_default();

        result.push_str(&format!("### {subject}\n\n{}\n\n---\n\n", note.trim()));
    }

    if result.is_empty() {
        Ok(format!(
            "No git notes found for commits touching {file_path}."
        ))
    } else {
        Ok(result)
    }
}

fn git_notes_show(repo_path: &str, commit: &str) -> Result<String, String> {
    let note = run_git(repo_path, &["notes", "show", commit])
        .ok_or_else(|| format!("No note found for {commit}"))?;

    let subject = run_git(repo_path, &["log", "--format=%h %an <%ae>%n%s", "-1", commit])
        .unwrap_or_default();

    Ok(format!("**Commit:** {subject}\n\n---\n\n{}", note.trim()))
}

fn run_git(repo_path: &str, args: &[&str]) -> Option<String> {
    let mut cmd_args = vec!["-C", repo_path];
    cmd_args.extend_from_slice(args);

    Command::new("git")
        .args(&cmd_args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
}
