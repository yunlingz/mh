# Project instructions

## Read the development context

Read `doc/development_context.md` before making changes. It records decisions, upstream references, implementation history, and known limitations.

## Scope

- Implement a minimal native Rust port of Pi's retained behavior.
- Support only native Linux, native macOS, WSL Linux, and native Windows used from PowerShell. Other runtime environments are out of scope.
- Keep `read`, `write`, `edit`, and `bash` separate from the built-in `todo` and `ask_user_question` capabilities.
- Treat `bash` as the retained shell-command tool name, not a requirement for a Bash executable on every platform. Use native shell execution for the supported platform: Unix shell behavior on Linux/macOS/WSL and PowerShell on native Windows, without requiring Git Bash, MSYS2, Cygwin, or WSL launchers on Windows. Inspect Pi's shared shell executor and PowerShell implementation; keep necessary mh adaptations explicit.
- Enable all built-in capabilities and automatic compaction. Do not add feature toggles.
- Support only OpenAI Responses. Resolve provider endpoints and API keys from named environment variables.
- Keep configuration under `~/.mh/config/` and session-owned files under `~/.mh/sessions/`.
- Keep `/btw` tool-free, concurrent with main work, and outside the main conversation history.
- Do not add MCP, subagents, session branches, an extension loader, or unsolicited features.

## Code and safety

- Use one Cargo package with clear modules and typed state machines.
- Validate external input at its boundary. Do not duplicate checks inside typed code.
- Preserve unrelated work, credentials, and user configuration.
- Keep endpoint values, API keys, and raw provider diagnostics out of logs and errors.
- Do not execute model-generated tools in tests except inside controlled temporary fixtures.
- Keep terminal drawing separate from network, process execution, and persistence.
- Use native Ratatui borders and the complete terminal viewport without outer padding. Keep labels readable; use restrained Unicode markers and separators as requested, without requiring Nerd Fonts. Preserve user and model content.
- Before any UI design change, inspect Pi and Claude Code's actual interfaces and behavior, supported by their source/tests or documentation. Follow their established designs rather than inventing a custom layout or interaction scheme. Use the user's Pi-style input appearance: full-width top/bottom rules and a plain `>` prompt, without vertical sides, corners, or a frame title. Keep necessary Rust/platform adaptations explicit and within the retained scope.
- Retain upstream license notices for ported material.
- Do not commit, push, or install into the user's home directory without explicit authorization.

## Required checks

Run these commands from the repository root after relevant code changes:

```text
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo build --release --locked
cargo run --example generate_schemas -- --check
```

Start with focused tests, then run the complete checks. Native platform claims require checks on that platform; Linux tests do not verify Windows or macOS.

## Record results

Update `doc/development_context.md` after significant decisions or development milestones. Distinguish proposed, implemented, verified, failed, and unverified work. Include check commands, relevant files, upstream revisions, and unresolved limitations. Keep transient progress out of this file.

Keep `AGENTS.md` and development documentation machine-neutral. Do not record personal home/workspace paths, private session identifiers, deployment endpoints, actual credential environment-variable names, shell-profile contents, or other details of the user's machine/configuration. Use repository-relative paths, generic placeholders, and supported platform names. Reference upstream revisions and reproducible checks without exposing deployment data.
