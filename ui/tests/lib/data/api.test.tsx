import { project, goal, task, event, page } from '../../support/rows';
import { json, stream, flush, fakeApi } from '../../support/api';
import type { Task, ChartMove, Event } from '@/lib/data/types';
import { describe, it, expect, vi } from 'vitest';
import { createLiveSource } from '@/lib/data/api';
import { createFixtureSource } from '../../support/render';
import { waitFor } from '@testing-library/react';
describe('readiness', () => {
  describe('live backlog readiness', () => {
    it.each(['end', 'error', 'invalid event'] as const)(
      'keeps writes disabled after stream %s until the replacement feed catches up',
      async (ending) => {
        vi.useFakeTimers();
        let first!: ReadableStreamDefaultController<Uint8Array>,
          next!: ReadableStreamDefaultController<Uint8Array>;
        const firstBody = new ReadableStream<Uint8Array>({
          start(c) {
            first = c;
          },
        });
        const nextBody = new ReadableStream<Uint8Array>({
          start(c) {
            next = c;
          },
        });
        const fetcher = fakeApi(
          [firstBody, nextBody].map(
            (body) => new Response(body, { headers: { 'dvb-version': '0.1.0' } }),
          ),
        );
        const source = createLiveSource({ base: '', fetch: fetcher, reconnectMs: 10 });
        source.start();
        await flush();
        first.enqueue(new TextEncoder().encode(': live\n\n'));
        await flush();
        expect(source.getSnapshot().connection.state).toBe('connected');
        expect((await source.getSnapshot().answer('q', { choice: 1 })).ok).toBe(true);
        if (ending === 'end') first.close();
        else if (ending === 'error') first.error(new Error('Feed lost'));
        else first.enqueue(new TextEncoder().encode('data: invalid\n\n'));
        await flush();
        expect(source.getSnapshot().connection.state).toBe('reconnecting');
        expect((await source.getSnapshot().usage()).ok).toBe(true);
        expect(source.getSnapshot().connection.state).toBe('reconnecting');
        const calls = fetcher.mock.calls.length;
        await expect(source.getSnapshot().answer('q', { choice: 1 })).rejects.toThrow(
          'Waiting to reconnect',
        );
        expect(fetcher).toHaveBeenCalledTimes(calls);
        expect(
          fetcher.mock.calls.filter(([url]) => String(url).startsWith('/events?')),
        ).toHaveLength(1);
        await vi.advanceTimersByTimeAsync(10);
        await flush();
        expect(
          fetcher.mock.calls.filter(([url]) => String(url).startsWith('/events?')),
        ).toHaveLength(2);
        expect(source.getSnapshot().connection.state).toBe('reconnecting');
        await expect(source.getSnapshot().answer('q', { choice: 1 })).rejects.toThrow(
          'Waiting to reconnect',
        );
        next.enqueue(new TextEncoder().encode(': live\n\n'));
        await flush();
        expect(source.getSnapshot().connection.state).toBe('connected');
        expect((await source.getSnapshot().answer('q', { choice: 1 })).ok).toBe(true);
        source.dispose();
        next.close();
        await flush();
      },
    );
    it('clears readiness on disposal and ignores a disposed stream across restart', async () => {
      let old!: ReadableStreamDefaultController<Uint8Array>,
        current!: ReadableStreamDefaultController<Uint8Array>;
      const oldBody = new ReadableStream<Uint8Array>({
        start(c) {
          old = c;
        },
      });
      const currentBody = new ReadableStream<Uint8Array>({
        start(c) {
          current = c;
        },
      });
      const fetcher = fakeApi(
        [oldBody, currentBody].map(
          (body) => new Response(body, { headers: { 'dvb-version': '0.1.0' } }),
        ),
      );
      const source = createLiveSource({ base: '', fetch: fetcher });
      source.start();
      await flush();
      old.enqueue(new TextEncoder().encode(': live\n\n'));
      await flush();
      expect(source.getSnapshot().connection.state).toBe('connected');
      source.dispose();
      expect(source.getSnapshot().connection.state).toBe('reconnecting');
      await source.getSnapshot().usage();
      expect(source.getSnapshot().connection.state).toBe('reconnecting');
      const calls = fetcher.mock.calls.length;
      await expect(source.getSnapshot().answer('q', { choice: 1 })).rejects.toThrow(
        'Waiting to reconnect',
      );
      expect(fetcher).toHaveBeenCalledTimes(calls);
      source.start();
      await flush();
      old.enqueue(new TextEncoder().encode(': live\n\n'));
      await flush();
      expect(source.getSnapshot().connection.state).toBe('reconnecting');
      current.enqueue(new TextEncoder().encode(': live\n\n'));
      await flush();
      expect(source.getSnapshot().connection.state).toBe('connected');
      old.close();
      await flush();
      expect(source.getSnapshot().connection.state).toBe('connected');
      expect((await source.getSnapshot().answer('q', { choice: 1 })).ok).toBe(true);
      source.dispose();
      current.close();
      await flush();
    });
    it('enables writes only when the feed has caught up', async () => {
      const controller: { current?: ReadableStreamDefaultController<Uint8Array> } = {};
      const body = new ReadableStream<Uint8Array>({
        start(c) {
          controller.current = c;
        },
      });
      const fetcher = fakeApi([new Response(body, { headers: { 'dvb-version': '0.1.0' } })]);
      const source = createLiveSource({ base: '', fetch: fetcher });
      source.start();
      await flush();
      expect(source.getSnapshot().connection.state).toBe('reconnecting');
      await expect(source.getSnapshot().answer('q', { choice: 1 })).rejects.toThrow(
        'Waiting to reconnect',
      );
      controller.current!.enqueue(new TextEncoder().encode(': live\n\n'));
      await flush();
      expect(source.getSnapshot().connection.state).toBe('connected');
      source.dispose();
      controller.current!.close();
      await flush();
    });
  });
});
describe('requests', () => {
  describe('owner request bodies', () => {
    it.each([
      { kind: 'report', role: 'backend', parent: null },
      { kind: 'group', roles: ['backend'], group: null },
      { kind: 'reorder', role: 'backend', before: null },
    ] satisfies ChartMove[])(
      'preserves the explicit null destination for $kind moves',
      async (movement) => {
        const fetcher = vi.fn<typeof fetch>().mockResolvedValue(json([]));
        const source = createLiveSource({ base: '', fetch: fetcher });
        source.set({ connection: { state: 'connected', engineVersion: '0.1.0' } });
        await source.getSnapshot().moveChart('p', movement);
        expect(fetcher.mock.calls[0]).toMatchObject([
          '/projects/p/chart',
          { method: 'POST', body: JSON.stringify(movement) },
        ]);
      },
    );
    it('sends atomic chart moves and both login calls with the matching Rust bodies', async () => {
      const fetcher = vi.fn<typeof fetch>().mockImplementation(
        async () =>
          new Response(JSON.stringify({ run: 'r', url: '', prompt: 'Logged in.' }), {
            headers: { 'Content-Type': 'application/json' },
          }),
      );
      const source = createLiveSource({ base: '', fetch: fetcher });
      source.set({ connection: { state: 'connected', engineVersion: '0.1.0' } });
      await source
        .getSnapshot()
        .moveChart('p', { kind: 'swap', role: 'backend', lead: 'orchestrator' });
      expect(fetcher.mock.calls[0]).toMatchObject([
        '/projects/p/chart',
        {
          method: 'POST',
          body: JSON.stringify({ kind: 'swap', role: 'backend', lead: 'orchestrator' }),
        },
      ]);
      await source.getSnapshot().startLogin('claude');
      expect(fetcher.mock.calls[1][1]?.body).toBeUndefined();
      await source.getSnapshot().finishLogin('claude', { run: 'r', code: 'code-from-page' });
      expect(fetcher.mock.calls[2]).toMatchObject([
        '/logins/claude',
        { body: JSON.stringify({ run: 'r', code: 'code-from-page' }) },
      ]);
      await source.getSnapshot().finishLogin('codex', { run: 'r', code: '' });
      expect(fetcher.mock.calls[3][1]?.body).toBe(JSON.stringify({ run: 'r', code: '' }));
    });
    it('hydrates goals and available models without depending on an untruncated feed', async () => {
      const fetcher = fakeApi([]);
      const original = fetcher.getMockImplementation()!;
      fetcher.mockImplementation(async (input, init) => {
        if (input === '/goals')
          return json([{ id: 'g', project: 'p', text: 'Stored goal', state: 'done' }]);
        if (input === '/models')
          return json({ models: [{ id: 'fake-1', vendor: 'fake', efforts: ['medium'] }] });
        return original(input, init);
      });
      const source = createLiveSource({ base: '', fetch: fetcher });
      source.start();
      await flush();
      expect(source.getSnapshot().goals[0]).toMatchObject({ text: 'Stored goal', state: 'done' });
      expect(source.getSnapshot().models.models[0]).toMatchObject({
        id: 'fake-1',
        efforts: ['medium'],
      });
      source.dispose();
    });
  });
});
describe('snapshot races', () => {
  it('launches from snapshots, ignores older stream mutations, and refreshes writes racing hydration', async () => {
    let send!: ReadableStreamDefaultController<Uint8Array>;
    const body = new ReadableStream<Uint8Array>({
      start(c) {
        send = c;
      },
    });
    let current = task('t', 'g', { state: 'done' });
    let delayed: ((r: Response) => void) | undefined;
    let hold = false;
    const fetcher = vi.fn<typeof fetch>(async (input) => {
      const url = String(input);
      if (url.startsWith('/events?'))
        return new Response(body, { headers: { 'dvb-version': '0.1.0' } });
      if (url.startsWith('/events/recent')) return json(page());
      if (url === '/version') return json({ version: '0.1.0' });
      if (url === '/projects') return json([project]);
      if (url === '/goals')
        return json([goal('g', current.state.state === 'done' ? 'done' : 'failed')]);
      if (url === '/tasks') {
        if (hold) {
          hold = false;
          return new Promise<Response>((resolve) => {
            delayed = resolve;
          });
        }
        return json([current]);
      }
      if (url === '/questions')
        return json([
          {
            id: 'q',
            project: 'p',
            subject: 'Still open',
            open: true,
            options: [],
            recommended: 1,
            format: 'simple',
          },
        ]);
      if (url === '/status') return json({ running: 7, needs_you: 2 });
      if (url === '/settings') return json({ values: {} });
      if (url === '/models') return json({ models: [] });
      return json([]);
    });
    const live = createLiveSource({ base: '', fetch: fetcher, version: '0.1.0' });
    live.start();
    await waitFor(() =>
      expect(fetcher.mock.calls.some(([url]) => url === '/events?after=100')).toBe(true),
    );
    const observed: string[] = [];
    const off = live.subscribe(() => observed.push(live.getSnapshot().tasks[0]?.state.state));
    const emit = (e: Event) =>
      send.enqueue(new TextEncoder().encode(`id: ${e.id}\ndata: ${JSON.stringify(e)}\n\n`)); // The engine snapshot already includes completion when this older change arrives.
    emit(
      event(101, 'task.state', { task: 't', from: { state: 'planned' }, to: { state: 'running' } }),
    );
    send.enqueue(new TextEncoder().encode(': live\n\n'));
    await waitFor(() => expect(live.getSnapshot().connection.state).toBe('connected'));
    expect(observed).not.toContain('running');
    expect(live.getSnapshot().status).toEqual({ running: 7, needs_you: 2 });
    hold = true;
    emit(event(102, 'goal.stated', { goal: 'next', text: 'Later goal' }));
    await waitFor(() => expect(delayed).toBeDefined());
    current = task('t', 'g', { state: 'failed', reason: 'failure' });
    emit(event(103, 'task.failed', { task: 't', reason: 'failure' }));
    delayed!(json([task('t', 'g', { state: 'done' })]));
    await waitFor(() => expect(live.getSnapshot().tasks[0].state.state).toBe('failed'));
    expect(live.getSnapshot().questions[0].open).toBe(true);
    for (const route of ['/projects', '/goals', '/tasks', '/questions', '/status'])
      expect(fetcher.mock.calls.some(([url]) => url === route)).toBe(true);
    expect(fetcher.mock.calls.some(([url]) => url === '/events?after=0')).toBe(false);
    off();
    live.dispose();
    send.close();
  });
});
describe('transport', () => {
  const started: Event = {
    id: 21,
    at: 1000,
    actor: 'engine',
    project: 'p',
    kind: 'engine.started',
    payload: { version: '0.1.0' },
  };
  describe('live transport without sockets', () => {
    it('reconnects with Last-Event-ID and ignores repeated events', async () => {
      vi.useFakeTimers();
      const frame = `id: 21\ndata: ${JSON.stringify(started)}\n\n`,
        fetcher = fakeApi([
          new Response(stream([frame]), { headers: { 'dvb-version': '0.1.0' } }),
          new Response(stream([frame]), { headers: { 'dvb-version': '0.1.0' } }),
        ]);
      const source = createLiveSource({ base: '', fetch: fetcher, reconnectMs: 10 });
      source.start();
      await flush();
      await vi.advanceTimersByTimeAsync(10);
      await flush();
      const requests = fetcher.mock.calls.filter(([url]) => String(url).startsWith('/events?'));
      expect(requests[0][1]?.headers).toMatchObject({ 'Last-Event-ID': '20' });
      expect(requests[1][0]).toBe('/events?after=21');
      expect(requests[1][1]?.headers).toMatchObject({ 'Last-Event-ID': '21' });
      expect(source.getSnapshot().events).toHaveLength(1);
      source.dispose();
      const count = fetcher.mock.calls.length;
      await vi.advanceTimersByTimeAsync(60000);
      expect(fetcher.mock.calls).toHaveLength(count);
    });
    it('takes the app version from the desk page, never from a literal', async () => {
      vi.useFakeTimers();
      document.head.querySelector('meta[name="dvb-version"]')!.setAttribute('content', '9.8.7');
      try {
        const fetcher = fakeApi(
          [new Response(stream([]), { headers: { 'dvb-version': '9.8.7' } })],
          '9.8.7',
        );
        const source = createLiveSource({ base: '', fetch: fetcher });
        source.start();
        await flush();
        expect(source.getSnapshot().connection.state).not.toBe('version-mismatch');
        source.dispose();
      } finally {
        document.head.querySelector('meta[name="dvb-version"]')!.setAttribute('content', '0.1.0');
      }
      const files = import.meta.glob('../src/lib/data/**/*.ts', {
        query: '?raw',
        import: 'default',
        eager: true,
      }) as Record<string, string>;
      for (const [file, text] of Object.entries(files))
        expect(text, file).not.toMatch(/APP_VERSION|['"`]\d+\.\d+\.\d+['"`]/);
    });
    it('keeps reads while refusing writes to a different version', async () => {
      vi.useFakeTimers();
      const fetcher = fakeApi(
        [new Response(stream([]), { headers: { 'dvb-version': '0.2.0' } })],
        '0.2.0',
      );
      const source = createLiveSource({ base: '', fetch: fetcher });
      source.start();
      await flush();
      expect(source.getSnapshot().projects).toHaveLength(1);
      expect(source.getSnapshot().connection.state).toBe('version-mismatch');
      const before = fetcher.mock.calls.length,
        result = await source.getSnapshot().answer('q', { choice: 1 });
      expect(result).toMatchObject({ ok: false, refusal: { code: 'VERSION_MISMATCH' } });
      expect(fetcher.mock.calls).toHaveLength(before);
      source.dispose();
    });
    it('uses the desk API prefix and reports failed projections', async () => {
      vi.useFakeTimers();
      const fetcher = vi
        .fn<typeof fetch>()
        .mockImplementation(async (input) =>
          String(input) === '/api/version'
            ? json({ version: '0.1.0' })
            : String(input).startsWith('/api/events/recent')
              ? json({ cursor: 20, events: [], before: null, activity: [] })
              : String(input).startsWith('/events?')
                ? new Response(stream([]), { headers: { 'dvb-version': '0.1.0' } })
                : json({ code: 'INTERNAL_ERROR', detail: 'Read failed' }, '0.1.0', 500),
        );
      const source = createLiveSource({ fetch: fetcher });
      source.start();
      await flush();
      expect(fetcher.mock.calls.some(([url]) => url === '/api/projects')).toBe(true);
      expect(source.getIssue()).toBe('Read failed');
      expect(source.getSnapshot().projects).toEqual([]);
      source.dispose();
    });
    it('does not start duplicate reconnect loops across a stop/start lifecycle', async () => {
      vi.useFakeTimers();
      const fetcher = fakeApi([]),
        source = createLiveSource({ base: '', fetch: fetcher, reconnectMs: 10 });
      source.start();
      source.dispose();
      source.start();
      await flush();
      expect(fetcher.mock.calls.filter(([url]) => String(url).startsWith('/events?'))).toHaveLength(
        1,
      );
      await vi.advanceTimersByTimeAsync(10);
      await flush();
      expect(fetcher.mock.calls.filter(([url]) => String(url).startsWith('/events?'))).toHaveLength(
        2,
      );
      source.dispose();
    });
    it('sends the typed answer and surfaces coded errors without sample substitution', async () => {
      const fetcher = vi
        .fn<typeof fetch>()
        .mockResolvedValue(json({ code: 'QUESTION_LAPSED', detail: 'Changed' }, '0.1.0', 409));
      const source = createLiveSource({ base: '', fetch: fetcher });
      source.set({ connection: { state: 'connected', engineVersion: '0.1.0' } });
      expect(await source.getSnapshot().answer('a/b', { reply: 'Keep going' })).toMatchObject({
        ok: false,
        refusal: { code: 'QUESTION_LAPSED' },
      });
      expect(fetcher.mock.calls[0]).toMatchObject([
        '/questions/a%2Fb/answer',
        {
          method: 'POST',
          credentials: 'same-origin',
          body: JSON.stringify({ reply: 'Keep going' }),
        },
      ]);
      fetcher.mockResolvedValue(
        json({ code: 'INTERNAL_ERROR', detail: 'Read failed' }, '0.1.0', 500),
      );
      expect(await source.getSnapshot().usage()).toMatchObject({
        ok: false,
        refusal: { code: 'INTERNAL_ERROR' },
      });
      expect(source.getSnapshot().tasks).toEqual([]);
    });
  });
});
describe('work facts', () => {
  it('hydrates and refreshes work facts from snapshots without deriving times from the stream', async () => {
    const { createLiveSource } = await import('@/lib/data/api');
    const { waitFor } = await import('@testing-library/react');
    let stream!: ReadableStreamDefaultController<Uint8Array>;
    const body = new ReadableStream<Uint8Array>({
      start(controller) {
        stream = controller;
        controller.enqueue(new TextEncoder().encode(': live\n\n'));
      },
    });
    const seed = createFixtureSource().getSnapshot();
    let at = 50;
    const fetcher: typeof fetch = async (input) => {
      const url = new URL(String(input), 'http://localhost');
      const headers = { 'dvb-version': '0.1.0' };
      if (url.pathname === '/events') return new Response(body, { headers });
      const value =
        url.pathname === '/version'
          ? { version: '0.1.0' }
          : url.pathname === '/events/recent'
            ? { cursor: 100, events: [], before: null, activity: [] }
            : url.pathname === '/projects'
              ? [
                  {
                    ...seed.projects[0],
                    last_work_at: at,
                    done_goals: 4,
                    last_finished: { text: 'Finish the import.', at: 40, pr: 135 },
                  },
                ]
              : url.pathname === '/tasks'
                ? [
                    {
                      ...seed.tasks.find((t) => t.id === 'resume')!,
                      current_run: {
                        ...seed.tasks.find((t) => t.id === 'resume')!.current_run!,
                        started_at: 30,
                      },
                    },
                  ]
                : url.pathname === '/questions'
                  ? [{ ...seed.questions[0], opened_at: 20 }]
                  : url.pathname === '/goals'
                    ? seed.goals
                    : url.pathname === '/settings'
                      ? seed.settings
                      : url.pathname === '/status'
                        ? seed.status
                        : url.pathname === '/models'
                          ? seed.models
                          : [];
      return new Response(JSON.stringify(value), { headers });
    };
    const live = createLiveSource({ fetch: fetcher, base: '', eventsBase: '', version: '0.1.0' });
    live.start();
    try {
      await waitFor(() => expect(live.getSnapshot().connection.state).toBe('connected'));
      expect(live.getSnapshot().projects[0].last_work_at).toBe(50);
      expect(live.getSnapshot().projects[0].done_goals).toBe(4);
      expect(live.getSnapshot().projects[0].last_finished?.pr).toBe(135);
      expect(live.getSnapshot().questions[0].opened_at).toBe(20);
      expect(live.getSnapshot().tasks[0].current_run?.started_at).toBe(30);
      at = 60;
      stream.enqueue(
        new TextEncoder().encode(
          `id: 101\ndata: ${JSON.stringify({ id: 101, at: 999999, project: 'harbour', actor: 'engine', kind: 'checks.observed', payload: { task: 'resume', head: 'h', status: 'passed', names: [] } })}\n\n`,
        ),
      );
      await waitFor(() => expect(live.getSnapshot().projects[0].last_work_at).toBe(60));
      expect(live.getSnapshot().tasks[0].current_run?.started_at).toBe(30);
    } finally {
      live.dispose();
      stream.close();
    }
  });
});

describe('task reason delivery', () => {
  it('refreshes connected task snapshots when resource reasons change or clear', async () => {
    let stream!: ReadableStreamDefaultController<Uint8Array>;
    const body = new ReadableStream<Uint8Array>({
      start(controller) {
        stream = controller;
        controller.enqueue(new TextEncoder().encode(': live\n\n'));
      },
    });
    const seed = createFixtureSource().getSnapshot();
    let reason: Task['reason'] = 'Waiting for memory';
    let taskReads = 0;
    const fetcher: typeof fetch = async (input) => {
      const url = new URL(String(input), 'http://localhost');
      const headers = { 'dvb-version': '0.1.0' };
      if (url.pathname === '/events') return new Response(body, { headers });
      if (url.pathname === '/tasks') taskReads++;
      const value =
        url.pathname === '/version'
          ? { version: '0.1.0' }
          : url.pathname === '/events/recent'
            ? { cursor: 100, events: [], before: null, activity: [] }
            : url.pathname === '/tasks'
              ? [{ ...seed.tasks[0], state: { state: 'preparing' }, reason }]
              : url.pathname === '/settings'
                ? seed.settings
                : url.pathname === '/status'
                  ? seed.status
                  : url.pathname === '/models'
                    ? seed.models
                    : [];
      return new Response(JSON.stringify(value), { headers });
    };
    const live = createLiveSource({ fetch: fetcher, base: '', version: '0.1.0' });
    const send = (id: number, kind: string, payload: unknown) =>
      stream.enqueue(
        new TextEncoder().encode(
          `id: ${id}\ndata: ${JSON.stringify({ id, at: id, project: 'p', actor: 'engine', kind, payload })}\n\n`,
        ),
      );
    live.start();
    try {
      await waitFor(() => expect(live.getSnapshot().connection.state).toBe('connected'));
      expect(live.getSnapshot().tasks[0].reason).toBe('Waiting for memory');
      // Ordinary output retains the existing refresh behavior.
      send(101, 'run.output', { run: 'r', item: { note: 'Still working' } });
      await waitFor(() => expect(live.getSnapshot().events.at(-1)?.id).toBe(101));
      expect(taskReads).toBe(1);
      reason = 'Waiting for disk';
      send(102, 'task.reason', { task: seed.tasks[0].id, run: 'r', reason });
      await waitFor(() => expect(live.getSnapshot().tasks[0].reason).toBe('Waiting for disk'));
      // Vendor admission still holds; no launch/state event supplies a refresh.
      reason = null;
      send(103, 'task.reason', { task: seed.tasks[0].id, run: 'r', reason });
      await waitFor(() => expect(live.getSnapshot().tasks[0].reason).toBeNull());
      expect(live.getSnapshot().tasks[0].state).toEqual({ state: 'preparing' });
      expect(taskReads).toBe(3);
      expect(live.getSnapshot().connection.state).toBe('connected');
    } finally {
      live.dispose();
      stream.close();
    }
  });
});

describe('owner refusal responses', () => {
  it.each([
    [404, 'NOT_FOUND'],
    [500, 'INTERNAL_ERROR'],
    [502, 'UPDATE_REFUSED'],
  ])('surfaces Refusal JSON at HTTP %s', async (status, code) => {
    const { createClient } = await import('@/lib/data/api/client');
    const fetcher = vi
      .fn<typeof fetch>()
      .mockResolvedValue(json({ code, detail: 'Reason' }, '0.1.0', status));
    const client = createClient(
      fetcher,
      '',
      '0.1.0',
      () => ({ state: 'connected', engineVersion: '0.1.0' }),
      vi.fn(),
    );
    expect(await client.call('/update', 'get')).toEqual({
      ok: false,
      refusal: { code, detail: 'Reason' },
    });
  });
});
