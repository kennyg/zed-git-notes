use std::collections::HashMap;
use std::time::Instant;

use tokio::process::Command;
use tokio::sync::RwLock;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

const NOTES_CACHE_TTL_SECS: u64 = 10;
const BLAME_CACHE_TTL_SECS: u64 = 5;
const BLAME_CACHE_MAX_ENTRIES: usize = 50;
const MAX_NOTE_BLOB_SIZE: usize = 1_048_576; // 1 MB

fn is_valid_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

// --- Pure parsing functions (no I/O, easy to test) ---

/// Parse `git notes list` output into (blob_sha, object_sha) pairs.
fn parse_notes_list(output: &str) -> Vec<(String, String)> {
    output
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                Some((parts[0].to_string(), parts[1].to_string()))
            } else {
                None
            }
        })
        .collect()
}

/// Parse `git cat-file --batch` output into a map of object_sha -> note content.
/// `blob_to_object` maps each blob SHA to the object it annotates (from parse_notes_list).
fn parse_cat_file_batch(output: &str, blob_to_object: &[(String, String)]) -> HashMap<String, String> {
    let mut notes = HashMap::new();
    let mut blob_idx = 0;
    let mut remaining = output;

    while !remaining.is_empty() && blob_idx < blob_to_object.len() {
        // Find the header line: "<sha> <type> <size>"
        let header_end = match remaining.find('\n') {
            Some(pos) => pos,
            None => break,
        };
        let header = &remaining[..header_end];
        remaining = &remaining[header_end + 1..];

        let header_parts: Vec<&str> = header.split_whitespace().collect();
        if header_parts.len() < 3 {
            break;
        }
        let size: usize = match header_parts[2].parse() {
            Ok(s) => s,
            Err(_) => break,
        };

        // Read exactly <size> bytes of content
        if remaining.len() < size {
            break;
        }
        // Skip oversized blobs to prevent memory exhaustion
        if size > MAX_NOTE_BLOB_SIZE {
            remaining = &remaining[size..];
            if remaining.starts_with('\n') {
                remaining = &remaining[1..];
            }
            blob_idx += 1;
            continue;
        }
        let content = remaining[..size].trim().to_string();
        remaining = &remaining[size..];

        // Skip trailing newline
        if remaining.starts_with('\n') {
            remaining = &remaining[1..];
        }

        if !content.is_empty() {
            let object = &blob_to_object[blob_idx].1;
            notes.insert(object.clone(), content);
        }
        blob_idx += 1;
    }

    notes
}

/// Parse `git blame --porcelain` output into a vec of commit SHAs, one per line.
/// Index 0 = line 1 of the file, etc.
fn parse_blame_porcelain(output: &str) -> Vec<String> {
    let mut line_commits: Vec<String> = Vec::new();
    let mut current_commit = String::new();

    for line in output.lines() {
        if line.len() >= 40
            && line
                .as_bytes()
                .first()
                .map_or(false, |b| b.is_ascii_hexdigit())
        {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 3 && is_valid_sha(parts[0]) {
                current_commit = parts[0].to_string();
            }
        } else if line.starts_with('\t') {
            line_commits.push(current_commit.clone());
        }
    }

    line_commits
}

