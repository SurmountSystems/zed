# Reference Implementations

This directory contains Git submodules pointing to reference implementations of capabilities (this project and the project it is based on) written in other languages.

These are reference implementations in other languages as submodules, not to be used directly, but their code referenced and rewritten as Rust crates for their capability to be integrated into the Rust binary.

## Submodules
- vibe-palace (https://github.com/suykerbuyk/vibe-palace): reference for documentation capability; see "Documentation Capability Intent" below for native Zed integration notes.

See root PLAN.md and AGENTS.md (Grok Build Integration section) for the native Rust rewrite context and rules.

## Documentation Capability Intent (vibe-palace)
This reference (vibe-palace) is for building documentation capability directly into Zed's agentic workflow. It must not be exposed as an MCP. The implementation must have no Obsidian dependencies (or similar external note-taking tool ties). The goal is native, first-class documentation support usable by Zed's agent primitives (per native-first direction in AGENTS.md and the Grok Build native re-implementation goals in root PLAN.md). When porting, apply the Rewrite Guidelines strictly: tests first (as Rust unit/integration), target unpublished workspace lib crate, prefer existing workspace analogs for any crates, idiomatic/non-line-for-line.

## Memory Palace Rust Crate
Vibe-palace is the reference implementation under ref/ for persistent memory, session capture, knowledge graphs, search, and skills for AI coding assistants. Since it has no dependency on the vibe-vault codebase, we focus exclusively on rewriting and integrating its documentation and agentic capabilities as the native Rust crate memory_palace in this Zed workspace. The crate provides built-in support for these features directly in Zed's agentic workflow using GPUI, heed3 plus rkyv for storage, and follows native-first Linux priority with tests first and idiomatic non-line-for-line ports. It avoids MCP exposure and Obsidian dependencies entirely, with the original Go code used only for reference and the capability integrated into existing Zed agent crates where possible. The crate was added to the workspace members with explicit [lib] path per guidelines, and starts with minimal skeleton for further development.