# Git Notes for Zed

A Zed extension that surfaces [git notes](https://git-scm.com/docs/git-notes) inline in the editor. Works with notes created by [gh-annotate](https://github.com/kennyg/gh-annotate) or any standard `git notes` workflow.

## What it does

When a commit has a git note attached, a lightning bolt indicator appears on the first line attributed to that commit. Hovering over it shows the full note content with commit details.

## Install

### 1. Install the Zed extension

In Zed: **Extensions > Install Dev Extension** and select this directory.

The LSP server binary (`git-notes-lsp`) is automatically downloaded from GitHub Releases on first use. If you prefer to build from source:

```sh
cargo install --path server
```

### 2. Enable inlay hints in Zed settings

Add this to your Zed `settings.json` (open with `cmd+,`):

```json
"inlay_hints": {
  "enabled": true,
  "show_other_hints": true
}
```

`show_other_hints` is required — without it, the git note indicators won't appear.

## How it works

1. The extension registers `git-notes-lsp` as a language server for 50+ file types
2. When you open a file, the LSP runs `git notes list` to find all notes in the repo
3. It runs `git blame` on the file to map lines to commits
4. Lines from commits with notes get an inlay hint (lightning bolt indicator)
5. Hovering shows the full note in a popup with commit info

Results are cached (notes: 10s, blame: 5s) so scrolling and hovering are fast.

## Creating notes

```sh
# Add a note to the current commit
git notes add -m "Design decision: chose X over Y because..."

# Add a note to a specific commit
git notes add -m "Bug fix context: see issue #42" abc1234

# Or use gh-annotate for structured annotations
gh annotate
```

## Development

```
zed-git-notes/
├── extension.toml       # Zed extension manifest
├── Cargo.toml           # Extension crate (compiles to WASM)
├── src/lib.rs           # Extension: auto-downloads + launches LSP
├── server/
│   ├── Cargo.toml       # LSP server crate (native binary)
│   └── src/main.rs      # LSP: inlay hints + hover via git blame
└── LICENSE
```

Run tests: `cd server && cargo test`
