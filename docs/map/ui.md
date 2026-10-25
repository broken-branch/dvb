# Cockpit UI

The React cockpit lets the owner manage projects, goals, questions and roles,
inspect tasks and usage, and change settings. `dvb-desk` embeds `ui/dist`, serves
it on loopback and proxies requests/events to the engine's Unix socket.
Execution, authorization, storage and merge decisions belong to the backend.

## Main modules

Paths below are under `ui/`.

- `src/main.tsx`, `App.tsx`, `lib/routes.ts`: startup, providers, app notices and hash routes.
- `src/components/dvb/workspace.tsx`: navigation, shortcuts, command menu and keyboard guide.
- `src/lib/store.tsx`: engine source lifecycle and shared React subscriptions.
- `src/lib/data/api/`: typed HTTP client, hydration, writes and SSE connection.
- `src/lib/data/pages.ts`, `selectors.ts`: bounded history and evidence selection.
- `src/lib/data/display.ts`, `work.ts`, `board.ts`: wording, work facts (Needs you, goal
  states, task notes) and board cards. `needsYou` lists open questions and the tasks of
  `GET /status` `tasks`, folding each task's `question` into its row; Home, `needs`
  and the project nav count all use it.
- `src/screens/home.tsx`, `project.tsx`: Now, Projects and project work views.
- `src/screens/task.tsx`, `task/`: task actions, changes, review, checks and feed.
- `src/screens/decision.tsx`, `company.tsx`: owner answers, org chart and role profiles.
- `src/screens/usage.tsx`, `settings.tsx`, `setup.tsx`, `github.tsx`: usage, preferences,
  first use and forge connection.
- `src/components/dvb/`: shared task board, org chart, history, feeds, goal issues, login and
  project-checks components.
- `src/components/ui/`: reusable controls and overlays.
- `src/design-system.css`, `DESIGN.md`: shared styles and interaction rules.
- `src/lib/appearance.tsx`, `theme.ts`, `palettes.ts`: appearance and palette.
- `src/lib/desk-theme.ts`, `desk-navigation.ts`: desktop palette and navigation.
- `public/icons/`: app icon and favicon; `node scripts/icons.mjs` renders the PNGs.
- `src/api/schema.ts`, `src/api/wording.json`: generated wire types and wording catalogue;
  regenerate rather than hand-edit.

## Start reading

Follow a route from `App.tsx` to its screen, then the store and data client.
Read [DESIGN.md](../../ui/DESIGN.md) before changing interactions or styling.
Vite and Vitest configuration are at the UI root. See [forge/desk](forge.md) and
[engine APIs](engine-rest.md) for the backend boundary.
