# Development context

This is a machine-neutral development record, not a raw conversation log. Keep personal paths, private session identifiers, deployment values, actual credential-variable names, and shell-profile details out of this document and `AGENTS.md`.

## Current state

The authorized eleven-item implementation, direct-effort/readability refinements, complete local `/help`, `/exit` alias, and Ctrl+Z input recovery are implemented. Ctrl+R redo and its unused state were removed at the user's request. Alt+Q restores pending messages without stopping main work; Alt+Up remains accepted. Tool-output `│` and `└` use the same border color; omission text remains dim.

A whole-source review was completed, followed by concrete boundary and regression repairs. The latest complete WSL Linux check set passed 254 tests and the release was actually launched and driven through editing, selection/copy, questions/todos, concurrent `/btw`, manual compaction, usage, continue/new/resume, and both exit commands. A separate actual print-mode driver verified automatic compaction and checkpoint replay. Native Windows MSVC final-source checks passed 245 tests and all five required commands. The explicit-target static-CRT executable passed PE dependency checks and was actually driven through Ctrl+Z recovery, Ctrl+D no-exit, complete help, questions, local skill creation, clover title, normal exit, exact input-mode restoration, and resume hints. Native console resizing remains unverified because the probe cannot reliably resize its allocated console viewport. Do not label the complete native console probe as passed.

Git initialization and the exact remote/push were explicitly authorized; the privacy-checked initial source is pushed to main. Distribution CI is implemented for Linux x64 musl, Windows MSVC x64 static CRT, and macOS arm64. YAML and embedded Unix script syntax were checked locally. The first hosted Linux job passed, including the static-musl distribution. Windows passed tests but failed exact generated-schema text comparison after CRLF checkout; `.gitattributes` now enforces LF. macOS failed Clippy; exact diagnostic annotations identified imports used only by non-macOS clipboard code, now conditionally compiled. An unnecessary macOS-only tail return was also removed. The following hosted macOS run reached the shell-executor tests but failed without assertion details in the public output; bounded exact native-test failure annotations were added rather than guessing a source fix. Corrected hosted jobs still need a successful rerun. No initial version tag/release was authorized or published.

## Scope and upstream

- One native Rust Cargo package, no Node.js runtime dependency.
- Runtime targets only native Linux, native macOS, WSL Linux, and native Windows used from PowerShell.
- OpenAI Responses HTTP SSE only, with named environment references for endpoint/key values.
- Separate `read`, `write`, `edit`, and shell-command `bash` tools; separate todo and interactive question capabilities.
- All retained capabilities and automatic compaction enabled; no feature toggles.
- No MCP, subagents, session branches, extension loader, RPC/OAuth, telemetry, automatic updates, prompt templates, sharing/export, or unsolicited features.
- Preserve unrelated work, credentials, configuration, and historical journals. Installation/commit/push authorization is action-specific.

Behavioral upstream is Pi revision `780bf31fcaeb252fa9b68df3964c735eae75bde3` (package 1.6.0); retain MIT notices. Actual Pi 1.5.1 and Claude Code 2.1.330 interfaces were inspected in isolated PTYs; source/tests and official UI documentation supported the retained layouts.

| Mechanism | Pi reference |
| --- | --- |
| Agent loop and ordered completed tools | `packages/agent/src/agent-loop.ts` |
| Core tools/native shell | `packages/coding-agent/src/core/tools/`, `utils/shell.ts` |
| PowerShell implementation | `core/tools/powershell.ts`, `utils/shell.ts` |
| Resources/skills | `core/resource-loader.ts`, `core/skills.ts` |
| Compaction/session | `core/compaction/`, `core/session-manager.ts` |
| Responses replay/SSE | `packages/ai/src/api/openai-responses.ts`, `openai-responses-shared.ts` |
| Retries | `packages/ai/src/utils/retry.ts`, `provider-retry.ts`, interactive `status-indicator.ts` |
| Pending-message editing | `core/keybindings.ts`, interactive `restoreQueuedMessagesToEditor` |

Pi's installed keybinding implementation selects Alt+Q on native Windows/WSL and Alt+Up on native Unix. mh accepts both and prominently documents Alt+Q. Pi restores all queued text plus the current draft without aborting unless explicitly requested; mh keeps typed attachments and transactional queue restoration.

UI references: <https://code.claude.com/docs/en/interactive-mode>, <https://code.claude.com/docs/en/terminal-config>, <https://code.claude.com/docs/en/fullscreen>, <https://code.claude.com/docs/en/sessions>. Earlier Codex redraw research at `1145028ae47e8119955323d38ce34122d418f951` is historical context, not authority for new designs.

## Configuration, identity, and Responses

