import { word } from './display';
import type { Goal, TaskState, Event } from './types';
export const activeTaskStates = new Set<TaskState['state']>([
  'preparing',
  'running',
  'collecting',
  'publishing',
  'in_review',
  'awaiting_checks',
  'merging',
  'retry_pending',
]);
export const isCurrentGoal = (g: Goal) => ['stated', 'approved', 'in_progress'].includes(g.state);
function projectGoals(goals: Goal[], project: string, order?: string[]) {
  const mine = goals.filter((g) => g.project === project);
  if (order?.length) {
    const rank = new Map(order.map((id, index) => [id, index]));
    mine.sort((a, b) => (rank.get(a.id) ?? order.length) - (rank.get(b.id) ?? order.length));
  }
  return mine;
}
export function projectGoal(goals: Goal[], project: string, order?: string[]) {
  const mine = projectGoals(goals, project, order);
  const current = mine.findLast(isCurrentGoal);
  return { current, last: mine.at(-1) };
}
/** The project's current goals in stated order, those already under way first. */
export function currentGoals(goals: Goal[], project: string, order?: string[]) {
  const current = projectGoals(goals, project, order).filter(isCurrentGoal);
  return [
    ...current.filter((g) => g.state !== 'stated'),
    ...current.filter((g) => g.state === 'stated'),
  ];
}
export const lastGoalText = (goal?: Goal) =>
  goal ? word(`goal_last.${goal.state}`) : word('goal_ready');
export const attentionEvent = (e: Event) => attentionKinds.some((kind) => kind === e.kind);
export function mergeEvents(...pages: Event[][]) {
  return [...new Map(pages.flat().map((e) => [e.id, e])).values()].sort((a, b) => a.id - b.id);
}

/** Board/header evidence retains one latest observation per displayed object. */
export function mergeEvidence(...pages: Event[][]) {
  const latest = new Map<string, Event>();
  for (const e of pages.flat()) {
    const task = 'task' in e.payload ? e.payload.task : '';
    const goal = 'goal' in e.payload ? e.payload.goal : '';
    const review = e.kind === 'run.started' && !!e.payload.reviews;
    const key = `${e.project}/${e.kind}/${task}/${goal}/${review}`;
    if (e.id > (latest.get(key)?.id ?? -1)) latest.set(key, e);
  }
  return [...latest.values()].sort((a, b) => a.id - b.id);
}

export const evidenceEventKinds = [
  'run.started',
  'checks.observed',
  'verdict.recorded',
  'github.mirror_observed',
] as const;
export const evidenceKinds = evidenceEventKinds.join(',');
const attentionKinds = ['question.opened', 'task.done', 'task.failed', 'goal.done'] as const;
export function taskRunIds(events: Event[], task: string) {
  return [
    ...new Set(
      events.flatMap((e) =>
        (e.kind === 'run.started' || e.kind === 'task.started') && e.payload.task === task
          ? [e.payload.run]
          : [],
      ),
    ),
  ];
}
export const parseChecks = (checks: string) =>
  checks
    .split(',')
    .map((name) => name.trim())
    .filter(Boolean);
