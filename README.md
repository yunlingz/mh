# mh

`mh` is a minimal native Rust coding harness based on Pi's retained agent behavior. It uses OpenAI Responses over HTTP Server-Sent Events, provides a fullscreen Ratatui interface and a clean print mode, and has no Node.js runtime dependency.

## Build

Rust 1.89 or newer is required.

```bash
cargo build --release --locked
```

The executable is `target/release/mh` (`target/release/mh.exe` on native Windows). The Windows build target is MSVC and the supported native command environment is PowerShell. Building does not install configuration or write into `~/.mh`.

## Configuration

Initialize the user configuration directory with:

```bash
mh --init
```

This creates `config.json` and `theme.json` containing only their relative `$schema` references, plus the generated `config.schema.json` and `theme.schema.json`. Existing editable JSON files are preserved. The two generated schema files are authoritative distribution artifacts and are replaced with the compiled standard versions on every normal `mh` startup and every `--init` run.

The initial `theme.json` is already a valid empty theme file. The initial `config.json` is intentionally incomplete: add a provider and default model before starting the harness. Every normal startup validates both editable files against the embedded standard JSON Schemas and then applies typed semantic validation before creating or resuming a session.

Endpoint and API-key values are never stored in `config.json`; it stores the names of environment variables containing them.

```json
{
  "$schema": "config.schema.json",
  "defaultModel": "work/deep",
  "providers": {
    "work": {
      "api": "openai-responses-sse",
      "base_url_env": "PROVIDER_BASE_URL",
      "api_key_env": "PROVIDER_API_KEY",
      "models": [
        {
          "id": "your-model-id",
          "alias": "deep",
          "input": ["text", "image"],
          "reasoning": true,
          "reasoningEffort": "high",
          "contextWindow": 128000,
          "maxTokens": 16384,
          "compaction": {
            "reserveTokens": 16384,
            "keepRecentTokens": 20000
          }
        }
      ]
    }
  }
}
```

`base_url_env` contains a base URL. `mh` removes a trailing slash and appends `/responses`. For example, `https://example.invalid/v1/` becomes `https://example.invalid/v1/responses`.

Each selectable reasoning entry has one fixed `reasoningEffort`, passed directly to `reasoning.effort`. Use `"none"` for an explicit no-effort request; JSON `null` or omission leaves the provider default unchanged. Non-reasoning entries omit `reasoningEffort`. There is no active level map or independent runtime effort setting.

Deprecated `thinkingLevel` / `thinkingLevelMap` input is normalized in memory for existing configurations; it is not written back to user files. New configuration and generated schemas use `reasoningEffort` only. Mixing canonical and legacy effort fields is rejected rather than guessing.

The optional `alias` is the local selection name and defaults to `id`. Aliases must be unique within a provider; multiple entries may share the same wire `id` with different fixed efforts. Select them as `provider/alias` through `--model` or `/model`. Existing configs without aliases remain valid. Old saved effort overrides are normalized to configured entries with a notice, without rewriting history or user configuration. See `examples/config.json` for same-wire fast/deep entries.

A custom palette can be placed in `~/.mh/config/theme.json`. The built-in default is `ci-dark`; custom themes override its semantic colors. On Windows, the conventional `USERPROFILE` environment variable determines the home directory, with the native known-folder location used as a fallback.

## Running

With terminal input and output, `mh` opens its fullscreen interface:

```bash
mh
mh "Inspect the current project"
mh --continue
mh --resume
mh -r
mh --resume 550e8400-e29b-41d4-a716-446655440000
mh --session 550e8400-e29b-41d4-a716-446655440000
```

If `--print` is present, or either standard input or standard output is redirected, `mh` uses print mode:

```bash
mh --print "Summarize this project"
printf 'Review this input' | mh --print
```

Print mode writes only the final visible assistant text to stdout. Diagnostics go to stderr. Exit status is:

- `0`: completed response, or a normal broken pipe
- `1`: provider or runtime failure
- `2`: output-length truncation
- `130`: cancellation

Piped stdin is read to EOF and concatenated with the positional prompt. Print mode never advertises `ask_user_question` because no human-input UI exists.

## Fullscreen controls

The main screen fills the terminal viewport with no outer gutters. The input follows Pi's style: a plain `>` prompt between full-width native horizontal borders, without vertical sides, corners, or a frame title. Input soft-wraps at Unicode cell/word boundaries and grows to a bounded height. Large pastes (over 3 lines or 800 characters) become atomic chips; pasting the same text again expands it. Full content is submitted, not the chip label. Clipboard images appear as positional `[Image #N]` chips and are sent as actual image input. Unsupported models reject image submissions without clearing the draft.

