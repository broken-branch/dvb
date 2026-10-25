import type { Project, Goal, Task, EventPage, Event } from '@/lib/data/types';
export const project: Project = {
  id: 'p',
  name: 'Example',
  repo: 'example-org/example',
  default_branch: 'main',
  paused: false,
  ci: 'no_ci',
};
export const goal = (id: string, state: Goal['state']): Goal => ({
  id,
  project: 'p',
  text: `Goal ${id}`,
  state,
});
export const task = (id: string, goal: string, state: Task['state']): Task => ({
  id,
  project: 'p',
  goal,
  title: `Task ${id}`,
  role: 'frontend',
  review: 'design',
  state,
  head: null,
  pr: null,
});
export const event = (id: number, kind: string, payload: object): Event =>
  ({ id, at: id, project: 'p', actor: 'engine', kind, payload }) as Event;
export const page = (events: Event[] = [], before: number | null = null): EventPage => ({
  cursor: 100,
  events,
  before,
  activity: [{ project: 'p', latest: 100, fresh: 9, goals: ['g'] }],
});
