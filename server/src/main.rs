use std::collections::HashMap;
use std::time::Instant;

use tokio::process::Command;
use tokio::sync::RwLock;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

const NOTES_CACHE_TTL_SECS: u64 = 10;
const BLAME_CACHE_TTL_SECS: u64 = 5;

struct GitNotesLsp {
    client: Client,
    notes_cache: RwLock<Option<(HashMap<String, String>, Instant)>>,
    blame_cache: RwLock<HashMap<String, (Vec<String>, Instant)>>,
}

#[derive(Debug, Clone)]
struct LineNote {
    commit: String,
    note: String,
}

impl LineNote {
    fn short_sha(&self) -> &str {
        &self.commit[..self.commit.len().min(8)]
    }
}

impl GitNotesLsp {
    fn new(client: Client) -> Self {
        Self {
            client,
            notes_cache: RwLock::new(None),
            blame_cache: RwLock::new(HashMap::new()),
        }
    }

    async fn git(cwd: &str, args: &[&str]) -> Option<String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .await
            .ok()
            .filter(|o| o.status.success())?;
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Get all git notes, cached with TTL.
    async fn get_all_notes(&self, repo_root: &str) -> HashMap<String, String> {
        // Check cache
        {
            let cache = self.notes_cache.read().await;
            if let Some((ref notes, fetched_at)) = *cache {
                if fetched_at.elapsed().as_secs() < NOTES_CACHE_TTL_SECS {
                    return notes.clone();
                }
            }
        }

        let notes = Self::fetch_all_notes(repo_root).await;

        // Update cache
        {
            let mut cache = self.notes_cache.write().await;
            *cache = Some((notes.clone(), Instant::now()));
        }

        notes
    }