Startup shows the version, supported shortcuts, configured model scope, actual loaded Context/Skills, warnings, and redacted proxy status. `Ctrl+O` opens the detailed transcript and full startup help. The footer keeps the model entry (including its fixed effort), path, current context, and cumulative billing fields in order; current context uses `used/limit (percentage)`, such as `0/1.1M (0.0%)`, with `~` for estimates.

Active work shows elapsed time and distinguishes waiting without visible output from streamed tool preparation. Main, compaction, and `/btw` requests automatically retry transient failures up to three times, with 2/4/8-second delays or bounded provider delay hints (at most 60 seconds). Retry countdowns remain visible, including inside the `/btw` popup. Main backoff is cancellable with Escape; the side popup uses Ctrl+C for independent cancellation. Quota/account failures are not retried. Every provider attempt has a 15-minute end-to-end limit, including waits to deliver UI updates; reaching that limit retains partial output and ends the attempt without automatically starting another full-length wait.

- `Enter` / `Ctrl+M`: submit ordinary input or a slash command
- `Shift+Enter` / `Ctrl+J`: insert a newline without submitting or advancing a field
- `Tab`: command/skill/model/file completion; arrows select, Escape dismisses
- `Up` / `Down`: move within visual input rows, then recall prompt history at its boundaries
- `Ctrl+U` / `Ctrl+K` / `Ctrl+W`: delete to line start/end or the previous word; `Ctrl+Y` restores deleted text
- `Ctrl+Z`: undo/recover one input change; Ctrl+R redo is removed, and Ctrl+Z does not suspend the fullscreen app
- `Ctrl+V` / `Alt+V`: paste an image or text through the OS clipboard reader
- `Alt+Enter`: queue a follow-up while work is active
- `Alt+Q` / `Alt+Up`: restore pending messages and the current draft for editing/requeueing without aborting main work. Pi defaults to Alt+Q on Windows/WSL and Alt+Up on native Linux/macOS; mh accepts both.
- `Ctrl+P` / `Ctrl+Shift+P`: cycle configured fixed-effort model entries
- `Ctrl+O`: open/close the detailed transcript and startup help; arrows scroll there, Escape returns to input
- `Ctrl+Up` / `Ctrl+Down`: jump between transcript messages
- `Ctrl+T`: expand or collapse the bounded todo panel
- `Esc`: cancel active main work; active modal dialogs handle Escape first
- `PageUp` / `PageDown`: scroll the transcript
- `Ctrl+Home` / `Ctrl+End`: jump to the beginning or resume following new output
- Drag transcript text, or use `Shift+arrows` in transcript review: select rendered text across scrolling history
- `Ctrl+C` when text is selected / `Alt+C`: copy selection; Escape clears an idle selection (active work still uses Escape cancellation)
- `Ctrl+C`: clear editor text; press again within one second with an empty editor to quit
- `Backspace`: delete backward; `Ctrl+D` / `Delete`: delete forward, never quit
- `/help`: open the complete scrollable command/shortcut reference, including modal controls; local only, never added to model history
- `/quit` / `/exit`: quit and print the session resume command

Question dialogs, pickers, `/btw`, and `/todos` are opaque, modal, and receive input before global controls. Question Enter/Ctrl+M confirms; Escape/Ctrl+C declines; Tab switches questions or leaves single-question notes/custom editing. Notes/custom input uses the same wrapping/folding editor, with a total paste-answer budget of 2 MiB. `PageUp` / `PageDown` scroll questionnaire previews and answer reviews; Submit/Cancel stay visible on short terminals. Pasted text is not executed until explicit submission; folding is presentation only. Unicode editor cursors follow rendered cells for IME placement.

Finalized transcript wrapping is cached by width, and each frame draws only its visible slice. Long transcripts can scroll beyond 65,535 rows without truncating the history.

## Commands

- `/btw <question>`: run a concurrent, tool-free side request from a stable main-context snapshot
- `/btw`: reopen the latest side exchange
- `/todos`: show the complete task list
- `/compact [focus]`: manually summarize older context
- `/new`: create and switch to a new workspace session
- `/resume [uuid]`: open the searchable workspace-session picker, or resume an exact session
- `/model [provider/model]`: open a model picker, or select an exact configured model
- `/skills create`: create a project or global skill through a local, validated wizard; no model request is made
- `/skill:<name> [arguments]`: explicitly expand and invoke a discovered skill
- `/help`: show the retained command set
- `/quit`: exit

