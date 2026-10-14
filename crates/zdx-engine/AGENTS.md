# zdx-engine development guide

Scope: core runtime engine — config, agent orchestration, tools, prompt/context assembly, and shared registries.

## Where things are

- `src/lib.rs`: engine crate exports
- `src/test_support.rs`: shared test environment RAII helper (`TestZdxHomeGuard`) for unit tests needing isolated `ZDX_HOME` state
- `src/providers.rs`: re-export of `zdx_providers::*`
- `src/audio/mod.rs`: shared audio module exports
- `src/audio/speak.rs`: shared text-to-speech (TTS) synthesis helpers (OpenAI/Mistral); default OGG/Opus output via ffmpeg transcode with MP3 fallback
- `src/audio/transcribe.rs`: shared audio transcription helpers (OpenAI/Mistral via `/audio/transcriptions`; xAI Grok STT via `/stt`; ElevenLabs Scribe via `/v1/speech-to-text` with `xi-api-key`)
- `src/agent_activity.rs`: active-run registry (ephemeral marker files for agent turns)
- `src/background_activity.rs`: background-process registry — durable marker files under `~/.zdx/run/background/` for long-lived processes started with the Bash tool's `background: true`; identity-guarded (`pid`+birth-time+`pgid`) kill defends against PID reuse; exited-tombstone + age prune
- `src/job_daemon.rs`: the scheduled daemon's context (running record `run/daemon.context.json`, else the installed launchd plist, else unavailable with a reason) and the jobs view resolved with it (`jobs()`, invalid files included). Surfaces that show daemon settings use this, never their own root.
- `src/jobs/`: jobs (see `docs/ARCHITECTURE.md` § Jobs Scheduler). `mod.rs`: discovery into a `JobCatalog` (an invalid file is listed, never fatal), frontmatter parsing, the shared resolver (`resolve_run`, `resolve_run_settings`, `overview`, plus `*_from` variants that take an explicit inherited context), the shared live state (`job_state`, `summaries`), and `set_model`, the one safe editor of a job's `model:` line. `schedule.rs`: manual/recurring/once schedules, cron, occurrence keys (DST-safe). `store.rs`: `JobsHome` paths and durable state (exclusive claims, `flock` run lock, `last.json` and its repair from claims, the per-job missed checkpoint, `jobs_runs.jsonl`, scheduler state, the no-clobber archive move, and the archived listing). `plan.rs`: the pure per-tick planner. `migrate.rs`: temporary `automations/` → `jobs/` migration and guard; remove one release after the rename. Surfaces (CLI, Monitor, Mini App) consume these; they do not re-derive schedule, status, model, or root.
- `src/config.rs`: config loading + paths (embeds `zdx_assets::DEFAULT_CONFIG_TOML`)
- `src/custom_commands.rs`: custom slash command discovery + frontmatter parsing (`<ZDX_HOME>/commands` + ancestor/current `.zdx/commands`, plus bundled commands from `zdx_assets::bundled_command_assets()`)
- `src/followups.rs`: shared `<followups>` suggestion-block parsing (surfaces strip + render their own way)
- `src/models.rs`: model registry for model picker (embeds `zdx_assets::DEFAULT_MODELS_TOML`)
- `src/mcp.rs`: MCP config loading, server discovery, helper workspace/runtime, and MCP tool-call execution helpers
- `src/prompts.rs`: prompt template helpers/re-exports of `zdx_assets` prompt constants.
- `src/skills.rs`: skills discovery + parsing (materializes bundled skills from `zdx_assets::bundled_skill_assets()`)
- `src/subagents.rs`: named subagent discovery + parsing (built-in subagents come from `zdx_assets::{EXPLORER_SUBAGENT,ORACLE_SUBAGENT,ORCHESTRATOR_SUBAGENT}`; `orchestrator` is reserved — user/project files cannot define it, and `load_builtin_orchestrator()` loads it directly from the embedded asset)
- `src/images/mod.rs`: shared image utilities module exports
- `src/images/decode.rs`: generic image decode/resize/PNG encode helpers
- `src/images/path_mime.rs`: path normalization + extension MIME helpers
- `src/media.rs`: one-shot media understanding (audio/video/PDF/image + prompt → Gemini text); shared by the `ask_media` tool and `zdx ask-media`
- `src/pidfile.rs`: PID file management
- `src/proc_liveness.rs`: process liveness and identity primitives shared by the PID file, agent-activity, and background-process trackers
- `src/service.rs`: launchd-backed control for the long-lived `bot`/`daemon` services — plist rendering, `install`/`uninstall`/`start`/`stop`/`restart`, and combined launchd+PID-file `state()`. Also owns the queued restart: one persisted request at `run/restart-queue.json` (`queue_restart` merges into a pending one and refuses unless a supervised bot is running to perform it; `QueuedRestart::is_fulfilled` compares service PID-file start times against `requested_at`). Agents run `~/.local/bin/zdx` (never `current_exe()`) so restart picks up the installed binary, launched via `zsh -c 'exec …'` so `~/.zshenv` (provider API keys, PATH) is sourced. Plists set `ZDX_SERVICE_SUPERVISOR=launchd` and capture output to `~/.zdx/run/logs/{name}.{out,err}`. macOS-only.
- `src/telegram.rs`: outbound Telegram Bot API calls (create topic, send message, send document); shared by the Telegram tool and `zdx telegram`
- `src/tracing_init.rs`: tracing setup
- `src/workspace_config.rs`: shared workspace model-settings view — workspace list (Telegram profile `cwd`s used as-is + the caller's root resolved to its project root), per-setting effective value + origin layer (`default`/`global`/`workspace`/`ancestor`) + inherited fallback, and targeted `set_setting`/`reset_setting` on one layer. Backs Monitor Config, `zdx config workspaces`, and `/api/workspaces`. Writers reuse `Config::*_to` (whose `layer_document` seeds the template only for the global file, so overlays stay minimal) and `Config::unset_layer_key`.
- `src/zdx_context.rs`: shared `{{ZDX_CONTEXT}}` block that tells helper subagents (handoff, tldr, prompt-builder, `Read_Thread`), which run without a system prompt, about installed subagents and skills

### Core runtime (`src/core/`)

- `core/mod.rs`: core module exports
- `core/events.rs`: agent event types for streaming
- `core/context.rs`: project context loading (`AGENTS.md`/`CLAUDE.md`, memory)
- `core/interrupt.rs`: signal handling
- `core/agent.rs`: agent loop + event channels. Owns the stalled-stream limit (`stream_idle_timeout_secs`): one no-progress budget per provider request, covering the wait for the stream to open and the gaps between non-`Ping` events, measured on Tokio's clock so tests use paused time. A stall is a retryable `Timeout` provider error, so it goes through the normal retry gate.
- `core/artifacts.rs`: shared thread artifact model — artifact-dir scan merged with transcript `<media>` refs (deduped), preview kinds, download path guard
- `core/fts_query.rs`: shared FTS5 `MATCH` builder, so thread search and memory search interpret a query the same way
- `core/goal.rs`: goal mode: in-memory objective state and the verifier agent that decides when the goal is met
- `core/handoff_generation.rs`: LLM-based handoff context generation and the header-first `/handoff` and `/btw` seeds (source header, labelled user message, labelled generated background; shared by TUI + bot)
- `core/media_fallback.rs`: image fallback for models with `input_images = false` — swaps user/tool-result image blocks for a note pointing at the `ask_media` tool, applied per model turn in `core/agent.rs`
- `core/prompt_builder_generation.rs`: LLM-based prompt-builder generation (shared by TUI + bot)
- `core/recency.rs`: shared recency decay applied to search relevance scores (`thread_index` and `native_memory`)
- `core/native_memory.rs`: native SQLite-backed memory index/search over exported thread Markdown, Notes, and Calendar; owns `$ZDX_HOME/cache/memory.sqlite` and native `zdxmem:v1:*` docids. Includes the opt-in hosted embedding layer (`[memory.embeddings]` profile, budgets, `(input_hash, profile_fingerprint)` vector storage, vector/hybrid retrieval with RRF fusion); agent searches never trigger corpus embedding.
- `core/thread_index.rs`: derived thread cache at `$ZDX_HOME/cache/threads.sqlite` (metadata, export dirty state, FTS over title + user/assistant text, tool-call rows); serves `list_threads()`, `search_threads()`, `search_thread_tools()` with a raw file-scan fallback. **The sync split is load-bearing:** `SyncScope::Meta` reads one line per changed thread; `SyncScope::Full` re-reads and re-tokenizes each changed transcript. Listing, lineage and lookup paths must take `Meta`; only search and browse take `Full`. Routing a metadata reader to `Full` puts corpus re-tokenization back on the Mini App hot path (measured 7.7s with 20 active threads). Details are in the module docs.
  - `indexed_thread_files()` publishes each thread's `(mtime_ns, size)` change token after a `Meta` sync, so other derived caches (`usage_stats`) never `stat()` the corpus themselves.
- `core/subagent.rs`: child `zdx exec` subagent runner. Child runs persist their own thread JSONL tagged via `ExecSubagentOptions::thread_origin_kind`/`thread_parent_id`/`thread_subagent_name` (so their usage is captured by `usage_stats`); tagged threads are hidden from default listings. `ExecSubagentOptions::thread_id` runs the child against an explicit existing thread (`--thread`, resumable workers). Unix children use the shared `zdx-tools` lease supervisor so cancellation, future drop, or owner death TERM→KILLs the group and reaps the target.
- `core/thread_export.rs`: clean Markdown transcript exports derived from saved thread JSONL
- `core/title_generation.rs`: LLM-based title generation (shared by TUI + bot)
- `core/tldr_generation.rs`: LLM-based thread TLDR/recap generation (shared by TUI)
- `core/thread_persistence/`: thread persistence (JSONL per thread; `persist`, `replay`, `search`, `storage`, `format`, `event`). `list_threads()` hides child runs (any thread with `Meta.origin_kind` set — subagents/helpers); `list_all_threads()` includes them. Usage stats scan raw files (`list_thread_files`) so they still count child runs.
- `core/thread_trajectory.rs`: UI-agnostic, serde-serializable per-thread trajectory projection shared by CLI JSON/text, Monitor, and the Mini App API; owns span pairing, exact interval placement/overlap, bottleneck ranking, live snapshots, and legacy uncertainty
- `core/usage_stats.rs`: usage/cost aggregation over saved threads, backed by the disposable cache `$ZDX_HOME/cache/usage-v3.sqlite` (see `docs/ARCHITECTURE.md`, Usage Accounting). Keep the caller's `default_model` out of cached rows: callers with different configs share the cache, and a config-dependent key makes them wipe it for each other (verified 2026-10-14: a full 21k-thread rescan, about 70 s, on every Mini App monitor refresh). Parse transcripts outside the cache's write lock and keep write transactions short: holding the lock across a scan made concurrent callers fail with `SQLITE_BUSY` and fall back to a full uncached scan (verified 2026-10-14). It learns which threads changed from `thread_index::indexed_thread_files()`; do not add a second directory walk on the dashboard path. The walk stays only as a fallback (tests, unavailable index).
- `core/workers.rs`: in-memory `WorkerManager` for the orchestrator profile (overview: `docs/ARCHITECTURE.md`, Orchestrator + Worker Threads). Invariants:
  - `all_idle()` relies on each worker sending `Completed` under the same lock that settles its status. Keep them together; the bot's queued restart depends on it.
  - Per worker, events arrive as `Created` → (`Prompted` | `Activity`)* → `Completed`; the default runner drains all `Activity` before the turn's `Completed`.
  - Completions route by `QueuedPrompt.requester_thread_id` (captured at enqueue), never by the mutable `WorkerState.owner_thread_id`, which only drives listings and cards.
  - `thread_persistence::is_worker_lineage` and its SQL twin `thread_index::WORKER_SQL` must stay in step (`worker_filter_matches_only_worker_lineage` checks this).
  - Resumes (`send_message`, `enqueue_from_topic`) re-read the worker thread's persisted root **and** model override. The child `zdx exec` never reads a thread's model override: without `-m` it re-resolves the model from config layers, so a mid-thread workspace change would silently switch the worker's model.
  - All state is process-lifetime; thread JSONL survives a restart.
- `core/worktree.rs`: git worktree management helpers

### Tools (`src/tools/`)

- `tools/mod.rs`: ToolContext, ToolRegistry, ToolSet, handlers
- `tools/orchestrator.rs`: the orchestrator's thread-control tools (unbound stubs in the default registry; the bot rebinds them to its live `WorkerManager`)
- `tools/background.rs`: `run_background` (spawn+register a background process, invoked by the Bash tool on `background: true`) + `background_output`/`background_kill` agent tools (thread-scoped)
- `tools/gh_api.rs`: read-only GitHub REST tool over `gh api` (GET only, base64 content decoded, diff/raw media types); wraps `gh` because its token lives in the OS keyring
- `tools/git.rs`: read-only git inspection tool (`status`/`log`/`diff`/`show`); fixed subcommands and flags, operands rejected if they look like options
- `tools/list_models.rs`: read-only listing of the configured `[[model_modes]]` (name, description, primary, alternatives) plus the model registry with `zdx models list` semantics (enabled providers + custom-provider models as exact `provider:model` ids, display names, subscription flag, optional provider filter); gives shell-less agents — the orchestrator profile — verified ids to dispatch instead of guesses; model output capped at 200 entries with a narrowing note
- `tools/telegram.rs`: outbound Telegram tool (`send_message`/`send_document`/`create_topic`) over `telegram.rs`; shares its core with `zdx telegram`
- `tools/ask_media.rs`: one-shot media understanding tool (image/PDF/audio/video → text) over `media.rs`; read-only, shares its core with `zdx ask-media`
- `tools/memory_search.rs`: native memory search returning stable memory refs
- `tools/read_thread.rs`: read saved thread transcript tool
- `tools/subagent.rs`: invoke_subagent tool
- `tools/todo_write.rs`: structured todo/task tracking tool
- `tools/thread_search.rs`: thread discovery tool

## Conventions

- Keep `zdx-engine` UI-agnostic.
- No direct terminal UI logic here; terminal behavior belongs in `zdx-tui`.
- Prefer `anyhow::Result` + `Context` at I/O boundaries.

### Test isolation and storage state

- Tests touching thread persistence/indexing or `$ZDX_HOME`-backed storage must hold `crate::test_support::temp_zdx_home()` for the duration of the test.
- Do not call `std::env::set_var`/`remove_var` directly in tests; use `temp_zdx_home()` or pure helpers.
- `temp_zdx_home()` returns an RAII guard (`TestZdxHomeGuard`) that acquires the engine test mutex, creates a `TempDir`, sets `ZDX_HOME`, resets `thread_index` caches, and restores original environment state on `Drop`.
- Do not introduce process-global `OnceLock<PathBuf>` or `LazyLock<PathBuf>` cached paths derived from `ZDX_HOME` or environment state.

### Logging / tracing

- Spans carry identity, events stay short. Existing spans: `turn` (`run_turn_inner`: `thread`/`model`/`provider`), `provider_request` (`request_stream`), `tool_turn` (`process_tool_turn`), `tool` (`ToolRegistry::execute_tool`: `tool`/`tool_use_id`). Log lines therefore read `turn:tool_turn:tool: … Tool failed`.
- Levels: lifecycle at `info`, per-request/per-tool detail at `debug`, failures at `warn`/`error`. Interruptions/cancellations are `debug`, not `warn`.
- Use structured fields with a stable literal message: `tracing::warn!(duration_ms, code, error = %msg, "Tool failed")`. Use `%` for display, `?` for debug.
- Logging an `anyhow::Error` requires `%format!("{err:#}")`, never a bare `%err`. anyhow's `Display` prints only the outermost `.context(…)` layer, so `%err` silently drops the cause that explains the failure; `?err` does walk the chain but emits a multi-line `Caused by:` block and breaks one-event-one-line.
- The alternate flag is anyhow-specific. For a concrete error type (`reqwest::Error`, `serde_json::Error`, `JoinError`) `{:#}` changes nothing, so pass `error = &err as &dyn std::error::Error` instead: `tracing` dispatches that to `record_error`, and the fmt layer appends the walked chain as `error.sources=[…]` on the same line. `subscription_quota.rs` uses this form for reqwest transport errors, where the cause (DNS, TLS) is the whole diagnostic value.
- MUST NOT log inside per-token / per-SSE-event / per-bash-chunk paths (`consume_stream`'s event loop, `handle_stream_event`, the `ToolOutputDelta` path). Log stream *ends*, not stream items.
- MUST NOT put prompts, full messages, tool inputs, error bodies, or secrets in fields; use `skip_all` on `#[instrument]` and truncate messages (`truncate_for_error`, `truncate_tool_error`).
- Tokio tasks do not inherit spans. When spawning (e.g. concurrent tool execution), attach the parent explicitly with `.instrument(tracing::Span::current())` or the context is lost.
- `tracing_init` pins noisy dependencies (`h2`, `hyper`, `reqwest`, `rustls`, `ignore`, …) to `warn` so `ZDX_LOG=debug` shows ZDX events instead of HTTP/2 frame dumps. Naming a crate in `ZDX_LOG` opts back in (`ZDX_LOG=debug,h2=debug`).

## Checks

- Follow the root verification rules. Scoped tests: `cargo test -p zdx-engine --lib --bins --tests`.

## Adding or updating models

- Canonical workflow: the project skill `.zdx/skills/model-provider-mgmt/SKILL.md`.
- Provider model lists live in the `default_*_provider()` functions in `src/config.rs`. `default_config.toml` is generated from them; never edit it directly.
- Run `cargo build` before `just update-defaults`, so the binary embeds the latest `config.rs`.
- `zdx models update` fills model data from models.dev, then the embedded `default_models.toml`, then OpenRouter, and last a zero-priced "(custom)" placeholder that needs a manual fix.
- Ask the user before applying a change to their own `~/.zdx/config.toml` or models registry.