    async fn fetch_all_notes(repo_root: &str) -> HashMap<String, String> {
        let mut notes = HashMap::new();

        let list = match Self::git(repo_root, &["notes", "list"]).await {
            Some(l) if !l.is_empty() => l,
            _ => return notes,
        };

        // Collect note blob SHAs and their annotated objects
        let mut blob_to_object: Vec<(String, String)> = Vec::new();
        for line in list.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                blob_to_object.push((parts[0].to_string(), parts[1].to_string()));
            }
        }

        if blob_to_object.is_empty() {
            return notes;
        }

        // Batch-read all note blobs with git cat-file --batch
        let blob_shas: Vec<&str> = blob_to_object.iter().map(|(b, _)| b.as_str()).collect();
        let input = blob_shas.join("\n");

        let output = Command::new("git")
            .arg("-C")
            .arg(repo_root)
            .args(["cat-file", "--batch"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn();

        let mut child = match output {
            Ok(c) => c,
            Err(_) => return notes,
        };

        // Write blob SHAs to stdin
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            let _ = stdin.write_all(input.as_bytes()).await;
            let _ = stdin.write_all(b"\n").await;
            drop(stdin);
        }

        let output = match child.wait_with_output().await {
            Ok(o) if o.status.success() => o,
            _ => return notes,
        };

        // Parse cat-file --batch output: each entry is:
        // <sha> <type> <size>\n<content>\n
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut blob_idx = 0;
        let mut chars = stdout.as_ref();

        while !chars.is_empty() && blob_idx < blob_to_object.len() {
            // Find the header line
            let header_end = match chars.find('\n') {
                Some(pos) => pos,
                None => break,
            };
            let header = &chars[..header_end];
            chars = &chars[header_end + 1..];

            // Parse "<sha> blob <size>"
            let header_parts: Vec<&str> = header.split_whitespace().collect();
            if header_parts.len() < 3 {
                break;
            }
            let size: usize = match header_parts[2].parse() {
                Ok(s) => s,
                Err(_) => break,
            };

            // Read exactly <size> bytes of content
            if chars.len() < size {
                break;
            }
            let content = chars[..size].trim().to_string();
            chars = &chars[size..];

            // Skip trailing newline
            if chars.starts_with('\n') {
                chars = &chars[1..];
            }

            if !content.is_empty() {
                let object = &blob_to_object[blob_idx].1;
                notes.insert(object.clone(), content);
            }
            blob_idx += 1;
        }

        notes
    }

    /// Get blame for a file, cached with TTL.
    async fn blame_file(&self, repo_root: &str, file_path: &str) -> Vec<String> {
        let cache_key = format!("{repo_root}:{file_path}");

        // Check cache
        {
            let cache = self.blame_cache.read().await;
            if let Some((ref commits, fetched_at)) = cache.get(&cache_key) {
                if fetched_at.elapsed().as_secs() < BLAME_CACHE_TTL_SECS {
                    return commits.clone();
                }
            }
        }

        let commits = Self::fetch_blame(repo_root, file_path).await;

        // Update cache
        {
            let mut cache = self.blame_cache.write().await;
            cache.insert(cache_key, (commits.clone(), Instant::now()));
        }

        commits
    }

    async fn fetch_blame(repo_root: &str, file_path: &str) -> Vec<String> {
        let blame_text = match Self::git(repo_root, &["blame", "--porcelain", "--", file_path]).await
        {
            Some(t) => t,
            None => return Vec::new(),
        };

        let mut line_commits: Vec<String> = Vec::new();
        let mut current_commit = String::new();

        for line in blame_text.lines() {
            if line.len() >= 40
                && line
                    .as_bytes()
                    .first()
                    .map_or(false, |b| b.is_ascii_hexdigit())
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

    /// Cross-reference notes with blame to find annotated lines.
    /// Returns one entry per commit (first line attributed to that commit).
    async fn get_file_line_notes(&self, repo_root: &str, file_path: &str) -> Vec<(u32, LineNote)> {
        let (all_notes, line_commits) = tokio::join!(
            self.get_all_notes(repo_root),
            self.blame_file(repo_root, file_path),
        );

        if all_notes.is_empty() {
            return Vec::new();
        }

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

    /// Look up the note for a specific line (any line, not just first-per-commit).
    async fn note_for_line(
        &self,
        repo_root: &str,
        file_path: &str,
        line: u32,
    ) -> Option<LineNote> {
        let (all_notes, line_commits) = tokio::join!(
            self.get_all_notes(repo_root),
            self.blame_file(repo_root, file_path),
        );

        let commit = line_commits.get(line as usize)?;
        let note = all_notes.get(commit.as_str())?;

        Some(LineNote {
            commit: commit.clone(),
            note: note.clone(),
        })
    }

    fn repo_root_for_uri(uri: &Url) -> Option<String> {
        let path = uri.to_file_path().ok()?;
        let dir = if path.is_file() {
            path.parent()?.to_str()?.to_string()
        } else {
            path.to_str()?.to_string()
        };

        // This one stays sync — it's needed before we can do anything else,
        // and spawning a blocking task for a single fast git call isn't worth it.
        let output = std::process::Command::new("git")
            .args(["-C", &dir, "rev-parse", "--show-toplevel"])
            .output()
            .ok()?;

        if output.status.success() {
            Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
        } else {
            None
        }
    }

    fn relative_path(uri: &Url, repo_root: &str) -> Option<String> {
        let path = uri.to_file_path().ok()?;
        let rel = path.strip_prefix(repo_root).ok()?;
        Some(rel.to_string_lossy().into_owned())
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

        let (repo_root, rel_path) = match Self::resolve_file(uri) {
            Some(r) => r,
            None => return Ok(None),
        };

        let line_notes = self.get_file_line_notes(&repo_root, &rel_path).await;
        if line_notes.is_empty() {
            return Ok(Some(Vec::new()));
        }

        let hints: Vec<InlayHint> = line_notes
            .into_iter()
            .map(|(line, note)| {
                let short = note.short_sha();
                let preview = note.note.lines().next().unwrap_or("");
                let label = if preview.len() > 60 {
                    format!(" [{short}] {}...", &preview[..57])
                } else {
                    format!(" [{short}] {preview}")
                };

                InlayHint {
                    position: Position {
                        line,
                        character: u32::MAX,
                    },
                    label: InlayHintLabel::String(label),
                    kind: None,
                    text_edits: None,
                    tooltip: Some(InlayHintTooltip::MarkupContent(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: format!(
                            "**Git Note** (`{short}`)\n\n---\n\n{}",
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

        let (repo_root, rel_path) = match Self::resolve_file(uri) {
            Some(r) => r,
            None => return Ok(None),
        };

        let note = match self.note_for_line(&repo_root, &rel_path, hover_line).await {
            Some(n) => n,
            None => return Ok(None),
        };

        let short = note.short_sha();

        let commit_info = Self::git(
            &repo_root,
            &["log", "--format=%h %an <%ae>%n%s", "-1", &note.commit],
        )
        .await
        .unwrap_or_default();

        let markdown = format!(
            "### Git Note\n\n**Commit:** `{short}`\n{}\n\n---\n\n{}",
            if commit_info.is_empty() {
                String::new()
            } else {
                format!("\n{commit_info}\n")
            },
            note.note
        );

        Ok(Some(Hover {
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
        }))
    }
}

impl GitNotesLsp {
    fn resolve_file(uri: &Url) -> Option<(String, String)> {
        let repo_root = Self::repo_root_for_uri(uri)?;
        let rel_path = Self::relative_path(uri, &repo_root)?;
        Some((repo_root, rel_path))
    }
}

#[tokio::main]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(|client| GitNotesLsp::new(client));
    Server::new(stdin, stdout, socket).serve(service).await;
}
