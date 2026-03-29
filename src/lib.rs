use zed_extension_api as zed;

const GITHUB_REPO: &str = "kennyg/zed-git-notes";
const BINARY_NAME: &str = "git-notes-lsp";

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

        // Try PATH first (for dev installs)
        if let Some(binary) = worktree.which(BINARY_NAME) {
            return Ok(zed::Command {
                command: binary,
                args: vec![],
                env: worktree.shell_env(),
            });
        }

        // Auto-download from GitHub Releases
        let (os, arch) = zed::current_platform();
        let target = match (os, arch) {
            (zed::Os::Mac, zed::Architecture::Aarch64) => "aarch64-apple-darwin",
            (zed::Os::Mac, zed::Architecture::X8664) => "x86_64-apple-darwin",
            (zed::Os::Linux, zed::Architecture::X8664) => "x86_64-unknown-linux-gnu",
            (zed::Os::Linux, zed::Architecture::Aarch64) => "aarch64-unknown-linux-gnu",
            _ => return Err("Unsupported platform".to_string()),
        };

        let release = zed::latest_github_release(
            GITHUB_REPO,
            zed::GithubReleaseOptions {
                require_assets: true,
                pre_release: false,
            },
        )
        .map_err(|e| format!("Failed to fetch release: {e}"))?;

        let asset_name = format!("{BINARY_NAME}-{target}.tar.gz");
        let asset = release
            .assets
            .iter()
            .find(|a| a.name == asset_name)
            .ok_or_else(|| format!("No release asset for {target}"))?;

        let binary_path = format!("{BINARY_NAME}-{}", release.version);

        zed::set_language_server_installation_status(
            language_server_id,
            &zed::LanguageServerInstallationStatus::Downloading,
        );

        zed::download_file(
            &asset.download_url,
            &binary_path,
            zed::DownloadedFileType::GzipTar,
        )
        .map_err(|e| format!("Failed to download: {e}"))?;

        let full_path = format!("{binary_path}/{BINARY_NAME}");
        zed::make_file_executable(&full_path)
            .map_err(|e| format!("Failed to make executable: {e}"))?;

        Ok(zed::Command {
            command: full_path,
            args: vec![],
            env: worktree.shell_env(),
        })
    }
}

zed::register_extension!(GitNotesExtension);
