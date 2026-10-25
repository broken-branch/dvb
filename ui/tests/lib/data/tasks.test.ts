import { createSource } from '@/lib/data/source';
import { json, flush, fakeApi } from '../../support/api';
import { describe, it, expect, vi } from 'vitest';
import { createFixtureSource } from '../../support/render';
import { createLiveSource } from '@/lib/data/api';
describe('cancellation', () => {
  describe('failed tasks of dropped goals', () => {
    it.each([true, false])(
      'preserves authoritative task hydration when goals arrive first: %s',
      async (goalsFirst) => {
        const seed = createFixtureSource().getSnapshot().tasks[0];
        let controller!: ReadableStreamDefaultController<Uint8Array>;
        const backlog = new ReadableStream<Uint8Array>({
          start(c) {
            controller = c;
            c.enqueue(new TextEncoder().encode(': live\n\n'));
          },
        });
        const base = fakeApi([new Response(backlog, { headers: { 'dvb-version': '0.1.0' } })]);
        const fetcher = vi.fn<typeof fetch>(async (input, init) => {
          const url = String(input);
          if (url === '/tasks' || url === '/goals') {
            if ((url === '/tasks') === goalsFirst)
              for (let i = 0; i < 5; i++) await Promise.resolve();
            return json(
              url === '/goals'
                ? [{ id: 'dropped', project: 'p', text: 'Goal', state: 'dropped' }]
                : [
                    {
                      ...seed,
                      id: 'failed',
                      project: 'p',
                      goal: 'dropped',
                      head: 'head',
                      state: { state: 'cancelled' },
                      reason: 'its goal was dropped',
                    },
                  ],
            );
          }
          return base(input, init);
        });
        const source = createLiveSource({ base: '', version: '0.1.0', fetch: fetcher });
        source.start();
        await flush();
        expect(source.getSnapshot().tasks[0]).toMatchObject({
          state: { state: 'cancelled' },
          reason: 'its goal was dropped',
          head: 'head',
        });
        source.dispose();
        controller.close();
      },
    );
  });
});

it('keeps task projections authoritative when only the goal changes', () => {
  const source = createSource();
  const task = {
    ...createFixtureSource().getSnapshot().tasks[0],
    goal: 'g',
    state: { state: 'failed' as const, reason: 'Engine failure' },
  };
  source.set({ tasks: [task] });
  source.set({ goals: [{ id: 'g', project: task.project, text: 'Goal', state: 'dropped' }] });
  expect(source.getSnapshot().tasks[0]).toBe(task);
  expect(source.getSnapshot().tasks[0].state).toEqual({
    state: 'failed',
    reason: 'Engine failure',
  });
});
