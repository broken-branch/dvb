# Code maps

Read `docs/map/<area>.md` before working in an area. Start with the owning
map and follow its neighbor entry points when a change crosses areas.

| Map | Owns |
|---|---|
| [Engine core](core.md) | Shared types, task decisions, events/store, projections, dispatch and recovery |
| [Engine APIs and effects](engine-rest.md) | Owner/run APIs, sockets, publication, usage and updates |
| [Workers, installation and CI](run.md) | Worker preparation, preflight, provider CLIs, execution and collection |
| [Forge, owner CLI and desk](forge.md) | GitHub/Fake forge, App setup, tokens, PRs, checks and board mirror |
| [Cockpit UI](ui.md) | React cockpit, API client, state/data, components and themes |

Each map is a module index of its area with reading entry points; behaviour
lives in the code and its tests. Update the owning map when its modules or entry
points change. Checks and release
procedures are in [testing.md](../testing.md); repository rules are in
[AGENTS.md](../../AGENTS.md).
