# Statix LSP

Statix LSP is a language server for [Statix](https://github.com/oppiliappan/statix), a static analysis tool for Nix. It provides lints and warnings for anti-patterns in Nix code, as well as quick fixes for some of them.

## Usage

You can use the Zed extension for Statix LSP here: [https://github.com/nemeott/statix-zed](https://github.com/nemeott/statix-zed). Alternatively, you can integrate Statix LSP with any editor that supports the Language Server Protocol (LSP).

## Installation

You can either download the pre-built binary on the release page or build it from source. To install Statix LSP from source, follow these steps:

1. Clone the repository:

```bash
git clone https://github.com/nemeott/statix-lsp.git
```

2. Install it with Cargo:

```bash
cargo install --path .
```

3. Alternatively, you can build the standalone binary using Cargo:

```bash
cargo build --release
```

4. You can install the standalone binary by copying it to a directory in your PATH:

```bash
cp target/release/statix-lsp ~/.local/bin/
```

## Implementation

WARNING: This implementation is fully vibe-coded. I don't feel like trying to learn how to make an LSP server and this works well enough for my needs. Feel free to contribute a more proper implementation if you like.