Ordinary Enter submissions during work enter an ordered steering queue and clear the editor for the next draft. Steering is injected after a complete assistant/tool batch; follow-ups wait until the run would finish. Pending messages can be restored, edited, and requeued repeatedly without interrupting the current stream. Cancellation restores pending input to the editor. Queues are bounded to 64 entries / 20 MiB and remain in memory; they are not autonomously replayed on resume. `/btw` remains the independent, concurrent, tool-free path. Model-entry changes during main work are staged for the next safe request boundary; the selected entry's effort is fixed.

Scrolling up pauses following new output. Scrolling back to the bottom, clicking the visible jump-to-latest row, or pressing Ctrl+End resumes following. Mouse-wheel scrolling and application-managed drag selection are supported. Selection is anchored to logical text across wrap/resize and finalized history; dynamic selections are cleared when finalization regroups their content rather than copying stale offsets. Copy is capped at 2 MiB and runs outside drawing, with literal UTF-8 sent on stdin to fixed OS helpers. Holding the terminal's selection modifier (usually Shift) remains a visible-screen fallback.

On interactive exit from a non-empty session, after terminal restoration, `mh` prints:

```text
Resume this session with:
mh --resume <actual-session-id>
```

The hint uses the final session after switches, and is omitted from empty launches and print mode. Bare CLI `--resume` requires a terminal and cancels without creating a session.

Clipboard shortcuts use `wl-paste`/`xclip` on Linux, fixed PowerShell clipboard commands on Windows/WSL, and `osascript`/`pbpaste` on macOS. The helpers run outside drawing with bounded output and cancellation; image decoding/resizing also runs in a worker. Bracketed text paste does not require these helpers. Clipboard helpers are separate from model-facing shell execution. Native macOS and actual desktop clipboard transport remain unverified; the verified image-paste tests use controlled clipboard helpers.

## Tools and capabilities

The main agent has four core tools:

- `read`: text head limited to 2,000 lines or 50 KiB; supported images become attachments
- `write`: complete-file creation or replacement
- `edit`: unique, disjoint replacements matched against the original file, with bounded diff windows
- `bash`: native shell execution (Unix Bash/sh or Windows PowerShell) with the model-facing tail limited to 2,000 lines or 50 KiB; full truncated output is retained in the session

It also has native `todo` and, only in the fullscreen UI, `ask_user_question`. `/todos` presents the complete persisted task state. Model-generated tools execute only after a completed tool-call stream event; truncated or unfinished calls are never executed.

The public `bash` name denotes command execution, not a cross-platform Bash dependency. Native Windows uses PowerShell 7 when available, otherwise Windows PowerShell; commands use PowerShell syntax, UTF-16LE encoded transport, UTF-8 output, and no execution-policy override. Git Bash, MSYS2, Cygwin, and Windows-to-WSL launchers are not required. PowerShell keeps final-statement exit semantics; use `exit $LASTEXITCODE` when an exact native-program exit code is needed. Native Linux/macOS and WSL Linux use Bash when available, otherwise `sh`.

## Instructions and skills

At startup, `mh` loads:

1. global `~/.mh/config/AGENTS.md`, falling back to `CLAUDE.md` in the same directory;
2. inherited `AGENTS.md` files from filesystem root to the working directory, with same-directory `CLAUDE.md` fallback;
3. project skills from conventional `.mh`, `.pi`, `.claude`, `.agents`, and `.codex` skill directories within the workspace boundary, then nonstandard workspace `SKILL.md` locations, then global `~/.mh/config/skills/`.

Skill metadata is loaded at startup, but bodies are read lazily. Discovery respects `.gitignore`, `.ignore`, and `.fdignore`, skips generated/dependency/cache trees, deduplicates canonical paths, rejects symlink escapes, and has shared traversal/read/catalog budgets. Precedence is nearest conventional project roots (mh first), nonstandard project roots, then global mh skills. The workspace boundary is the nearest repository root, or the working directory without a repository. Supporting paths are relative to the skill directory; diagnostics appear under `[Warnings]`.

`/skills create` writes only canonical project `.mh/skills/<name>/SKILL.md` or global `~/.mh/config/skills/<name>/SKILL.md`, never overwriting an existing skill. It refreshes the catalog/completion/prompt while main work is idle. `disable-model-invocation: true` excludes the skill from model-facing discovery/system prompts while retaining explicit `/skill:<name>` invocation. This is catalog/invocation policy, not a filesystem ACL.

