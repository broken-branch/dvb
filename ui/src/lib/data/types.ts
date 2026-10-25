import type { components, paths } from '@/api/schema';
import type { Endpoints } from './api';

type Schemas = components['schemas'];

export type EventPage = Schemas['EventPage'];
export type EventFilter = NonNullable<paths['/events/recent']['get']['parameters']['query']>;
export type EventKind = Schemas['EventKind'];
export type Event = Schemas['Event'];
export type Project = Schemas['ProjectView'];
export type NewProject = Schemas['NewProject'];
export type IssueCounts = Schemas['IssueCounts'];
export type Goal = Schemas['GoalView'];
export type Question = Schemas['QuestionView'];
export type Task = Schemas['TaskView'];
export type TaskState = Schemas['TaskState'];
export type Role = Schemas['RoleRecord'];
export type RoleFields = Schemas['RoleFields'];
export type RoleChanges = Schemas['RoleChanges'];
export type Hire = Schemas['HireView'];
export type SettingsKey = Schemas['SettingsKey'];
export type Settings = Schemas['Settings'];
// Require each variant's destination explicitly; null means detach, ungroup or append.
type ExplicitChartDestination<T> = T extends unknown
  ? T & Required<Pick<T, Extract<keyof T, 'parent' | 'group' | 'before'>>>
  : never;
export type ChartMove = ExplicitChartDestination<Schemas['ChartMove']>;
export type Usage = Schemas['Usage'];
export type LoginStarted = Schemas['LoginStarted'];
export type Logins = Schemas['Logins'];
export type ReportKind = Schemas['ReportKind'];
export type ReportPreview = Schemas['ReportPreview'];
export type Vendor = Schemas['Vendor'];
export type Refusal = Schemas['Refusal'];

/** The link to the engine. Another engine version blocks writes, reads go on. */
export type Connection =
  | { state: 'connected'; engineVersion: string }
  | { state: 'reconnecting' }
  | { state: 'version-mismatch'; engineVersion: string; appVersion: string };

/** A call's outcome: the value, or a refusal with a code (never prose to match on). */
export type ApiResult<T = void> = { ok: true; value: T } | { ok: false; refusal: Refusal };

/** The projections, kept current from /events, and the calls. */
export type DataStore = Endpoints & {
  connection: Connection;
  status: Schemas['Status'];
  projects: Project[];
  goals: Goal[];
  tasks: Task[];
  questions: Question[];
  roles: Record<Schemas['ProjectId'], Role[]>;
  hires: Hire[];
  settings: Settings;
  models: Schemas['ModelPool'];
  /** Invalidates usage reads independently of retained feed rows. */
  usageVersion: number;
  /** The feed, in event id order. */
  events: Event[];
  evidence: Event[];
  activity: Record<string, { latest: number; fresh: number; goals?: string[] }>;
};
