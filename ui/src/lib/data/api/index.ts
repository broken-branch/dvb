import type { components, paths } from '@/api/schema';
import { createSource, appVersion } from '../source';
import {
  mergeEvents,
  mergeEvidence,
  evidenceKinds,
  evidenceEventKinds,
  attentionEvent,
} from '../selectors';
import { createClient, type Fetch } from './client';
import { refusalText, eventText } from '../display';
import { readEvents } from './sse';
import type { ChartMove, Connection, DataStore, Event, EventFilter, Settings } from '../types';
type Schemas = components['schemas'];
type Call = ReturnType<typeof createClient>['call'];
/** Every engine and desk call; their types are DataStore's methods. */
function endpoints(
  call: Call,
  desk: Call,
  memoryChanged: (settings: Settings) => void,
  projectRemoved: (id: Schemas['ProjectId']) => void,
) {
  return {
    eventPage: (filter?: EventFilter) => call('/events/recent', 'get', { query: filter }),
    run: (id: Schemas['RunId']) => call('/runs/{id}', 'get', { params: { id } }),
    runOutput: (id: Schemas['RunId'], after?: Schemas['EventId']) =>
      call('/runs/{id}/output', 'get', { params: { id }, query: { after } }),
    diff: (id: Schemas['TaskId']) => call('/tasks/{id}/diff', 'get', { params: { id } }),
    verdict: (id: Schemas['TaskId']) => call('/tasks/{id}/verdict', 'get', { params: { id } }),
    usage: (filter?: NonNullable<paths['/usage']['get']['parameters']['query']>) =>
      call('/usage', 'get', { query: filter }),
    update: () => call('/update', 'get'),
    github: (signal?: AbortSignal) => call('/github', 'get', { signal }),
    githubRepositories: (signal?: AbortSignal) => call('/github/repositories', 'get', { signal }),
    discoverChecks: (repo: string, signal?: AbortSignal) =>
      call('/github/checks', 'get', { query: { repo }, signal }),
    retryGithubInstall: () => desk('/github/install', 'post'),
    // The desk's route adds the owner's git identity to the engine's stored values.
    githubPrefill: () => desk('/github/prefill', 'get'),
    startGithub: (
      body: Pick<Schemas['GithubManifestRequest'], 'organization' | 'owner_name' | 'owner_email'>,
    ) =>
      desk('/github/manifest', 'post', {
        path: '/github/start',
        // The authenticated desk supplies the state and redirect URL before forwarding to the engine.
        body: body as Schemas['GithubManifestRequest'],
      }),
    logins: () => call('/logins', 'get'),
    roleHistory: (id: Schemas['RoleId']) => call('/roles/{id}/history', 'get', { params: { id } }),
    hireHistory: (id: Schemas['HireId']) => call('/hires/{id}/history', 'get', { params: { id } }),
    settingsHistory: (key: Schemas['SettingsKey']) =>
      call('/settings/history', 'get', { query: { key } }),
    publication: () => call('/settings/publication', 'get'),
    brief: (id: Schemas['ProjectId']) => call('/projects/{id}/brief', 'get', { params: { id } }),
    projectIssues: (id: Schemas['ProjectId']) =>
      call('/projects/{id}/issues', 'get', { params: { id } }),
    issueCounts: (id: Schemas['ProjectId']) =>
      call('/projects/{id}/issues/counts', 'get', { params: { id } }),
    // The owner's verbs.
    setBrief: (id: Schemas['ProjectId'], body: Schemas['Brief']) =>
      call('/projects/{id}/brief', 'put', { params: { id }, body }),
    markProjectSeen: (id: Schemas['ProjectId']) =>
      call('/projects/{id}/seen', 'post', { params: { id } }),
    restoreRole: (id: Schemas['RoleId'], body: Schemas['Restore']) =>
      call('/roles/{id}/restore', 'post', { params: { id }, body }),
    restoreHire: (id: Schemas['HireId'], body: Schemas['Restore']) =>
      call('/hires/{id}/restore', 'post', { params: { id }, body }),
    restoreSettings: (body: Schemas['SettingsRestore']) =>
      call('/settings/restore', 'post', { body }),
    report: (body: Schemas['Report']) => call('/report', 'post', { body }),
    addProject: (body: Schemas['NewProject']) => call('/projects', 'post', { body }),
    pauseProject: (id: Schemas['ProjectId']) =>
      call('/projects/{id}/pause', 'post', { params: { id } }),
    resumeProject: (id: Schemas['ProjectId']) =>
      call('/projects/{id}/resume', 'post', { params: { id } }),
    removeProject: async (id: Schemas['ProjectId']) => {
      const result = await call('/projects/{id}/remove', 'post', { params: { id } });
      // The rail and lists drop the project before its event arrives.
      if (result.ok) projectRemoved(id);
      return result;
    },
    stateGoal: (id: Schemas['ProjectId'], body: Schemas['NewGoal']) =>
      call('/projects/{id}/goals', 'post', { params: { id }, body }),
    /** Asks the orchestrator to propose the next goal from the open trusted issues. */
    proposeGoal: (id: Schemas['ProjectId']) =>
      call('/projects/{id}/proposal', 'post', { params: { id } }),
    redirect: (id: Schemas['GoalId'], body: Schemas['Redirect']) =>
      call('/goals/{id}/redirect', 'post', { params: { id }, body }),
    dropGoal: (id: Schemas['GoalId']) => call('/goals/{id}/drop', 'post', { params: { id } }),
    /** Asks a plan on hold's question again, with no new plan. */
    resumeGoal: (id: Schemas['GoalId']) => call('/goals/{id}/resume', 'post', { params: { id } }),
    answer: (id: Schemas['QuestionId'], body: Schemas['Answer']) =>
      call('/questions/{id}/answer', 'post', { params: { id }, body }),
    stopTask: (id: Schemas['TaskId']) => call('/tasks/{id}/stop', 'post', { params: { id } }),
    resumeTask: (id: Schemas['TaskId']) => call('/tasks/{id}/resume', 'post', { params: { id } }),
    /** With text, also sends a stopped or in-review task with a PR back to its builder. */
    retryTask: (id: Schemas['TaskId'], body?: Schemas['RetryTask']) =>
      call('/tasks/{id}/retry', 'post', { params: { id }, body }),
    addRole: (id: Schemas['ProjectId'], body: Schemas['RoleFields']) =>
      call('/projects/{id}/roles', 'post', { params: { id }, body }),
    changeRole: (id: Schemas['RoleId'], body: Schemas['RoleChanges']) =>
      call('/roles/{id}', 'patch', { params: { id }, body }),
    moveChart: (id: Schemas['ProjectId'], body: ChartMove) =>
      call('/projects/{id}/chart', 'post', { params: { id }, body }),
    finishLogin: (vendor: Schemas['Vendor'], body: Schemas['LoginCode']) =>
      call('/logins/{vendor}', 'post', { params: { vendor }, body }),
    hire: (body: Schemas['NewHire']) => call('/hires', 'post', { body }),
    retire: (id: Schemas['HireId']) => call('/hires/{id}/retire', 'post', { params: { id } }),
    changeSettings: async (body: Settings) => {
      const result = await call('/settings', 'put', { body });
      // The live memory limit is not stored and emits no settings.changed event.
      if (result.ok && 'runs_memory_gb' in body.values) memoryChanged(result.value);
      return result;
    },
    setPublication: (body: Schemas['PublicationChange']) =>
      call('/settings/publication', 'put', { body }),
    startUpdate: () => call('/update', 'post'),
    startLogin: (vendor: Schemas['Vendor']) =>
      call('/logins/{vendor}', 'post', { params: { vendor } }),
  };
}
export type Endpoints = ReturnType<typeof endpoints>;
export function createLiveSource(
  options: {
    fetch?: Fetch;
    base?: string;
    eventsBase?: string;
    version?: string;
    reconnectMs?: number;
  } = {},
) {
  const source = createSource(),
    get = source.getSnapshot;
  const fetcher = options.fetch || fetch,
    base = options.base ?? '/api',
    eventsBase = options.eventsBase ?? '',
    version = options.version || appVersion();
  let ready = false,
    observed: Connection = { state: 'reconnecting' };
  const setConnection = (connection: Connection) => {
    observed = connection;
    source.set({
      connection:
        connection.state === 'connected' && !ready ? { state: 'reconnecting' } : connection,
    });
  };
  const client = createClient(fetcher, base, version, () => get().connection, setConnection);
  const deskClient = createClient(fetcher, '', version, () => get().connection, setConnection);
  const call = client.call;
  let controller: AbortController | undefined,
    timer: ReturnType<typeof setTimeout> | undefined,
    active = false,
    initialized = false,
    cursor = 0,
    failures = 0,
    epoch = 0,
    settingsGeneration = 0;
  function clearReadiness() {
    ready = false;
    if (get().connection.state !== 'version-mismatch')
      source.set({ connection: { state: 'reconnecting' } });
  }
  source.set(
    endpoints(
      call,
      deskClient.call,
      (settings) => {
        settingsGeneration++;
        source.set({ settings });
      },
      (id) => source.set({ projects: get().projects.filter((p) => p.id !== id) }),
    ),
  );
  // Refreshes are serialized. Events received during a read schedule another
  // pass; no delayed response or historical event can replace newer state.
  let activityDirty = false;
  const snapshotRoutes = {
    goals: '/goals',
    projects: '/projects',
    tasks: '/tasks',
    questions: '/questions',
    hires: '/hires',
    settings: '/settings',
    status: '/status',
    models: '/models',
  } as const satisfies Partial<Record<keyof DataStore, keyof import('@/api/schema').paths>>;
  type SnapshotKey = keyof typeof snapshotRoutes | 'roles';
  const snapshotKeys: SnapshotKey[] = [
    ...(Object.keys(snapshotRoutes) as (keyof typeof snapshotRoutes)[]),
    'roles',
  ];
  async function hydrate(signal: AbortSignal, generation: number, keys: SnapshotKey[]) {
    const settingsAtStart = settingsGeneration;
    const patch: Partial<DataStore> = {};
    await Promise.all(
      keys
        .filter((key) => key !== 'roles')
        .map(async (key) => {
          const r = await call(snapshotRoutes[key], 'get', { signal });
          if (!r.ok) throw new Error(refusalText(r.refusal));
          Object.assign(patch, { [key]: r.value });
        }),
    );
    if (keys.includes('roles')) {
      const roles: DataStore['roles'] = {};
      await Promise.all(
        (patch.projects || get().projects).map(async (p) => {
          const r = await call('/projects/{id}/roles', 'get', {
            params: { id: p.id },
            signal,
          });
          if (!r.ok) throw new Error(refusalText(r.refusal));
          roles[p.id] = r.value;
        }),
      );
      patch.roles = roles;
    }
    if (activityDirty) {
      activityDirty = false;
      const summary = await call('/events/recent', 'get', {
        query: { limit: 1, summary: true },
        signal,
      });
      if (!summary.ok) throw new Error(refusalText(summary.refusal));
      patch.activity = Object.fromEntries(
        summary.value.activity.map((a) => [
          a.project,
          {
            latest: Math.max(a.latest, get().activity[a.project]?.latest ?? 0),
            fresh: a.fresh,
            goals: a.goals,
          },
        ]),
      );
    }
    // A confirmed memory save supersedes settings reads already in flight.
    if (settingsAtStart !== settingsGeneration) delete patch.settings;
    if (active && generation === epoch && !signal.aborted) source.set(patch);
  }
  const dirtyKeys = new Set<SnapshotKey>();
  let refreshing: Promise<void> | undefined;
  function refresh(
    signal: AbortSignal,
    generation: number,
    keys: readonly SnapshotKey[] = snapshotKeys,
  ) {
    keys.forEach((key) => dirtyKeys.add(key));
    if (!refreshing) {
      let completed = false;
      refreshing = (async () => {
        do {
          const keys = [...dirtyKeys];
          dirtyKeys.clear();
          await hydrate(signal, generation, keys);
        } while (
          (dirtyKeys.size || activityDirty) &&
          active &&
          generation === epoch &&
          !signal.aborted
        );
        completed = true;
      })().finally(() => {
        refreshing = undefined;
        if (
          completed &&
          (dirtyKeys.size || activityDirty) &&
          active &&
          generation === epoch &&
          !signal.aborted
        )
          return refresh(signal, generation, []);
      });
    }
    return refreshing;
  }
  function changed(e: Event): SnapshotKey[] {
    if (e.kind === 'project.added') return ['projects', 'roles'];
    if (e.kind === 'project.removed')
      return ['projects', 'roles', 'goals', 'tasks', 'questions', 'status'];
    if (e.kind === 'project.paused' || e.kind === 'project.resumed') return ['projects'];
    if (e.kind.startsWith('role.')) return ['roles'];
    if (e.kind.startsWith('hire.')) return ['hires'];
    if (e.kind === 'settings.changed') return ['settings'];
    if (
      e.kind.startsWith('goal.') ||
      e.kind.startsWith('plan.') ||
      e.kind.startsWith('question.') ||
      e.kind.startsWith('task.') ||
      e.kind === 'engine.reconciled'
    )
      return ['projects', 'goals', 'tasks', 'questions', 'status'];
    if (e.kind === 'run.started' || e.kind === 'run.ended')
      return ['projects', 'tasks', 'status', 'questions', 'goals'];
    if (e.kind === 'run.rerouted') return ['projects', 'tasks'];
    if (e.kind.startsWith('run.')) return ['projects'];
    if (e.kind.startsWith('branch.') || e.kind.startsWith('pr.')) return ['projects', 'tasks'];
    if (['checks.', 'verdict.', 'merge.', 'effect.'].some((prefix) => e.kind.startsWith(prefix)))
      return ['projects'];
    return [];
  }
  async function connect(generation: number) {
    source.reportIssue('');
    clearReadiness();
    const connectionController = new AbortController();
    controller = connectionController;
    const signal = connectionController.signal;
    try {
      await refreshing?.catch(() => {});
      const v = await call('/version', 'get', { signal });
      if (!v.ok) throw new Error(refusalText(v.refusal));
      client.version(v.value.version);
      // Capture the boundary BEFORE reading snapshots. Stream events invalidate
      // projections, including changes that raced any of the snapshot requests.
      const page = await call('/events/recent', 'get', { query: { summary: true }, signal });
      if (!page.ok) throw new Error(refusalText(page.refusal));
      if (!active || generation !== epoch) return;
      if (!initialized) {
        initialized = true;
        cursor = page.value.cursor;
        source.set({ events: mergeEvents(get().events, page.value.events) });
      }
      source.set({
        activity: Object.fromEntries(
          page.value.activity.map((a) => [
            a.project,
            { latest: a.latest, fresh: a.fresh, goals: a.goals },
          ]),
        ),
      });
      await refresh(signal, generation);
      if (!get().evidence.length) {
        let before: number | undefined;
        do {
          const r = await call('/events/recent', 'get', {
            query: {
              kinds: evidenceKinds,
              latest: true,
              before,
              limit: 200,
            },
            signal,
          });
          if (!r.ok) throw new Error(refusalText(r.refusal));
          if (!active || generation !== epoch) return;
          source.set({ evidence: mergeEvidence(get().evidence, r.value.events) });
          before = r.value.before ?? undefined;
        } while (before);
      }
      if (!active || generation !== epoch) return;
      const response = await fetcher(`${eventsBase}/events?after=${cursor}`, {
        credentials: 'same-origin',
        signal,
        headers: { Accept: 'text/event-stream', 'Last-Event-ID': String(cursor) },
      });
      client.version(response.headers.get('dvb-version'));
      if (!response.ok || !response.body) throw new Error('Unable to follow the live feed.');
      failures = 0;
      await readEvents(
        response.body,
        (data) => {
          if (!active || generation !== epoch) return;
          const event = JSON.parse(data) as Event;
          if (event.id <= cursor) return;
          source.set({
            usageVersion: ['run.usage', 'vendor.limited', 'vendor.resumed'].includes(event.kind)
              ? event.id
              : get().usageVersion,
            events: mergeEvents(get().events, [event]).slice(-200),
            evidence: evidenceEventKinds.some((kind) => kind === event.kind)
              ? mergeEvidence(get().evidence, [event])
              : get().evidence,
          });
          if (event.project && eventText(event)) {
            const activity = get().activity[event.project];
            if (activity)
              source.set({
                activity: {
                  ...get().activity,
                  [event.project]: { ...activity, latest: Math.max(activity.latest, event.id) },
                },
              });
          }
          if (
            attentionEvent(event) ||
            ['goal.stated', 'goal.approved', 'project.seen'].includes(event.kind) ||
            (event.kind === 'task.state' &&
              ['done', 'failed', 'stopped'].includes(event.payload.to.state)) ||
            event.kind === 'engine.reconciled'
          )
            activityDirty = true;
          const keys = changed(event);
          if (keys.length || activityDirty) {
            void refresh(signal, generation, keys).catch((e) => {
              source.reportIssue(e instanceof Error ? e.message : 'Unable to refresh state.');
              connectionController.abort();
            });
          }
          if (event.kind === 'engine.started') client.version(event.payload.version);
          cursor = event.id;
        },
        () => {
          if (active && generation === epoch) {
            void refresh(signal, generation, [])
              .then(() => {
                if (active && generation === epoch && !signal.aborted) {
                  ready = true;
                  source.set({ connection: observed });
                }
              })
              .catch(() => connectionController.abort());
          }
        },
      );
      connectionController.abort();
    } catch (e) {
      if (!signal.aborted)
        source.reportIssue(e instanceof Error ? e.message : 'Unable to connect.');
    }
    connectionController.abort();
    if (!active || generation !== epoch) return;
    clearReadiness();
    timer = setTimeout(
      () => connect(generation),
      Math.min((options.reconnectMs ?? 1000) * 2 ** failures++, 30000),
    );
  }
  return {
    ...source,
    start() {
      if (active) return;
      active = true;
      void connect(++epoch);
    },
    dispose() {
      active = false;
      epoch++;
      clearReadiness();
      clearTimeout(timer);
      controller?.abort();
    },
  };
}
