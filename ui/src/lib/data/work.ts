import type { DataStore, Goal, Project, Question, Task } from './types';
import { activeTaskStates, isCurrentGoal, projectGoal } from './selectors';
import { stateLabels, word } from './display';
export const sevenDays = 7 * 86400000;
export const goalTitle = (text: string) => text.trim().split(/(?<=[.!?])\s|\n/)[0];
const list = new Intl.ListFormat('en', { type: 'conjunction' });
export const taskList = (names: string[]) => list.format(names);
/** Names task ids by their titles; text that is not a known task stays as written. */
const taskTitles = (tasks: Task[], ids: string[]) =>
  taskList(ids.map((id) => tasks.find((t) => t.id === id)?.title || id));
export function waitingText(tasks: Task[], t: Task) {
  return t.state.state === 'planned' && t.waiting_for?.length
    ? word('task.waiting-for', { tasks: taskTitles(tasks, t.waiting_for) })
    : undefined;
}
const partialWords = { out_of_role: 'out of my role', completion_unmet: 'done-when unmet' };
function outcomeText(tasks: Task[], t: Task) {
  const o = t.outcome;
  switch (o?.status) {
    case 'partial':
      return o.reason === 'other'
        ? `Stopped: ${o.detail}`
        : `Stopped: ${partialWords[o.reason]}: ${o.detail}`;
    case 'blocked':
      return `Blocked on: ${taskTitles(tasks, [o.prerequisite])}`;
    case 'complete':
    case undefined:
      return undefined;
  }
}
/** A waiting or held builder's own words; work the outcome no longer holds keeps its usual wording. */
export const taskNote = (tasks: Task[], t: Task) =>
  waitingText(tasks, t) ||
  (['stopped', 'failed'].includes(t.state.state) ? outcomeText(tasks, t) : undefined);
export const lastWork = (p: Project) => p.last_work_at ?? 0;
export const quietProject = (p: Project, now: number) => now - lastWork(p) >= sevenDays;
export function problems(s: DataStore, p: Project) {
  return s.tasks.filter(
    (t) =>
      t.project === p.id && ['failed', 'stopped'].includes(t.state.state) && !t.problem_superseded,
  );
}
type Need = 'question' | 'stopped' | 'failed';
const needsCache = new WeakMap<DataStore, Map<string, Need[]>>();
/** Why each item under Needs you is there, by project: an open question, or a stopped or failed task. */
function projectNeeds(s: DataStore, p: Project) {
  let byProject = needsCache.get(s);
  if (!byProject) {
    byProject = new Map();
    const { questions, faults } = needsYou(s);
    for (const [project, need] of [
      ...questions.map((q) => [q.project, 'question'] as const),
      ...faults.map(
        (f) =>
          [
            f.task.project,
            f.question ? 'question' : f.task.state.state === 'failed' ? 'failed' : 'stopped',
          ] as const,
      ),
    ])
      byProject.set(project, [...(byProject.get(project) ?? []), need]);
    needsCache.set(s, byProject);
  }
  return byProject.get(p.id) ?? [];
}
export const needs = (s: DataStore, p: Project) => projectNeeds(s, p).length;
export const needLabels = {
  question: 'Waiting for your answer',
  stopped: { one: 'Task stopped', many: 'Tasks stopped' },
  failed: { one: 'Task failed', many: 'Tasks failed' },
} as const;
/** Says why the project needs you; reasons of more than one kind read as plain Needs you. */
function needText(s: DataStore, p: Project) {
  const all = projectNeeds(s, p),
    kinds = new Set(all);
  if (kinds.size !== 1) return workLabels.attention.rail;
  const [kind] = kinds;
  return kind === 'question'
    ? needLabels.question
    : needLabels[kind][all.length === 1 ? 'one' : 'many'];
}
/** The project's last goal, by stated order, when it ended failed and its failed or stopped tasks still count. */
export function failedGoal(s: DataStore, p: Project): Goal | undefined {
  const { last } = projectGoal(s.goals, p.id, s.activity[p.id]?.goals);
  return last?.state === 'failed' && problems(s, p).some((t) => t.goal === last.id)
    ? last
    : undefined;
}
/** A current goal whose plan the owner put on hold; nothing asks or runs for it. */
const parkedGoal = (s: DataStore, p: Project) =>
  s.goals.find((g) => g.project === p.id && g.plan_parked && isCurrentGoal(g));
export const activeTask = (s: DataStore, p: Project) =>
  s.tasks.find((t) => t.project === p.id && activeTaskStates.has(t.state.state));
