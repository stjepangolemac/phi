---
name: planning
description: Create, maintain, resume, and finish durable per-conversation plans for nontrivial multi-step work.
---

# Planning

Store plans in the current conversation's durable `plans/` directory under Phi home. Do not create a plan for a simple request that can be completed directly.

## Start or resume

Before planning new work, inspect the current session's `plans/` directory when its path is available in prior tool output or conversation context. Read the relevant existing plan and resume from its stage, current task, acceptance criteria, blockers, and notes instead of reconstructing progress from conversation history. Multiple plans may coexist; infer the relevant plan from the user's request and the plan contents rather than maintaining an active-plan pointer.

When starting a plan, call `create_plan` with a short descriptive name and the complete initial Markdown. The helper atomically allocates a zero-padded, monotonically increasing filename such as `0001-session-storage.md` in the current session. Use the returned path for subsequent reads and edits. Do not manually choose a number, create workspace `.phi/PLAN.md`, modify Git excludes, or introduce current-plan, archive, index, or database state.

Create a human-readable Markdown file with this shape:

```markdown
# Goal

What we are trying to accomplish.

**Stage:** writing

# Acceptance Criteria

- [ ] The requested behavior works as agreed with the user.
- [ ] Relevant validation passes.

# Tasks

- [ ] Inspect the relevant code and constraints.
- [ ] Implement the smallest coherent change.
- [ ] Validate and review the result.

# Blockers

- None.

# Notes

Important decisions and context needed to resume work.
```

While the plan is in `writing`, gather enough relevant context from the user and workspace to make the goal, acceptance criteria, and approach reliable. Ask targeted questions when the user's intent or an important constraint is unclear. Read files and perform non-mutating discovery as needed, but do not begin implementation. Present the completed plan to the user and keep it in `writing` until the user explicitly approves it; clarifications and suggested edits are not approval.

When the user approves the plan, edit the returned durable plan path to set its stage to `executing` and mark the first current task with `[>]` before implementing. When `context_mark` is available, mark the start of writing and the switch to execution with concise descriptive labels. The plan file remains the durable execution state; context markers only create optional compaction boundaries.

## Maintain

- Keep one stage for the plan as a whole: `writing`, `executing`, or `done`.
- Use `writing` while gathering context and awaiting explicit user approval, `executing` while carrying out the approved work, and `done` only when nothing remains to do.
- Keep one flat task list. Do not add milestones, subtasks, dependency graphs, or user-visible stable task IDs.
- Use `[ ]` for pending tasks, `[>]` for the current task, and `[x]` for completed tasks. During `executing`, mark exactly one task `[>]` until all tasks are complete; use no `[>]` marker during `writing` or `done`.
- Keep acceptance criteria as a separate checklist using `[ ]` and `[x]`. Check each criterion only after verifying the outcome, not merely after implementing a related task.
- After approval, use best judgment to revise tasks, acceptance criteria, and implementation details as new information appears, and continue autonomously without returning to `writing` or requesting reapproval. Pause only for a genuine blocker, required user decision, or permission or safety boundary.
- Check off completed tasks and update acceptance criteria, blockers, or notes at meaningful checkpoints, not after every small action.
- Keep enough decisions, validation results, and resume context in the plan to continue without relying on the full conversation.
- When `context_mark` is available, use the same generic marker when the current task materially changes. Do not mark trivial steps or assume markers grant permissions.

## Finish

When all tasks are complete, all acceptance criteria have been verified and checked, and nothing else remains to do, set the plan stage to `done` and record any final context needed for handoff. Keep completed plans in the session's `plans/` directory; never delete, archive, or move them automatically.

When `context_mark` is available, a final marker may close the completed planning/execution span before handoff. Planning is only one producer of these generic boundaries; no active plan is required to use them.

This workflow is intentionally skill-first and uses normal file-reading and editing tools rather than a slash-command suite or machine-heavy schema. Its interaction model takes design inspiration from the planning workflows in OpenAI Codex and Claude Code.
