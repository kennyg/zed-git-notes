use std::collections::HashMap;
use std::process::Command as ProcessCommand;

use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

#[derive(Debug)]
struct GitNotesLsp {
    client: Client,
}

#[derive(Debug, Clone)]
struct LineNote {
    commit: String,
    note: String,
}

impl GitNotesLsp {
    fn new(client: Client) -> Self {
        Self { client }
    }

    /// Get all git notes as a map of commit SHA -> note content.
    fn get_all_notes(&self, repo_root: &str) -> HashMap<String, String> {
        let mut notes = HashMap::new();

        let output = ProcessCommand::new("git")
            .args(["-C", repo_root, "notes", "--list"])
            .output();

        let output = match output {
            Ok(o) if o.status.success() => o,
            _ => return notes,
        };

        let list = String::from_utf8_lossy(&output.stdout);
        for line in list.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 2 {
                continue;
            }
            let object = parts[1];

            if let Ok(note_out) = ProcessCommand::new("git")
                .args(["-C", repo_root, "notes", "show", object])
                .output()
            {
                if note_out.status.success() {
                    let content = String::from_utf8_lossy(&note_out.stdout).trim().to_string();
                    if !content.is_empty() {
                        notes.insert(object.to_string(), content);
                    }
                }
            }
        }

        notes
    }

    /// Run git blame on a file to map lines to commits.
    /// Returns a vec indexed by 0-based line number -> commit SHA.
    fn blame_file(&self, repo_root: &str, file_path: &str) -> Vec<String> {
        let output = ProcessCommand::new("git")
            .args(["-C", repo_root, "blame", "--porcelain", "--", file_path])
            .output();

        let output = match output {
            Ok(o) if o.status.success() => o,
            _ => return Vec::new(),
        };

        let blame_text = String::from_utf8_lossy(&output.stdout);
        let mut line_commits: Vec<String> = Vec::new();
        let mut current_commit = String::new();

        for line in blame_text.lines() {
            if line.len() >= 40
                && line
                    .chars()
                    .next()
                    .map_or(false, |c| c.is_ascii_hexdigit())
            {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 3 {
                    current_commit = parts[0].to_string();
                }
            } else if line.starts_with('\t') {
                line_commits.push(current_commit.clone());
            }
        }

        line_commits
    }

    /// Get notes for lines of a specific file.
    fn get_file_line_notes(&self, repo_root: &str, file_path: &str) -> Vec<(u32, LineNote)> {
        let all_notes = self.get_all_notes(repo_root);
        if all_notes.is_empty() {
            return Vec::new();
        }

        let line_commits = self.blame_file(repo_root, file_path);
        let mut result = Vec::new();
        let mut seen_commits = std::collections::HashSet::new();

        for (line_idx, commit) in line_commits.iter().enumerate() {
            if let Some(note) = all_notes.get(commit.as_str()) {
                if seen_commits.insert(commit.clone()) {
                    result.push((
                        line_idx as u32,
                        LineNote {
                            commit: commit.clone(),
                            note: note.clone(),
                        },
                    ));
                }
            }
        }

        result
    }

    fn repo_root_for_uri(&self, uri: &Url) -> Option<String> {
        let path = uri.to_file_path().ok()?;
        let dir = if path.is_file() {
            path.parent()?.to_str()?.to_string()
        } else {
            path.to_str()?.to_string()
        };

        let output = ProcessCommand::new("git")
            .args(["-C", &dir, "rev-parse", "--show-toplevel"])
            .output()
            .ok()?;

        if output.status.success() {
            Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
        } else {
            None
        }
    }

    fn relative_path(&self, uri: &Url, repo_root: &str) -> Option<String> {
        let path = uri.to_file_path().ok()?;
        let path_str = path.to_str()?;
        if path_str.starts_with(repo_root) {
            Some(
                path_str[repo_root.len()..]
                    .trim_start_matches('/')
                    .to_string(),
            )
        } else {
            Some(path_str.to_string())
        }
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for GitNotesLsp {
    async fn initialize(&self, _params: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                inlay_hint_provider: Some(OneOf::Left(true)),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn initialized(&self, _params: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "git-notes-lsp initialized")
            .await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn inlay_hint(&self, params: InlayHintParams) -> Result<Option<Vec<InlayHint>>> {
        let uri = &params.text_document.uri;

        let repo_root = match self.repo_root_for_uri(uri) {
            Some(r) => r,
            None => return Ok(None),
        };

        let rel_path = match self.relative_path(uri, &repo_root) {
            Some(p) => p,
            None => return Ok(None),
        };

        let line_notes = self.get_file_line_notes(&repo_root, &rel_path);
        if line_notes.is_empty() {
            return Ok(Some(Vec::new()));
        }

        let hints: Vec<InlayHint> = line_notes
            .into_iter()
            .map(|(line, note)| {
                let short_commit = &note.commit[..note.commit.len().min(8)];
                let preview = note.note.lines().next().unwrap_or("").to_string();
                let label = if preview.len() > 60 {
                    format!(" [{short_commit}] {}...", &preview[..57])
                } else {
                    format!(" [{short_commit}] {preview}")
                };

                InlayHint {
                    position: Position {
                        line,
                        character: u32::MAX,
                    },
                    label: InlayHintLabel::String(label),
                    kind: None, // No specific kind — it's a note annotation
                    text_edits: None,
                    tooltip: Some(InlayHintTooltip::MarkupContent(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: format!(
                            "**Git Note** (`{short_commit}`)\n\n---\n\n{}",
                            note.note
                        ),
                    })),
                    padding_left: Some(true),
                    padding_right: None,
                    data: None,
                }
            })
            .collect();

        Ok(Some(hints))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let hover_line = params.text_document_position_params.position.line;

        let repo_root = match self.repo_root_for_uri(uri) {
            Some(r) => r,
            None => return Ok(None),
        };

        let rel_path = match self.relative_path(uri, &repo_root) {
            Some(p) => p,
            None => return Ok(None),
        };

        let all_notes = self.get_all_notes(&repo_root);
        let line_commits = self.blame_file(&repo_root, &rel_path);

        if let Some(commit) = line_commits.get(hover_line as usize) {
            if let Some(note_content) = all_notes.get(commit.as_str()) {
                let short = &commit[..commit.len().min(8)];

                let commit_info = ProcessCommand::new("git")
                    .args([
                        "-C",
                        &repo_root,
                        "log",
                        "--format=%h %an <%ae>%n%s",
                        "-1",
                        commit,
                    ])
                    .output()
                    .ok()
                    .filter(|o| o.status.success())
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                    .unwrap_or_default();

                let markdown = format!(
                    "### Git Note\n\n**Commit:** `{short}`\n{}\n\n---\n\n{}",
                    if commit_info.is_empty() {
                        String::new()
                    } else {
                        format!("\n{commit_info}\n")
                    },
                    note_content
                );

                return Ok(Some(Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: markdown,
                    }),
                    range: Some(Range {
                        start: Position {
                            line: hover_line,
                            character: 0,
                        },
                        end: Position {
                            line: hover_line,
                            character: u32::MAX,
                        },
                    }),
                }));
            }
        }

        Ok(None)
    }
}

#[tokio::main]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(|client| GitNotesLsp::new(client));
    Server::new(stdin, stdout, socket).serve(service).await;
}