The shared editor provides atomic bounded Ctrl+Z undo/recovery for replacements, pastes, images, and completion. Redo and its unused history state are removed; restored history/queue drafts start fresh undo boundaries without losing their contents. Opaque rectangular dialogs and blank message/tool-group separators improve readability without outer viewport gutters. The terminal title uses bounded mh-owned `🍀 mh` activity labels; title-stack restoration is best-effort on supporting terminals.

## Sessions and compaction

Session-owned files are stored under:

```text
~/.mh/sessions/<workspace-id>/<session-id>/
  session.jsonl
  session.lock
  outputs/output_<sha256-of-tool-call-id>.log
```

Sessions are workspace-scoped and exclusively locked while open. Discovery ignores sessions with no user message. A torn, unterminated final journal write is repaired to the preceding durable record; malformed complete or interior records remain errors.

Automatic compaction is always enabled. `reserveTokens` controls its threshold and `keepRecentTokens` controls the retained tail. Tool-call/result pairs remain together. Failed automatic compaction retains the original context and does not turn an already completed answer into a failed turn. Manual `/compact` reports failure directly.

Cumulative usage survives resume and compaction and includes main requests, side requests, and compaction attempts. Current-context percentage is tracked separately. The footer cache percentage is:

```text
cacheRead / (input + cacheRead + cacheWrite) * 100
```

`/btw` text is kept only in a bounded in-memory thread of 20 exchanges and never enters the main transcript or model context. Its numeric usage is persisted.

## Proxy behavior

Provider requests honor only:

- `HTTP_PROXY` / `http_proxy`
- `HTTPS_PROXY` / `https_proxy`
- `NO_PROXY` / `no_proxy`

Uppercase names take precedence, including empty values. `ALL_PROXY`, `all_proxy`, and operating-system proxy settings are ignored. Startup status reports variable names and configured/direct routing without printing proxy URLs, credentials, provider endpoints, or API keys.

## Deliberate non-goals

`mh` does not provide MCP, subagents, an extension loader, session branches or forks, RPC/JSON modes, OAuth, sharing/export, telemetry, automatic updates, prompt templates, public context hooks, configurable keybindings, a regular-screen renderer, dynamic system themes, or feature toggles.

## Verification and platforms

The complete local check set is:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo build --release --locked
cargo run --example generate_schemas -- --check
```

Supported runtime targets are only native Linux, native macOS, WSL Linux, and native Windows from PowerShell. Unix builds use `tikv-jemallocator`; native Windows uses Rust's system allocator and the MSVC build target.

WSL verification includes all required checks and actual release interaction: editing/recovery, Alt+Q queue re-editing, questions/todos, `/btw`, compaction, cumulative usage, session continue/new/resume, both exit commands, rendering colors, and terminal restoration. Native Windows MSVC complete checks and genuine HTTPS/stateless transport passed at the preceding milestone; final-source/static-CRT verification is recorded separately in `doc/development_context.md`. A native executable was actually driven through questions, skill creation, title, exit, and input-mode restoration; console resize remains unverified. Native Linux/macOS execution and hosted CI are not claimed merely from configuration. Desktop clipboard data was preserved when nonempty; controlled helpers verify app wiring.

## Distribution CI and releases

Pushes and pull requests run native checks and upload self-contained binary archives with SHA-256 checksums for:

- Linux x64: `x86_64-unknown-linux-musl`, static libc, no dynamic loader/shared-library dependency.
- Windows x64: `x86_64-pc-windows-msvc`, static CRT, used directly from PowerShell.
- macOS arm64: `aarch64-apple-darwin`, only OS-provided dynamic libraries.

“Self-contained” means no extra language runtime or redistributable C runtime; the OS, terminal, native shell, and optional clipboard helpers remain prerequisites. Linux musl builds require `musl-tools`. Windows static-CRT builds use an explicit target and target-specific flags so host proc-macro builds remain unaffected:

```powershell
$env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS = '-C target-feature=+crt-static'
cargo build --release --locked --target x86_64-pc-windows-msvc
```

A pushed `v<version>` tag matching `Cargo.toml` publishes the three checked archives/checksums after every native matrix job passes. Existing release assets are not overwritten. Ordinary pushes do not publish a version release; workflow artifacts are retained for 14 days. The workflow uses pinned official GitHub action revisions and checks architecture/runtime dependencies before packaging.