Configuration stays under `~/.mh/config/`. `mh --init` preserves editable config/theme files and creates schema-linked templates; normal startup refreshes authoritative schemas and validates before opening a session. Metadata help/version is side-effect free. Errors/logs omit endpoint values, keys, and raw provider diagnostics.

Each model has a local alias (wire ID fallback) and one fixed direct `reasoningEffort`. Selection is `provider/alias`, the request sends the wire ID. Multiple aliases can share a wire ID. Null/absent effort means provider default/omission; literal `none` is explicit. Effort is a bounded nonempty ASCII identifier and requires reasoning capability. Selection identities reject controls/surrounding whitespace.

There is no active `thinkingLevelMap`, independent `--thinking` or `/thinking`, mutable effort selection, or thinking picker. Legacy fields normalize only at the input boundary, in memory before modern schema validation; raw typed parsing preserves duplicate-field rejection. Mixed canonical/legacy effort fields are invalid. Old saved choices resolve only when unambiguous; journals and editable user configurations are not rewritten. The unused thinking-color selector was removed. Historical theme keys remain accepted for compatibility but thinking-level/tool-background/bash-mode overrides are inactive in the retained fixed-effort/plain transcript design.

Responses use `store: false` and compatible `include: ["reasoning.encrypted_content"]` for reasoning-capable models. The current [OpenAI reasoning guide](https://developers.openai.com/api/docs/guides/reasoning?api-mode=responses) says include is optional in stateless mode. Encrypted content and assistant phase survive streamed/terminal merges, ordered tool replay, resume, and retained compaction tail. Debug redacts opaque reasoning. Continuation is conservatively same provider/wire ID, including aliases, without guessing model families or adding unsupported universal fields.

SSE requires a terminal event. Completed tool arguments cannot be overwritten by unfinished argument fragments; unfinished calls never execute. Partial message text is retained per output index even if a reasoning/tool/other message already completed. Optional leading BOM is accepted across network chunk boundaries. Frames are capped at 16 MiB, total stream input at 64 MiB, and output indices at 4,096.

Transient transport/provider failures retry at most three times with 2/4/8-second delays or bounded provider hints (60-second cap). Fractional millisecond and HTTP-date hints work. Quota/account errors, explicit no-retry, cancellation, and full deadline expiry are terminal. A 15-minute outer request deadline includes header/stream/UI-notification waits and preserves partial output/usage; connect/read limits remain distinct. Cancellation has priority.

Provider proxy discovery honors only HTTP/HTTPS/NO_PROXY in uppercase/lowercase; uppercase wins including empty values. ALL_PROXY and OS settings are ignored. Presentation reports generic route status/names, not values.

## Runtime, files, and persistence

One main-operation guard excludes overlaps. Complete tool batches execute concurrently and persist in source order. Mutation locks combine destination reservations with existing-file identity, including hard links; creating new destinations uses a gate to prevent case aliases from racing. This is not a filesystem sandbox against hostile external replacements.

The public `bash` name means shell command execution. Linux/macOS/WSL use executable Bash then sh. Native Windows prefers PowerShell 7, then Windows PowerShell, with UTF-16LE encoded whole scripts, UTF-8 output, and final-statement exit semantics. Use `exit $LASTEXITCODE` for exact native exit codes. No Git Bash/MSYS2/Cygwin/Windows-to-WSL launcher or execution-policy override is required. Windows command transport checks length before encoding. Unix process groups and Windows Job Objects clean descendants. Stdout/stderr are independently incrementally decoded before textual merging; incomplete UTF-8 scalars cannot cross pipes.

Shared `src/regular_file.rs` supplies safe regular-file opens to tools and instruction/skill readers. Unix O_NONBLOCK/O_NOCTTY prevents raced FIFO/terminal blocking; Windows guards device/pipe namespaces. Writes do not truncate until handle validation/cancellation checks; edits read/write one verified handle. Instruction files have 1 MiB each / 4 MiB aggregate read budgets. Skill limits remain separate below.

Read uses bounded streaming with Pi numeric slicing, counting/rewind only for negative ends. Negative-limit continuation metadata derives from the effective endpoint; fractional behavior is retained. Core output limits are 2,000 lines / 50 KiB (read head, shell tail), with session-owned spools. Images use verified bounded handles, 64 MiB compressed/128 MiB decode allocation limits, PNG for resized alpha-bearing input, and separate actual x/y coordinate scales.

Edit matches disjoint unique source spans. Fuzzy matching maps grapheme-normalized offsets back to original substrings instead of normalizing unrelated content on affected lines. Generated diff work/fields are byte/line bounded before mutation. Unified unterminated lines carry the standard no-newline marker. BOM/CRLF-preserving edits explicitly label normalized patch previews non-applicable to original bytes.

Sessions are append-only, workspace-scoped, exclusively locked, and stored under `~/.mh/sessions/<workspace-id>/<session-id>/`. Only torn final records are repaired. Interrupted tools close without replay. Spools use `output_<sha256-of-complete-case-sensitive-call-id>.log`, avoiding long/reserved/case-colliding names. Existing spools/journals are unchanged. Pre-write oversized record rejection no longer poisons a healthy writer. Input preflight counts actual escaped journal bytes before clearing a draft.

Steer/follow-up queues are in-memory FIFO (64 entries / 20 MiB) with claim/commit rollback; model selection stages at safe boundaries. `/skill:` expansion preserves the requested queue kind. Queues are never autonomously replayed after resume.

Automatic compaction is always enabled by model budgets. Cuts round backwards to retain complete call/result batches rather than dropping the recent-token allowance. Structured empty file arrays are authoritative; absent legacy fields alone parse summary suffixes, null is rejected. Failed automatic compaction preserves original context. Presentation/history remain separate from compacted model context.

Usage totals include main, retries/failures, compaction, and side work; current context is distinct. `cacheRead / (input + cacheRead + cacheWrite) * 100` uses unrounded inputs. A presentation revision keeps usage-only snapshots from rebuilding immutable historical rendering/wrapping. Full presentation rebuilds on actual content changes remain a large-session performance limitation.

`/btw` is concurrent, tool-free, bounded to 20 exchanges, and outside main history/context. Only usage is persisted. Reopening while active preserves the current question and streamed answer. Escape closes without cancellation; Ctrl+C cancels only side work; X clears it. Interactive and print runtimes share graceful termination signals and cleanup.

## UI, editing, tasks, and skills

Edge-to-edge viewport; native top/bottom input rules and plain `>` without sides/corners/title. Dialogs use opaque native Plain rectangular borders, outside field labels, separated sticky actions, and readable message/tool-group spacing. Tiny viewports collapse spacing. User/model content is preserved; terminal controls sanitize only presentation. Print stdout sanitizes controls only for a TTY; pipes preserve original bytes. Piped context and positional prompts receive a separator; stdin is bounded.

`src/ui/help.rs` is local, scrollable complete command/shortcut help, never model/history content. Commands/completion descriptions share the retained registry; `/exit` and `/quit` are aliases. Modal controls take precedence. Help supports arrows/pages/wheel/Home/End and closes without cancelling main work.

Ctrl+Z undoes/recovers one atomic editing transaction, never suspends fullscreen Unix mh. Ctrl+R redo and its state are removed. Ctrl+U/K delete to line start/end, Backspace/Ctrl+D delete backward/forward, Shift+Enter/Ctrl+J insert a newline. Ctrl+D never quits empty input. Newline-modified Enter never confirms a non-editor question/skill/picker action. Atomic text/folded-paste/image snapshots are capped at 100 / 32 MiB. Every mutation validates expanded attachment occurrences, total bytes, logical rows, and line columns; failed completion rolls back. History/queue restoration starts fresh boundaries without losing content. Question field mutations enforce the aggregate 2 MiB limit, including keyboard/yank paths.

Transcript selections use logical source offsets and native-wrap-compatible grapheme/cell mapping with 64 cached source lines and a 2 MiB copy cap. Finalized selections survive scrolling/resizing/appends; dynamic offsets clear on finalization regrouping. Mouse/Shift navigation and fixed OS UTF-8 stdin helpers implement copy outside drawing. Nonempty desktop clipboard data is never overwritten merely to verify. Wayland text helpers fall back to X11 discovery when unavailable.

Task/tool/activity markers share todo semantics and restrained Unicode. Dependency cycle checks are iterative, avoiding recursive stack overflow; update rejects create-only blockedBy. Confirmed questionnaire custom intent survives deselecting the last option. Long question headings reserve visible answer rows and can page. Completed tasks remain in the bounded panel until the next run. Titles contain only bounded sanitized mh-owned clover activity labels; exact title-stack restoration is terminal-dependent.

Skills: conventional workspace `.mh`, `.pi`, `.claude`, `.agents`, `.codex` nearest-first inside repository boundary (cwd fallback), then nonstandard workspace SKILL.md, then canonical mh global root. No unrelated global Pi/Claude imports. Ignore rules, canonical dedupe, escape/loop protection, deterministic collisions and shared bounds: 20,000 entries/depth 64/4 MiB discovery/256 skills/256 KiB catalog/64 KiB metadata and ignore/1 MiB lazy body. Nonstandard discovery is an explicit requested adaptation.

`/skills create` is a typed local six-step wizard; no model request. Valid portable names/frontmatter, no overwrite/symlink traversal, atomic no-replace hard-link publication, and guarded resource/prompt refresh. `disable-model-invocation: true` is explicit catalog/invocation policy, not a filesystem ACL. Hard-link-unsupported filesystems fail safely.

## Verification and distribution

Focused tests precede the complete repository-root checks:

```text
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo build --release --locked
cargo run --example generate_schemas -- --check
```

Actual-app drivers, always controlled fixtures:

```text
python3 -I tests/manual_tui.py target/release/mh
python3 -I tests/manual_print_tty.py target/release/mh
python3 -I tests/manual_compaction.py target/release/mh
pwsh -NoLogo -NoProfile -File tests/manual_console.ps1 -Binary <native-mh.exe> [-SkipResize]
pwsh -NoLogo -NoProfile -File tests/manual_powershell.ps1
```

The PTY workflow checks stream/Alt+Q re-edit, fixed aliases, a completed file tool, questions/todos, isolated concurrent/reopened side answers, copy via controlled helpers, local explicit-only skills, Ctrl+Z/no-redo/newlines/deletion, emitted RGB connector consistency, complete help, manual compaction, journal/footer usage, real relaunch/continue/new/resume/exit, resize/title/resume hints, and terminal restoration. The print driver separately checks actual TTY OSC/CSI suppression and piped-prompt separation. The automatic-compaction driver launches actual mh twice, checks checkpoint replay and durable compaction usage, and executes no tools.

Opt-in `MH_LIVE_VERIFY_CONFIG=<config-file> cargo test --test live_provider -- --nocapture` previously passed two genuine HTTPS checks on WSL and native Windows: tool-free Responses transport and two-turn stateless encrypted continuation. Deployment details are intentionally omitted. Normal test runs do not enable these network checks. Existing PowerShell 7/Windows PowerShell probes passed transport/UTF-8/exit/stderr/malformed whole-script semantics with unchanged policy; they are distinct from compiled app verification.

`.github/workflows/ci.yml` builds/tests on Ubuntu x64, Windows MSVC x64/PowerShell, and native macOS arm64 (architecture asserted). Distributions: Linux `x86_64-unknown-linux-musl` without interpreter/NEEDED entries; Windows explicit `x86_64-pc-windows-msvc` with target-specific `-C target-feature=+crt-static` and PE dependency checks; macOS `aarch64-apple-darwin` with only OS dylibs. Exact official action revisions are pinned. Ordinary pushes/PRs upload archives/checksums; matching `v<Cargo-version>` push tags publish immutable releases only after all jobs pass. Self-contained excludes extra language/C runtimes, not OS libraries/terminal/native shell/optional clipboard helpers.

## Milestones and unresolved limitations

- Foundation/baseline: typed config, separate retained capabilities, Responses, journals, compaction, schema init/refresh, native viewport, and ownership/recovery fixes.
- Milestone 22: Pi-style editor/wrapping/completion/history/attachments, ordered queues, modal questions/pickers, startup/footer/resume. WSL checks: 138 tests; historical Windows GNU: 137, not new native evidence.
- Milestone 23: independently reproduced notification-backpressure deadline bypass and bounded retries/countdowns. WSL: 148. An interrupted quiet wait alone did not prove deadlock.
- Milestone 24 (2026-11-22): full eleven-item backlog, direct effort and readability, retained-core eight-defect repairs, machine-neutral docs. WSL: 234; native MSVC: 226; genuine HTTPS/stateless verification passed on both.
- Milestone 25 (2026-11-23): whole-source review, Ctrl+Z/no-redo, complete local help/exit, connector-color consistency, input/journal/stream/compaction/task/question/native-file fixes, actual session/compaction/usage probes, Git and three-target distribution workflow. Latest WSL checks: 254 tests, including recoverable completion rejection without terminating the UI. Final native/hosted verification is separate.

Remaining limitations:

- Native Windows console resize probe remains failed/unverified; its non-resize Ctrl+Z/help/interaction/cleanup checks passed on the static-CRT binary. Initial recovery-probe failures were caused by non-blittable fixture key records/incorrect virtual-key mappings; corrected records passed.
- Hosted native Linux checks/static-musl distribution passed. Hosted macOS/Windows initial failures and corrections are recorded above; they are not full hosted verification until the rerun passes. Local native Linux/macOS execution was unavailable.
- Clipboard desktop transport not fully verified; nonempty data preserved. Very long intra-line scroll remains constrained by Ratatui u16 representation; global rows are usize.
- Real content updates still rebuild full historical presentation/wrapping. Usage-only rebuilds are eliminated; additional incremental rendering needs careful source-anchor handling.
- Filesystem mutation/skill publication is not a sandbox against hostile concurrent path replacement; hard-link limitations remain explicit.
- Negative-end reads need a counting pass and do not promise stable external snapshots; edit loads full regular-file content even though generated diff work/metadata are bounded.
- Historical theme override names remain accepted but inactive as documented; no dynamic thinking UI is reintroduced.
- Earlier denied cleanup was not retried through another route. Controlled new fixtures own their data; registered compiler/SDK installations are not casually deleted.