/// Cross-reference notes with blame to find which lines have notes.
/// Returns one entry per unique commit (placed on the first line for that commit).
fn match_notes_to_lines(
    notes: &HashMap<String, String>,
    line_commits: &[String],
) -> Vec<(u32, LineNote)> {
    let mut result = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for (line_idx, commit) in line_commits.iter().enumerate() {
        if let Some(note) = notes.get(commit.as_str()) {
            if seen.insert(commit.clone()) {
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
        let list = match Self::git(repo_root, &["notes", "list"]).await {
            Some(l) if !l.is_empty() => l,
            _ => return HashMap::new(),
        };

        let blob_to_object = parse_notes_list(&list);
        if blob_to_object.is_empty() {
            return HashMap::new();
        }

        // Batch-read all note blobs with git cat-file --batch
        let input = blob_to_object
            .iter()
            .map(|(b, _)| b.as_str())
            .collect::<Vec<_>>()
            .join("\n");

        let child = Command::new("git")
            .arg("-C")
            .arg(repo_root)
            .args(["cat-file", "--batch"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn();

        let mut child = match child {
            Ok(c) => c,
            Err(_) => return HashMap::new(),
        };

        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            let _ = stdin.write_all(input.as_bytes()).await;
            let _ = stdin.write_all(b"\n").await;
            drop(stdin);
        }

        let output = match child.wait_with_output().await {
            Ok(o) if o.status.success() => o,
            _ => return HashMap::new(),
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        parse_cat_file_batch(&stdout, &blob_to_object)
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

        // Update cache, evicting oldest entries if over limit
        {
            let mut cache = self.blame_cache.write().await;
            if cache.len() >= BLAME_CACHE_MAX_ENTRIES {
                // Evict the oldest entry
                if let Some(oldest_key) = cache
                    .iter()
                    .min_by_key(|(_, (_, t))| *t)
                    .map(|(k, _)| k.clone())
                {
                    cache.remove(&oldest_key);
                }
            }
            cache.insert(cache_key, (commits.clone(), Instant::now()));
        }

        commits
    }

    async fn fetch_blame(repo_root: &str, file_path: &str) -> Vec<String> {
        match Self::git(repo_root, &["blame", "--porcelain", "--", file_path]).await {
            Some(text) => parse_blame_porcelain(&text),
            None => Vec::new(),
        }
    }

    /// Cross-reference notes with blame to find annotated lines.
    /// Returns one entry per commit (first line attributed to that commit).
    async fn get_file_line_notes(&self, repo_root: &str, file_path: &str) -> Vec<(u32, LineNote)> {
        let (all_notes, line_commits) = tokio::join!(
            self.get_all_notes(repo_root),
            self.blame_file(repo_root, file_path),
        );

        match_notes_to_lines(&all_notes, &line_commits)
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
        // Use parent directory lexically — don't stat the path (avoids TOCTOU)
        let dir = path.parent()?.to_str()?.to_string();

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

        if !is_valid_sha(&note.commit) {
            return Ok(None);
        }

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

// --- Tests ---
//
// In Rust, tests live inside a `#[cfg(test)]` module at the bottom of the file.
// `#[cfg(test)]` means this code is ONLY compiled when running `cargo test`,
// so it adds zero overhead to the production binary.
//
// Each test function is marked with `#[test]` (or `#[tokio::test]` for async).
// Run them with: `cargo test`
//
// Tests can access private functions because they're in the same crate.
// The `assert_eq!(actual, expected)` macro is the main way to check results.

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Unit tests for pure parsing functions ----
    // These are fast (no I/O) and test the parsing logic in isolation.

    #[test]
    fn test_parse_notes_list_basic() {
        let output = "abc123def456 1111111111111111111111111111111111111111\n\
                       fedcba987654 2222222222222222222222222222222222222222\n";

        let result = parse_notes_list(output);

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].0, "abc123def456"); // blob sha
        assert_eq!(result[0].1, "1111111111111111111111111111111111111111"); // object sha
        assert_eq!(result[1].0, "fedcba987654");
        assert_eq!(result[1].1, "2222222222222222222222222222222222222222");
    }

    #[test]
    fn test_parse_notes_list_empty() {
        assert!(parse_notes_list("").is_empty());
        assert!(parse_notes_list("   \n").is_empty());
    }

    #[test]
    fn test_parse_notes_list_malformed_lines_skipped() {
        // Lines with only one token should be skipped
        let output = "only-one-token\n\
                       abc123 def456\n";

        let result = parse_notes_list(output);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "abc123");
    }

    #[test]
    fn test_parse_cat_file_batch() {
        // Simulate git cat-file --batch output for two blobs
        let blob_to_object = vec![
            ("blob1".to_string(), "commit_aaa".to_string()),
            ("blob2".to_string(), "commit_bbb".to_string()),
        ];

        // Format: "<sha> blob <size>\n<content>\n"
        let output = "blob1 blob 13\nHello, world!\n\
                       blob2 blob 8\nBug fix\n";

        let notes = parse_cat_file_batch(output, &blob_to_object);

        assert_eq!(notes.len(), 2);
        assert_eq!(notes["commit_aaa"], "Hello, world!");
        assert_eq!(notes["commit_bbb"], "Bug fix");
    }

    #[test]
    fn test_parse_cat_file_batch_empty_content_skipped() {
        let blob_to_object = vec![("blob1".to_string(), "commit_aaa".to_string())];

        // A blob with only whitespace content
        let output = "blob1 blob 3\n   \n";

        let notes = parse_cat_file_batch(output, &blob_to_object);
        assert!(notes.is_empty()); // trimmed to empty, should be skipped
    }

    #[test]
    fn test_parse_cat_file_batch_multiline_content() {
        let blob_to_object = vec![("blob1".to_string(), "commit_aaa".to_string())];

        let content = "Line one\nLine two\nLine three";
        let output = format!("blob1 blob {}\n{}\n", content.len(), content);

        let notes = parse_cat_file_batch(&output, &blob_to_object);
        assert_eq!(notes["commit_aaa"], content);
    }

    #[test]
    fn test_parse_blame_porcelain() {
        // This is what `git blame --porcelain` actually looks like.
        // Each "group" starts with a 40-char SHA line, then metadata, then a \t-prefixed source line.
        let output = "\
aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111 1 1 3\n\
author Alice\n\
author-mail <alice@example.com>\n\
summary First commit\n\
filename app.py\n\
\timport json\n\
aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111 2 2\n\
\timport sys\n\
bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222 3 3 1\n\
author Bob\n\
author-mail <bob@example.com>\n\
summary Second commit\n\
filename app.py\n\
\tprint('hello')\n";

        let commits = parse_blame_porcelain(output);

        // 3 source lines total
        assert_eq!(commits.len(), 3);
        // Lines 1-2 come from commit aaaa...
        assert_eq!(commits[0], "aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111");
        assert_eq!(commits[1], "aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111");
        // Line 3 from commit bbbb...
        assert_eq!(commits[2], "bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222");
    }

    #[test]
    fn test_parse_blame_porcelain_empty() {
        assert!(parse_blame_porcelain("").is_empty());
    }

    #[test]
    fn test_match_notes_to_lines() {
        let mut notes = HashMap::new();
        notes.insert("commit_aaa".to_string(), "Note for A".to_string());
        // commit_bbb has no note

        let line_commits = vec![
            "commit_aaa".to_string(), // line 0
            "commit_aaa".to_string(), // line 1 (same commit, should be skipped)
            "commit_bbb".to_string(), // line 2 (no note)
            "commit_aaa".to_string(), // line 3 (already seen)
        ];

        let result = match_notes_to_lines(&notes, &line_commits);

        // Should only return one entry: line 0 for commit_aaa
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, 0); // line number
        assert_eq!(result[0].1.commit, "commit_aaa");
        assert_eq!(result[0].1.note, "Note for A");
    }

    #[test]
    fn test_match_notes_to_lines_multiple_commits_with_notes() {
        let mut notes = HashMap::new();
        notes.insert("commit_aaa".to_string(), "Note A".to_string());
        notes.insert("commit_bbb".to_string(), "Note B".to_string());

        let line_commits = vec![
            "commit_aaa".to_string(), // line 0
            "commit_bbb".to_string(), // line 1
            "commit_aaa".to_string(), // line 2 (duplicate)
        ];

        let result = match_notes_to_lines(&notes, &line_commits);

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].0, 0);
        assert_eq!(result[0].1.note, "Note A");
        assert_eq!(result[1].0, 1);
        assert_eq!(result[1].1.note, "Note B");
    }

    #[test]
    fn test_match_notes_to_lines_no_notes() {
        let notes = HashMap::new(); // empty
        let line_commits = vec!["commit_aaa".to_string()];

        let result = match_notes_to_lines(&notes, &line_commits);
        assert!(result.is_empty());
    }

    // ---- Tests for LineNote ----

    #[test]
    fn test_short_sha() {
        let note = LineNote {
            commit: "abcdef1234567890abcdef1234567890abcdef12".to_string(),
            note: "test".to_string(),
        };
        assert_eq!(note.short_sha(), "abcdef12");
    }

    #[test]
    fn test_short_sha_short_commit() {
        // Edge case: commit string shorter than 8 chars
        let note = LineNote {
            commit: "abc".to_string(),
            note: "test".to_string(),
        };
        assert_eq!(note.short_sha(), "abc");
    }

    // ---- Tests for relative_path ----

    #[test]
    fn test_relative_path() {
        let uri = Url::parse("file:///home/user/project/src/main.rs").unwrap();
        let result = GitNotesLsp::relative_path(&uri, "/home/user/project");
        assert_eq!(result.as_deref(), Some("src/main.rs"));
    }

    #[test]
    fn test_relative_path_root_file() {
        let uri = Url::parse("file:///home/user/project/README.md").unwrap();
        let result = GitNotesLsp::relative_path(&uri, "/home/user/project");
        assert_eq!(result.as_deref(), Some("README.md"));
    }

    #[test]
    fn test_relative_path_prefix_overlap_bug() {
        // This was a bug with the old string-slicing approach:
        // "/foo/bar" is a string prefix of "/foo/barmain.rs"
        // Path::strip_prefix correctly rejects this since "barmain.rs" != "bar/main.rs"
        let uri = Url::parse("file:///foo/barmain.rs").unwrap();
        let result = GitNotesLsp::relative_path(&uri, "/foo/bar");
        assert_eq!(result, None); // should NOT match
    }

    // ---- Integration test: full pipeline against a real git repo ----
    // These tests create a temporary git repo, add commits and notes,
    // then verify the LSP logic works end-to-end.

    #[tokio::test]
    async fn test_full_pipeline_with_real_git() {
        // Create a temp directory for our test repo
        let tmp_dir = std::env::temp_dir().join(format!("git-notes-test-{}", std::process::id()));
        let tmp = tmp_dir.to_str().unwrap();

        // Helper to run git commands in the test repo
        async fn git(dir: &str, args: &[&str]) -> String {
            let output = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@test.com")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@test.com")
                .output()
                .await
                .expect("git command failed");
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }

        // Set up: init repo, create a file, commit, add a note
        std::fs::create_dir_all(tmp).unwrap();
        git(tmp, &["init"]).await;
        std::fs::write(tmp_dir.join("hello.txt"), "line one\nline two\n").unwrap();
        git(tmp, &["add", "hello.txt"]).await;
        git(tmp, &["commit", "-m", "initial"]).await;

        let commit_sha = git(tmp, &["rev-parse", "HEAD"]).await;
        git(tmp, &["notes", "add", "-m", "This is a test note", &commit_sha]).await;

        // Test fetch_all_notes
        let notes = GitNotesLsp::fetch_all_notes(tmp).await;
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[&commit_sha], "This is a test note");

        // Test fetch_blame
        let blame = GitNotesLsp::fetch_blame(tmp, "hello.txt").await;
        assert_eq!(blame.len(), 2); // two lines in the file
        assert_eq!(blame[0], commit_sha);
        assert_eq!(blame[1], commit_sha);

        // Test the full cross-reference
        let matched = match_notes_to_lines(&notes, &blame);
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].0, 0); // note on line 0 (first occurrence)
        assert_eq!(matched[0].1.note, "This is a test note");

        // Clean up
        std::fs::remove_dir_all(tmp).unwrap();
    }

    // ---- Security validation tests ----

    #[test]
    fn test_is_valid_sha() {
        assert!(is_valid_sha("abcdef1234567890abcdef1234567890abcdef12"));
        assert!(is_valid_sha("0000000000000000000000000000000000000000"));
        assert!(is_valid_sha("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
    }

    #[test]
    fn test_is_valid_sha_rejects_bad_input() {
        // Too short
        assert!(!is_valid_sha("abcdef12"));
        // Too long
        assert!(!is_valid_sha("abcdef1234567890abcdef1234567890abcdef12aa"));
        // Non-hex characters
        assert!(!is_valid_sha("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"));
        // Uppercase is valid (git can output mixed case in some contexts)
        assert!(is_valid_sha("ABCDEF1234567890ABCDEF1234567890ABCDEF12"));
        // Git flag injection attempt
        assert!(!is_valid_sha("--upload-pack=evil_command_here_padding"));
        // Empty
        assert!(!is_valid_sha(""));
    }

    #[test]
    fn test_blame_parser_rejects_invalid_sha() {
        // A line that starts with hex but isn't a valid 40-char SHA should be skipped
        let output = "\
--upload-pack=evil 1 1 1\n\
\tline content\n";

        let commits = parse_blame_porcelain(output);
        // Should produce a line entry but with empty commit (no valid SHA parsed)
        assert!(commits.is_empty() || commits.iter().all(|c| c.is_empty() || is_valid_sha(c)));
    }

    #[test]
    fn test_cat_file_batch_skips_oversized_blob() {
        let blob_to_object = vec![
            ("blob1".to_string(), "commit_aaa".to_string()),
            ("blob2".to_string(), "commit_bbb".to_string()),
        ];

        // First blob exceeds MAX_NOTE_BLOB_SIZE, second is normal
        let big_size = MAX_NOTE_BLOB_SIZE + 100;
        let big_content = "x".repeat(big_size);
        let output = format!(
            "blob1 blob {big_size}\n{big_content}\nblob2 blob 4\ntest\n"
        );

        let notes = parse_cat_file_batch(&output, &blob_to_object);

        // Oversized blob should be skipped, normal one included
        assert!(!notes.contains_key("commit_aaa"));
        assert_eq!(notes.get("commit_bbb").map(|s| s.as_str()), Some("test"));
    }
}
