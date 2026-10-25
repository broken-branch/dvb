# dvb — shared design rules

`src/design-system.css` owns tokens, components, spacing and motion.

- Use square surfaces and borders. Rows use `--row-gap` (16px); related icons use `--label-gap` (8px). Wrap before controls touch.
- Put help in focusable, tappable InfoTip controls. Keep goals, editable descriptions, decisions, transcripts and status data visible. Use text key names.
- The cockpit reads live engine data. Show connection/loading/error feedback and disable writes when unavailable. Now shows work needing attention and active projects; Projects compares project facts. Project pages contain the task board, conversation, activity, private brief and settings; Settings removes the project after one confirm that names it.
- Org chart: drag a non-root role card after an 8px pointer movement. Drop onto a role to report to it; receiving reports promotes a lead, and losing its last report removes the lead. Drop onto your own lead to swap positions, groups and direct reports. Branches move together. Before/after targets reorder siblings; group targets group siblings; the orchestrator and detach zone detach a branch. Invalid destinations reject drops. Space picks up/drops, arrows choose a destination, Escape cancels. Profiles edit role fields; role, hire and settings history can restore earlier values.
- The task board groups live tasks into Blocked, Planned, Working, In review, Changes requested, Checks running, Ready to merge, Stopped and Done / merged. Hide empty columns; scroll horizontally on narrow screens. Keep reasons visible. Cards link to task details and show role, active hire and applicable CI evidence. Done uses compact rows ordered by completion, showing ten before Show all.
- Task pages expose Stop, Resume or Retry as appropriate, and Send back with a note on a stopped or in-review task with a PR, plus changes, review, checks, decisions and feed. Preserve drafts when a write is refused. Use shared overlays and controls for profiles, history, questions and reports.
- The logo uses the Inner arc geometry and a pointer-responsive equalizer. Respect motion settings and system reduced motion; suspend animation in hidden tabs.
