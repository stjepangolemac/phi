---
name: planning
description: Create, maintain, resume, and finish a lightweight local plan for nontrivial multi-step work.
---

# Planning

Use `.phi/PLAN.md` as persistent execution state for nontrivial, multi-step work. Do not create a plan for a simple request that can be completed directly.

## Start or resume

Before planning new work, check for `.phi/PLAN.md`. If it exists, read it and resume from its stage, current task, acceptance criteria, blockers, and notes instead of reconstructing progress from conversation history. This is also how work resumes after a new session or compaction.

When starting a plan in a Git workspace, add the plan's repository-relative path to the repository-local exclude file reported by `git rev-parse --git-path info/exclude`. Do not modify `.gitignore` or another shared ignore file. Verify the plan is ignored before continuing.

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

When the user approves the plan, set its stage to `executing` and mark the first current task with `[>]` before implementing. When `context_mark` is available, mark the start of writing and the switch to execution with concise descriptive labels. The plan file remains the durable execution state; context markers only create optional compaction boundaries.

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

When all tasks are complete, all acceptance criteria have been verified and checked, and nothing else remains to do, set the plan stage to `done` and record any final context needed for handoff. Keep `.phi/PLAN.md`; never delete or commit it.

When `context_mark` is available, a final marker may close the completed planning/execution span before handoff. Planning is only one producer of these generic boundaries; no active plan is required to use them.

This workflow is intentionally skill-first and uses normal file-reading and editing tools rather than a slash-command suite or machine-heavy schema. Its interaction model takes design inspiration from the planning workflows in OpenAI Codex and Claude Code.
