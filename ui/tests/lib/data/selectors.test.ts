import type { EventKind, Event } from '@/lib/data/types';
import { expect, it, describe } from 'vitest';
import { createFixtureSource } from '../../support/render';
import { taskRunIds, parseChecks } from '@/lib/data/selectors';
describe('runs', () => {
  const task = createFixtureSource().getSnapshot().tasks[0];
  const event = (id: number, kind: EventKind): Event => ({
    id,
    at: id,
    actor: 'engine',
    project: task.project,
    ...kind,
  });
  it('collects and deduplicates task run ids and parses check names', () => {
    const rows = [
      event(1, { kind: 'task.started', payload: { task: task.id, run: 'r' } }),
      event(2, { kind: 'task.started', payload: { task: task.id, run: 'r' } }),
    ];
    expect(taskRunIds(rows, task.id)).toEqual(['r']);
    expect(taskRunIds(rows, 'other')).toEqual([]);
    expect(parseChecks(' tests, , build , ')).toEqual(['tests', 'build']);
  });
});
