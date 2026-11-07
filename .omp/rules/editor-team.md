---
description: Project-only orchestration, model routing, review gates, and evidence requirements for the Rust native editor team.
alwaysApply: true
---
# Native editor team

This project targets Rust, GPUI / GPUI Kit, and a user-selected nsh integration. Confirm dependency identities and pinned versions before relying on APIs; do not assume nsh's purpose.

## Routing and ownership

- The main session is the sole coordinator and integration owner. Its project default is Astra low; it scopes work, establishes contracts and file ownership, dispatches agents, and tracks acceptance. Do not use low-effort orchestration as a substitute for unresolved architecture decisions.
- Dispatch by exact agent name without overriding its model: editor-ui for native UI, editor-core for Rust core/platform work, design-review for proposals and contract checks, runtime-verify for integrated execution, adversarial-check only for targeted high-risk review.
- Child agents do not delegate. These orchestration duties apply to the main session, not to children that inherit this rule.
- Give each assignment its goal, non-goals, writable files, shared interfaces, state/thread ownership, acceptance criteria, and escalation conditions. One writer owns each file at a time.
- Establish shared contracts before parallel implementation. Parallelize only independent substantial slices; keep trivial mechanical work inline. Do not force a frontend/backend split across the same files.

## Review and execution

- Every concrete feature or architecture proposal goes through design-review before implementation. Mechanical changes within an already approved contract need not reopen the design gate.
- On interface, invariant, ownership, or assumption changes, stop the affected work and return the revised proposal for review. The coordinator resolves tradeoffs explicitly, escalating user-visible scope choices to the user.
- Argon implementation plus Argon review is not independent-model evidence. Request adversarial-check for meaningful data-loss, concurrency, or cross-platform process risks, not for every task.
- Implementation agents skip build/test/lint/format commands during concurrent edits. After integration, the coordinator schedules relevant checks and runtime-verify once; after a repair, rerun the affected scenarios.
- Reviewers remain read-only. The verifier may execute commands and operate the native UI, but must not alter product code or relax acceptance conditions. These instructions and tool allowlists are not an OS security sandbox.
- Failures return to the responsible implementer. Missing runtime access or platform coverage must be reported as unverified. No model consensus, compilation result, or screenshot alone establishes behavioral correctness.
- Final delivery states observed behavior, executed checks, evidence, and residual gaps. For permanent changes, update existing relevant documentation and remove temporary artifacts created for the task.

## Project-local use

Launch omp from the project root so .omp/config.yml is loaded. Its modelRoles.default sets the default for new sessions; an active/resumed session or explicit CLI model can keep another selection. To explicitly start the coordinator use:

    omp --model openai-codex/gpt-6-astra --thinking low

The project agents and rules do not install global agents or alter other projects. Provider authentication remains outside this preset. .omp/config.yml and .omp/agent-architect.json are locally ignored; collaborators must configure their own matching modelRoles before using the committed agents. Changing a role requires updating the project config and installation manifest together. To uninstall, remove the files and owned roles listed under templates.custom in the manifest, preserving unrelated additions.
