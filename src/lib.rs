use zed_extension_api as zed;

struct GitNotesExtension;

impl zed::Extension for GitNotesExtension {
    fn new() -> Self {
        GitNotesExtension
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

zed::register_extension!(GitNotesExtension);
