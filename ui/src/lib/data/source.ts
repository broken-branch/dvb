import type { DataStore } from './types';
// The desk injects the version of the binary that embeds this bundle; there is no literal here.
export const appVersion = () =>
  document.querySelector<HTMLMetaElement>('meta[name="dvb-version"]')?.content || '';
export type DataSource = {
  getSnapshot(): DataStore;
  subscribe(listener: () => void): () => void;
  getIssue?(): string;
  start(): void;
  dispose(): void;
};
export function createSource() {
  let snapshot: Partial<DataStore> = {
    connection: { state: 'reconnecting' },
    status: { running: 0, needs_you: 0 },
    models: { models: [] },
    projects: [],
    goals: [],
    tasks: [],
    questions: [],
    roles: {},
    hires: [],
    settings: { values: {} },
    usageVersion: 0,
    events: [],
    evidence: [],
    activity: {},
  };
  let issue = '';
  const listeners = new Set<() => void>();
  return {
    // The live source installs actions before exposing this snapshot.
    getSnapshot: () => snapshot as DataStore,
    getIssue: () => issue,
    reportIssue: (message: string) => {
      issue = message;
      snapshot = { ...snapshot };
      listeners.forEach((listener) => listener());
    },
    subscribe: (listener: () => void) => {
      listeners.add(listener);
      return () => {
        listeners.delete(listener);
      };
    },
    set: (patch: Partial<DataStore>) => {
      snapshot = { ...snapshot, ...patch };
      listeners.forEach((listener) => listener());
    },
    start() {},
    dispose() {},
  };
}