export const workLabels = {
  paused: { label: 'Paused', rail: 'Paused', tone: 'neutral' },
  working: { label: 'Working', rail: 'Working', tone: 'working' },
  failed: { label: 'Goal failed', rail: 'Goal failed', tone: 'bad' },
  attention: { label: 'Needs you', rail: 'Needs you', tone: 'warn' },
  parked: { label: word('goal.plan_parked'), rail: word('goal.plan_parked'), tone: 'neutral' },
  quiet: { label: 'Quiet', rail: '', tone: 'neutral' },
  ready: { label: 'Ready', rail: '', tone: 'neutral' },
} as const;
function workState(s: DataStore, p: Project, now: number) {
  if (p.paused) return 'paused';
  if (activeTask(s, p)) return 'working';
  if (failedGoal(s, p)) return 'failed';
  if (needs(s, p)) return 'attention';
  if (parkedGoal(s, p)) return 'parked';
  return quietProject(p, now) ? 'quiet' : 'ready';
}
export function railGroups(s: DataStore, now: number) {
  const ordered = s.projects.toSorted(
    (a, b) => lastWork(b) - lastWork(a) || a.name.localeCompare(b.name),
  );
  const groups: { label: string; projects: Project[] }[] = [
    'Working now',
    'Needs you',
    'Recent',
    'Quiet',
  ].map((label) => ({ label, projects: [] }));
  for (const p of ordered)
    groups[activeTask(s, p) ? 0 : needs(s, p) ? 1 : quietProject(p, now) ? 3 : 2].projects.push(p);
  return groups.filter((g) => g.projects.length);
}
export function railStatus(s: DataStore, p: Project, now: number) {
  const state = workState(s, p, now);
  if (state === 'working') return stateLabels[activeTask(s, p)!.state.state];
  if (state === 'attention') return needText(s, p);
  if (workLabels[state].rail) return workLabels[state].rail;
  if (!p.last_work_at) return 'No work yet';
  const day = new Date(now);
  day.setHours(0, 0, 0, 0);
  if (lastWork(p) >= +day) return 'today';
  day.setDate(day.getDate() - 1);
  return lastWork(p) >= +day
    ? 'yesterday'
    : new Date(lastWork(p)).toLocaleDateString('en-GB', {
        weekday: 'short',
        day: 'numeric',
        month: 'short',
      });
}
export function relative(at: number | null | undefined, now: number) {
  if (!at) return 'never';
  const minutes = Math.max(0, Math.floor((now - at) / 60000));
  if (minutes < 1) return 'just now';
  if (minutes < 60) return `${minutes} min ago`;
  const count = Math.floor(minutes / (minutes < 1440 ? 60 : 1440));
  const unit = minutes < 1440 ? 'hour' : 'day';
  return `${count} ${unit}${count === 1 ? '' : 's'} ago`;
}
export function duration(at: number | null | undefined, now: number) {
  return relative(at, now)
    .replace(' ago', '')
    .replace('just now', 'less than a minute')
    .replace('never', 'unknown');
}
export const workDate = (at: number | null | undefined) =>
  at
    ? `${new Date(at).toLocaleDateString('en-GB', { weekday: 'short', day: 'numeric', month: 'short' })} ${new Date(at).toLocaleTimeString('en-US', { hour: 'numeric', minute: '2-digit' })}`
    : 'No work yet';
export function progress(s: DataStore, p: Project) {
  const { current, last } = projectGoal(s.goals, p.id, s.activity[p.id]?.goals),
    goal = current || last;
  const tasks = s.tasks.filter((t) => t.goal === goal?.id);
  const done = tasks.filter((t) => t.state.state === 'done').length;
  return {
    goal,
    done,
    total: tasks.length,
    percent: tasks.length ? (done / tasks.length) * 100 : 0,
  };
}
export const overviewColumns = [
  'Project',
  'Where it is at',
  'Goal',
  'Progress',
  'Last work',
  'Goals finished',
  'Open problems',
] as const;
export type SortColumn = (typeof overviewColumns)[number];
export function sortProjects(
  s: DataStore,
  projects: Project[],
  column: SortColumn,
  descending: boolean,
  now: number,
) {
  const value = (p: Project): string | number => {
    const g = progress(s, p);
    switch (column) {
      case 'Project':
        return p.name;
      case 'Where it is at':
        return projectStatus(s, p, now).label;
      case 'Goal':
        return g.goal?.text || '';
      case 'Progress':
        return g.percent;
      case 'Last work':
        return lastWork(p);
      case 'Goals finished':
        return p.done_goals ?? 0;
      case 'Open problems':
        return problems(s, p).length;
    }
  };
  return projects.toSorted((a, b) => {
    const av = value(a),
      bv = value(b);
    return (
      (typeof av === 'number' && typeof bv === 'number'
        ? av - bv
        : String(av).localeCompare(String(bv))) * (descending ? -1 : 1) ||
      a.name.localeCompare(b.name)
    );
  });
}

/** The page's open question: the task's, else its project's oldest, else the oldest overall. */
export function pageQuestion(questions: Question[], project?: string, task?: string) {
  const open = questions
    .filter((q) => q.open)
    .toSorted((a, b) => (a.opened_at ?? 0) - (b.opened_at ?? 0));
  return (
    (task && open.find((q) => q.task === task)) ||
    (project && open.find((q) => q.project === project)) ||
    open[0]
  );
}
/** The engine's Needs you list: open questions, and its failed or stopped tasks, each with the question it folds. */
export function needsYou(s: DataStore) {
  const open = s.questions.filter((q) => q.open);
  const faults = (s.status.tasks ?? []).map((task) => ({
    task,
    question: open.find((q) => q.id === task.question),
  }));
  const folded = new Set(faults.map((f) => f.task.question));
  return { questions: open.filter((q) => !folded.has(q.id)), faults };
}
/** Paused, working, failed goal, attention, plan on hold, then quiet/ready is the shared status priority. */
export function projectStatus(
  s: DataStore,
  p: Project,
  now: number,
  compact = false,
): { tone: import('./tone').Tone; label: string } {
  const state = workState(s, p, now);
  const { tone, label } = workLabels[state];
  if (compact && state === 'working')
    return { tone, label: stateLabels[activeTask(s, p)!.state.state] };
  return { tone, label: compact && state === 'quiet' ? workLabels.ready.label : label };
}
