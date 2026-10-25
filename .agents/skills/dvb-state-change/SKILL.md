---
name: dvb-state-change
description: How to change the task state machine (TaskState, Command, decide, apply). Use when adding a state, a command or a transition.
---

# Changing the task state machine

The machine is `crates/dvb-core/src/state/`: `TaskState`, `Command`, `Context`,
`decide(state, command, context) -> Result<Vec<EventKind>, Refusal>` and
`apply(state, event) -> TaskState`. They are the only places a task's state changes.

1. Read the [core map](../../../docs/map/core.md), then find the relevant
   `TaskState`, `Command`, `decide` and `apply` arms in the checkout.
2. Add or change the `match` arm in `decide`; every `(state, command)` pair has one.
   A transition writes a `task.state` event (the only event `apply` moves state on)
   plus whatever the feed shows.
3. Extend the proptest model with the new state or command.
4. A new event kind goes in `EventKind` and `contracts/events.md` with its one
   wording; a new refusal is a new `RefusalCode`. Both are a `[contract]` PR.
5. Update the task projection in `crates/dvb-engine/src/proj/tasks.rs` if it reads the new state.
