use zed_extension_api as zed;

struct GitNotesExtension;

impl zed::Extension for GitNotesExtension {
    fn new() -> Self {
        GitNotesExtension
    }

    fn complete_slash_command_argument(
        &self,
        command: zed::SlashCommand,
        _args: Vec<String>,
    ) -> Result<Vec<zed::SlashCommandArgumentCompletion>, String> {
        if command.name != "git-notes" {
            return Ok(Vec::new());
        }

        Ok(vec![
            zed::SlashCommandArgumentCompletion {
                label: "file".to_string(),
                new_text: "file".to_string(),
                run_command: true,
            },
            zed::SlashCommandArgumentCompletion {
                label: "all".to_string(),
                new_text: "all".to_string(),
                run_command: true,
            },
        ])
    }

    fn run_slash_command(
        &self,
        command: zed::SlashCommand,
        args: Vec<String>,
        worktree: Option<&zed::Worktree>,
    ) -> Result<zed::SlashCommandOutput, String> {
        if command.name != "git-notes" {
            return Err(format!("Unknown command: {}", command.name));
        }

        let worktree = worktree.ok_or("No worktree available")?;
        let root = worktree.root_path();

        let mode = args.first().map(|s| s.as_str()).unwrap_or("file");

        match mode {
            "all" => self.show_all_notes(&root),
            _ => self.show_file_notes(&root, mode),
        }
    }

    fn language_server_command(
        &mut self,
        language_server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> zed::Result<zed::Command> {
        if language_server_id.as_ref() != "git-notes-lsp" {
            return Err(format!("Unknown language server: {language_server_id}"));
        }

        let binary = worktree
            .which("git-notes-lsp")
            .ok_or("git-notes-lsp binary not found on PATH. Install with: cargo install --path server")?;

        Ok(zed::Command {
            command: binary,
            args: vec![],
            env: worktree.shell_env(),
        })
    }
}

impl GitNotesExtension {
    fn show_all_notes(&self, root: &str) -> Result<zed::SlashCommandOutput, String> {
        // List all notes refs
        let refs_output = zed::Command::new("git")
            .args(["-C", root, "notes", "--list"])
            .output()
            .map_err(|e| format!("Failed to list notes: {e}"))?;

        if refs_output.status != Some(0) {
            let stderr = String::from_utf8_lossy(&refs_output.stderr);
            if stderr.contains("No notes found") || refs_output.stdout.is_empty() {
                return Ok(zed::SlashCommandOutput {
                    text: "No git notes found in this repository.".to_string(),
                    sections: vec![zed::SlashCommandOutputSection {
                        range: (0..43u32).into(),
                        label: "Git Notes".to_string(),
                    }],
                });
            }
            return Err(format!("git notes --list failed: {stderr}"));
        }

        let list = String::from_utf8_lossy(&refs_output.stdout);
        let mut text = String::new();
        let mut sections = Vec::new();

        for line in list.lines() {
            // Each line is: <note-blob> <annotated-object>
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 2 {
                continue;
            }
            let object = parts[1];
            let short = &object[..object.len().min(8)];

            // Get the note content
            let note_output = zed::Command::new("git")
                .args(["-C", root, "notes", "show", object])
                .output()
                .map_err(|e| format!("Failed to show note for {short}: {e}"))?;

            if note_output.status != Some(0) {
                continue;
            }

            let note = String::from_utf8_lossy(&note_output.stdout);

            // Get commit summary
            let log_output = zed::Command::new("git")
                .args(["-C", root, "log", "--format=%s", "-1", object])
                .output();

            let subject = log_output
                .ok()
                .filter(|o| o.status == Some(0))
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_default();

            let section_start = text.len() as u32;

            let header = if subject.is_empty() {
                format!("## {short}\n\n")
            } else {
                format!("## {short} — {subject}\n\n")
            };
            text.push_str(&header);
            text.push_str(note.trim());
            text.push_str("\n\n---\n\n");

            let section_end = text.len() as u32;
            sections.push(zed::SlashCommandOutputSection {
                range: (section_start..section_end).into(),
                label: format!("Note: {short}"),
            });
        }

        if text.is_empty() {
            text = "No git notes found in this repository.".to_string();
            sections.push(zed::SlashCommandOutputSection {
                range: (0..text.len() as u32).into(),
                label: "Git Notes".to_string(),
            });
        }

        Ok(zed::SlashCommandOutput { text, sections })
    }

    fn show_file_notes(&self, root: &str, file_path: &str) -> Result<zed::SlashCommandOutput, String> {
        // Get blame for the file to map lines -> commits
        let blame_output = zed::Command::new("git")
            .args(["-C", root, "blame", "--porcelain", file_path])
            .output()
            .map_err(|e| format!("Failed to blame {file_path}: {e}"))?;

        if blame_output.status != Some(0) {
            let stderr = String::from_utf8_lossy(&blame_output.stderr);
            return Err(format!("git blame failed: {stderr}"));
        }

        let blame = String::from_utf8_lossy(&blame_output.stdout);

        // Extract unique commits from blame
        let mut commits: Vec<String> = Vec::new();
        for line in blame.lines() {
            if line.len() >= 40 && line.chars().next().map_or(false, |c| c.is_ascii_hexdigit()) {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 3 {
                    let sha = parts[0].to_string();
                    if !commits.contains(&sha) && sha != "0000000000000000000000000000000000000000" {
                        commits.push(sha);
                    }
                }
            }
        }

        let mut text = String::new();
        let mut sections = Vec::new();
        let mut found_any = false;

        for commit in &commits {
            let note_output = zed::Command::new("git")
                .args(["-C", root, "notes", "show", commit])
                .output();

            let note_output = match note_output {
                Ok(o) if o.status == Some(0) => o,
                _ => continue,
            };

            found_any = true;
            let note = String::from_utf8_lossy(&note_output.stdout);
            let short = &commit[..commit.len().min(8)];

            let section_start = text.len() as u32;

            text.push_str(&format!("## Commit {short}\n\n"));
            text.push_str(note.trim());
            text.push_str("\n\n---\n\n");

            let section_end = text.len() as u32;
            sections.push(zed::SlashCommandOutputSection {
                range: (section_start..section_end).into(),
                label: format!("Note: {short}"),
            });
        }

        if !found_any {
            text = format!("No git notes found for commits touching {file_path}.");
            sections.push(zed::SlashCommandOutputSection {
                range: (0..text.len() as u32).into(),
                label: "Git Notes".to_string(),
            });
        }

        Ok(zed::SlashCommandOutput { text, sections })
    }
}

zed::register_extension!(GitNotesExtension);
