import { vi } from 'vitest';
import { beforeEach } from 'vitest';
import { describe, expect, it } from 'vitest';
import { createFixtureSource } from '../../support/render';
import { railGroups, railStatus, sevenDays, relative, needs } from '@/lib/data/work';
beforeEach(() => vi.spyOn(Date, 'now').mockReturnValue(1790811300000));
describe('work navigation', () => {
  it('uses the exact seven-day quiet boundary', () => {
    const source = createFixtureSource(),
      now = Date.now(),
      p = source.getSnapshot().projects[3];
    source.set({
      projects: [
        { ...p, last_work_at: now - sevenDays + 1 },
        { ...p, id: 'boundary', last_work_at: now - sevenDays },
        { ...p, id: 'older', last_work_at: now - sevenDays - 1 },
      ],
      tasks: [],
      questions: [],
    });
    expect(
      railGroups(source.getSnapshot(), now).map((g) => [g.label, g.projects.map((p) => p.id)]),
    ).toEqual([
      ['Recent', [p.id]],
      ['Quiet', ['boundary', 'older']],
    ]);
  });
});
describe('relative', () => {
  it.each([
    [1, '1 min ago'],
    [2, '2 min ago'],
    [60, '1 hour ago'],
    [120, '2 hours ago'],
    [1440, '1 day ago'],
    [2880, '2 days ago'],
  ])('formats relative time for %i minutes as %s', (minutes, expected) => {
    const at = 1_000_000;
    expect(relative(at, at + minutes * 60000)).toBe(expected);
  });
});
describe('status', () => {
  it('shares paused, working, attention and quiet status priority', async () => {
    const { projectStatus } = await import('@/lib/data/work');
    const s = createFixtureSource().getSnapshot(),
      p = s.projects[0],
      now = 1_800_000_000_000;
    expect(projectStatus(s, { ...p, paused: true }, now)).toEqual({
      label: 'Paused',
      tone: 'neutral',
    });
    expect(projectStatus(s, p, now)).toEqual({ label: 'Working', tone: 'working' });
    expect(projectStatus({ ...s, tasks: [] }, p, now)).toEqual({
      label: 'Needs you',
      tone: 'warn',
    });
    expect(projectStatus({ ...s, tasks: [], questions: [] }, p, now)).toEqual({
      label: 'Quiet',
      tone: 'neutral',
    });
  });
  it('shows a plan on hold, which is not under Needs you', async () => {
    const { projectStatus, needsYou, railStatus } = await import('@/lib/data/work');
    const s = createFixtureSource().getSnapshot(),
      p = s.projects[0],
      now = 1_800_000_000_000,
      // "Not yet" closed the plan question and parked the goal.
      parked = {
        ...s,
        tasks: [],
        goals: s.goals.map((g) =>
          g.project === p.id ? { ...g, state: 'stated' as const, plan_parked: true } : g,
        ),
        questions: s.questions.map((q) => (q.project === p.id ? { ...q, open: false } : q)),
      };
    expect(needsYou(parked).questions.filter((q) => q.project === p.id)).toEqual([]);
    expect(projectStatus(parked, p, now)).toEqual({ label: 'Plan on hold', tone: 'neutral' });
    expect(railStatus(parked, p, now)).toBe('Plan on hold');
  });
});

it('counts attention once across projects and refreshes with the next store snapshot', () => {
  const source = createFixtureSource();
  const { projects, questions } = source.getSnapshot();
  source.set({
    tasks: [],
    questions: [
      { ...questions[0], project: projects[0].id, open: true },
      { ...questions[0], id: 'other', project: projects[1].id, open: true },
      { ...questions[0], id: 'closed', project: projects[0].id, open: false },
    ],
  });
  const snapshot = source.getSnapshot();
  const scan = vi.spyOn(snapshot.questions, 'filter');
  expect(needs(snapshot, projects[0])).toBe(1);
  expect(needs(snapshot, projects[1])).toBe(1);
  expect(needs(snapshot, projects[2])).toBe(0);
  expect(scan).toHaveBeenCalledTimes(1);
  source.set({ questions: [] });
  expect(needs(source.getSnapshot(), projects[0])).toBe(0);
  expect(needs(snapshot, projects[0])).toBe(1);
});

describe('why a project needs you', () => {
  const source = createFixtureSource(),
    s = source.getSnapshot(),
    p = s.projects[0],
    question = { ...s.questions[0], id: 'q', project: p.id, open: true },
    fault = (id: string, state: 'stopped' | 'failed') => ({
      id,
      project: p.id,
      title: id,
      state: { state } as never,
    }),
    rail = (questions: (typeof question)[], tasks: ReturnType<typeof fault>[]) =>
      railStatus(
        {
          ...s,
          tasks: [],
          goals: [],
          questions,
          status: { ...s.status, tasks },
        },
        p,
        Date.now(),
      );
  it.each([
    ['an open question', [question], [], 'Waiting for your answer'],
    ['a stopped task', [], [fault('a', 'stopped')], 'Task stopped'],
    ['stopped tasks', [], [fault('a', 'stopped'), fault('b', 'stopped')], 'Tasks stopped'],
    ['a failed task', [], [fault('a', 'failed')], 'Task failed'],
    ['failed tasks', [], [fault('a', 'failed'), fault('b', 'failed')], 'Tasks failed'],
    [
      'a stopped task that asks',
      [question],
      [{ ...fault('a', 'stopped'), question: 'q' }],
      'Waiting for your answer',
    ],
    ['a question and a stopped task', [question], [fault('a', 'stopped')], 'Needs you'],
    ['a stopped and a failed task', [], [fault('a', 'stopped'), fault('b', 'failed')], 'Needs you'],
  ])('reads %s', (_, questions, tasks, label) => {
    expect(rail(questions, tasks)).toBe(label);
  });
});
