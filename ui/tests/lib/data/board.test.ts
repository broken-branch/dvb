import type { EventKind, Event, TaskState } from '@/lib/data/types';
import { describe, expect, it } from 'vitest';
import { boardCard, completionOrder, createBoardCards } from '@/lib/data/board';
import { createFixtureSource } from '../../support/render';
describe('cards', () => {
  const task = createFixtureSource().getSnapshot().tasks[0];
  const event = (id: number, kind: EventKind): Event => ({
    id,
    at: id,
    actor: 'engine',
    project: task.project,
    ...kind,
  });
  const check = (head = task.head): Event =>
    event(1, {
      kind: 'checks.observed',
      payload: { task: task.id, head: head || '', status: 'failed', names: ['tests'] },
    });
  describe('board card', () => {
    const states: [TaskState, string][] = [
      [{ state: 'planned' }, 'planned'],
      [{ state: 'preparing' }, 'working'],
      [{ state: 'running' }, 'working'],
      [{ state: 'collecting' }, 'working'],
      [{ state: 'publishing' }, 'working'],
      [{ state: 'in_review' }, 'review'],
      [{ state: 'awaiting_checks' }, 'checks'],
      [{ state: 'merging' }, 'ready'],
      [{ state: 'retry_pending' }, 'blocked'],
      [{ state: 'unknown' }, 'blocked'],
      [{ state: 'failed', reason: 'Run failed' }, 'blocked'],
      [{ state: 'stopped', was: { state: 'running' } }, 'stopped'],
      [{ state: 'done' }, 'done'],
      [{ state: 'cancelled' }, 'done'],
    ];
    it.each(states)('classifies %j as %s', (state, column) => {
      expect(boardCard({ ...task, state, role: 'missing-role' }, [])).toMatchObject({
        status: column,
        role: 'missing-role',
        ci: 'Not reported',
      });
    });
    it('blocks failing current-head CI but preserves terminal columns and reasons', () => {
      const t = { ...task, head: 'current', state: { state: 'in_review' as const } };
      expect(boardCard(t, [check('old')]).status).toBe('review');
      expect(boardCard(t, [check('current')])).toMatchObject({
        status: 'blocked',
        reason: 'Failing CI',
      });
      expect(boardCard({ ...t, state: { state: 'done' } }, [check('current')]).status).toBe('done');
      expect(
        boardCard({ ...t, state: { state: 'cancelled' }, reason: 'its goal was dropped' }, [
          check('current'),
        ]),
      ).toMatchObject({ status: 'done', reason: 'Cancelled: its goal was dropped' });
      expect(boardCard({ ...t, state: { state: 'cancelled' } }, []).reason).toBe('Cancelled');
      expect(boardCard({ ...t, state: { state: 'failed', reason: 'Run failed' } }, []).reason).toBe(
        'Run failed',
      );
    });
    it('requests changes only for open current-head findings in review', () => {
      const verdict = event(2, {
        kind: 'verdict.recorded',
        payload: {
          task: task.id,
          head: 'current',
          summary: 'Fix it',
          findings: [
            { severity: 'high', file: 'a', line: null, title: 'Bug', detail: 'Fix', open: true },
          ],
        },
      });
      const t = { ...task, head: 'current', state: { state: 'in_review' as const } };
      expect(boardCard(t, [verdict])).toMatchObject({ status: 'changes', reason: 'Bug' });
      expect(boardCard({ ...t, head: 'next' }, [verdict]).status).toBe('review');
      expect(boardCard({ ...t, state: { state: 'done' } }, [verdict]).status).toBe('done');
    });
    it('precomputes terminal ordering including historical legacy state events', () => {
      const rows = [
        event(20, { kind: 'task.done', payload: { task: 'a' } }),
        event(10, { kind: 'task.cancelled', payload: { task: 'b', reason: 'Dropped' } }),
        event(30, {
          kind: 'task.state',
          payload: { task: 'b', from: { state: 'running' }, to: { state: 'cancelled' } },
        }),
      ];
      expect([...completionOrder(rows)]).toEqual([
        ['a', 20],
        ['b', 30],
      ]);
    });
  });
  it('retains card identities for unchanged facts across refreshed snapshots', () => {
    const derive = createBoardCards();
    const first = derive([task], []);
    expect(derive([{ ...task }], [check('old')])[0]).toBe(first[0]);
    expect(derive([{ ...task, title: 'Changed' }], [])[0]).not.toBe(first[0]);
  });
});

it.each([
  'Waiting for memory',
  'Waiting for disk',
  'Waiting for host load',
  'Waiting for a run slot',
])('retains %s card identity until the snapshot reason changes', (reason) => {
  const task = createFixtureSource().getSnapshot().tasks[0],
    derive = createBoardCards(),
    held = { ...task, state: { state: 'preparing' as const }, reason };
  const first = derive([held], [])[0];
  expect(first.reason).toBe(reason);
  expect(derive([{ ...held }], [])[0]).toBe(first);
  const released = derive([{ ...held, reason: undefined }], [])[0];
  expect(released).not.toBe(first);
  expect(released.reason).toBeUndefined();
});
