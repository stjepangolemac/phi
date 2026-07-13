# Phi

Phi is an experimental, self-modifiable agent harness with a small Rust kernel and an agent policy written in [Steel](https://github.com/mattwparas/steel), an embeddable Scheme implemented in Rust.

The goal is not to reimplement an async runtime, HTTP stack, terminal framework, or every provider in Rust. Rust supplies a boring, trustworthy execution substrate; Steel defines nearly all agent behavior and can propose improvements to itself.

## Architecture

```text
Steel policy and plugins
  agent loop, prompts, providers, tools, routing, compaction,
  planning, delegation, retries, evaluation, improvement
                         |
                    effects/events
                         |
Stable Rust kernel
  runtime events, Steel VM, HTTP and streaming transport, filesystem/process
  capabilities, secrets, scheduling, cancellation, event log,
  resource enforcement, policy versions and rollback
```

Rust owns mechanisms whose failure could expose secrets, violate isolation, lose history, defeat cancellation, or prevent recovery. Everything describing agent behavior should be replaceable Steel code.

## Event/effect boundary

Steel is synchronous and event-driven. Rust owns async work and sends completion events back to Steel; native async support in the policy language is therefore unnecessary.

The policy exports a minimal interface:

```scheme
(init config)             ; -> state
(on-event state event)    ; -> (state effects)
```

Typical events include user messages, model stream events, tool results, job failures, and cancellations. Typical effects include:

```scheme
'(http-request ...)
'(run-tool ...)
'(spawn-job ...)
'(checkpoint ...)
'(finish ...)
```

Rust validates every effect before executing it. Steel can request a shell command or use a secret handle, but cannot grant itself permission or read the underlying secret.

## Plugins

Providers should be Steel plugins over generic Rust HTTP and streaming primitives. A provider plugin builds requests, interprets provider-specific stream events, normalizes messages and tool calls, and classifies errors.

Other Steel-level plugins can implement:

- tools and argument schemas;
- prompts and context compaction;
- model routing, retries and failover;
- permission and approval policy;
- sessions, memory and retrieval;
- planning, subagents and delegation;
- MCP protocol behavior;
- CLI commands, hooks and output formatting;
- evaluation and self-improvement strategies.

Rust should retain raw connection/framing reliability, capability enforcement, job scheduling, durable event logging, secret handling, and recovery.

## Self-modification

The active policy never overwrites itself. It observes traces, identifies a measurable weakness, and submits a candidate policy with a hypothesis. Prefer structural Scheme/AST transformations over fragile text editing.

```text
observe -> diagnose -> propose candidate -> parse/lint -> test
        -> replay -> benchmark -> canary -> promote or rollback
```

The Rust kernel stores immutable policy versions and evaluates candidates in isolation. Promotion is atomic and initially requires user approval. Later versions may promote automatically only within kernel-enforced thresholds. Existing sessions may remain pinned to their starting policy version.

Possible learned improvements include reducing redundant file reads, adapting context compaction, routing simple work to cheaper models, choosing when to plan or parallelize, adding recovery rules, and turning repeated command sequences into typed tools.

## POC scope

The first useful version needs:

- one Steel VM and the `init`/`on-event` contract;
- generic HTTP plus SSE transport;
- one provider implemented in Steel;
- `read_file`, `apply_patch`, and bounded `shell` capabilities;
- Tokio-owned jobs, timeouts, and cancellation;
- append-only JSONL session events;
- active and candidate policy versions;
- fixtures for checking and replaying a candidate;
- explicit approval before activation.

A tentative Rust layout:

```text
src/
  main.rs
  protocol.rs       # events and effects
  steel_runtime.rs  # policy loading and invocation
  transport.rs      # HTTP, SSE and async jobs
  capabilities.rs   # filesystem, patch and shell enforcement
  session.rs        # append-only event log
  policy_store.rs   # candidates, activation and rollback
policy/
  agent.scm
  providers/
tests/
  fixtures/
```

Defer multiple providers, database, native Steel async, MCP, subagents, automatic promotion, and canary deployment until the basic loop and candidate replay work.

## Why Steel?

Scheme is small, interpretable, and well suited to manipulating messages, policies, and programs as data. Steel keeps the interpreter in Rust while enabling a compact agent DSL, macros, live reload, and eventually structural self-modification. Rust remains the stable constitution; Steel is the evolving policy.

## Current WIP

The repository is a Cargo workspace:

- `phi-protocol`: provider-neutral events and effects;
- `phi-core`: trusted filesystem, process, and JSONL session mechanisms;
- `phi-eval`: isolated policy candidate validation;
- `phi-runtime`: frontend-neutral agent loop and typed runtime events;
- `phi-steel`: sandboxed Steel policy loading and invocation;
- `phi-cli`: the thin executable;
- `phi-tui`: a thin Ratatui frontend;
- `policy/providers/openai.scm`: OpenAI-specific provider behavior.
- `policy/compaction/simple.scm`: replaceable context compaction policy.

The OpenAI plugin calls the Responses endpoint directly using the existing ChatGPT login. Rust owns generic HTTP/SSE transport and injects configured secret handles without exposing tokens to Steel. `phi.json` selects the policy, provider plugin, network allowlist, and secret mapping.

```sh
cargo build
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings

cargo run -p phi-cli -- check-policy
cargo run -p phi-cli -- read README.md
cargo run -p phi-cli -- shell pwd
cargo run -p phi-cli -- run "Reply with exactly: phi works"
cargo run -p phi-cli -- --json run "Stream this response"
cargo run -p phi-tui
```

Human output streams text deltas. `--json` emits JSONL lifecycle events and deltas. Each run prints a session ID and writes state plus events under `.phi/sessions/<id>/`; continue it with:

```sh
cargo run -p phi-cli -- resume SESSION_ID "Continue the conversation"
```

Model tools cross the policy boundary as a name plus JSON arguments. The kernel currently provides confined `read_file`, revision-checked atomic `replace_file`, bounded direct-program `shell`, and candidate submission. Shell and general writes require explicit approval flags:

```sh
cargo run -p phi-cli -- --allow-shell run "Run cargo test"
cargo run -p phi-cli -- --allow-write run "Update the requested file"
```

Steel stores provider-neutral message, tool-call, and tool-result history. It tracks estimated and provider-reported usage and invokes the configured compaction plugin before requests when context exceeds its budget.

The TUI streams the same runtime events as the CLI. Enter sends, Ctrl-Enter inserts a newline, Esc cancels active work, and Ctrl-C exits when idle. Shell and write requests show a one-shot approval prompt unless pre-approved with `--allow-shell` or `--allow-write`. Resume a session with:

```sh
cargo run -p phi-tui -- --session SESSION_ID
```

Candidate policies are parsed before storage and require explicit activation:

```sh
cargo run -p phi-cli -- policy-candidate policy/agent.scm
cargo run -p phi-cli -- policy-activate CANDIDATE_ID
```

Phi can perform a constrained policy-improvement pass:

```sh
cargo run -p phi-cli -- run \
  "Inspect policy/agent.scm, propose one small measurable improvement, submit it as a candidate, and stop for approval."
```

Policy submission checks Steel loading, a replay fixture, formatting, tests, and Clippy, then returns a diff. Activation remains a separate manual command.
