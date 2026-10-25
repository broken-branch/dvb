import { project, goal, task, event, page } from '../support/rows';
import type { components } from '@/api/schema';
import { json } from '../support/api';
import type { Task, Event, Settings } from '@/lib/data/types';
import { mount } from '../support/render';
import { it, expect, describe, vi, afterEach } from 'vitest';
import { screen, act, within, render, waitFor } from '@testing-library/react';
import { TaskPage } from '@/screens/task';
import { TaskBoard } from '@/components/dvb/task-board';
import { createFixtureSource } from '../support/render';
import { Home } from '@/screens/home';
import { ProjectPage } from '@/screens/project';
import { createLiveSource } from '@/lib/data/api';
import { StoreProvider } from '@/lib/store';
import { AppearanceProvider } from '@/lib/appearance';
import { TooltipProvider } from '@/components/ui/tooltip';
// These tests supply the filtered rows the engine returns; no task/run association logic.
function taskSource() {
  const source = createFixtureSource();
  const initial = source.getSnapshot().events;
  const read = source.getSnapshot().eventPage;
  source.set({
    eventPage: (filter) => {
      if (!filter?.task || source.getSnapshot().events === initial) return read(filter);
      const events = source.getSnapshot().events;
      return Promise.resolve({
        ok: true,
        value: { cursor: events.at(-1)?.id ?? 0, events, before: null, activity: [] },
      });
    },
  });
  return source;
}
describe('actions', () => {
  it('Task stops and resumes saved work, loads diff and verdict, and persists panel choices', async () => {
    const source = taskSource();
    source.set({
      settings: { values: { ...source.getSnapshot().settings.values, motion: 'Off' } },
    });
    const { user, rerender, container } = mount(<TaskPage id="resume" />, source);
    const actions = within(container.querySelector('.page-heading .actions')! as HTMLElement);
    const task = source.getSnapshot().tasks.find((t) => t.id === 'resume')!;
    const stop = vi.spyOn(source.getSnapshot(), 'stopTask');
    const resume = vi.spyOn(source.getSnapshot(), 'resumeTask');
    const save = vi.spyOn(source.getSnapshot(), 'changeSettings');
    expect(await screen.findByText('+ saved.keepRemoved(records);')).toBeInTheDocument();
    await user.click(actions.getByRole('button', { name: 'Stop task' }));
    expect(stop).toHaveBeenCalledWith('resume');
    act(() =>
      source.set({
        tasks: [{ ...task, state: { state: 'stopped', was: { state: 'in_review' } } }],
      }),
    );
    await user.click(actions.getByRole('button', { name: 'Resume task' }));
    expect(resume).toHaveBeenCalledWith('resume');
    act(() => source.set({ tasks: [task] }));
    await user.click(
      within(container.querySelector('.page-tabs')! as HTMLElement).getByRole('tab', {
        name: 'Review',
      }),
    );
    expect(location.hash).toBe('#task/resume/review');
    rerender(<TaskPage id="resume" tab="review" />);
    expect(
      await screen.findByRole('heading', { level: 3, name: 'Keep removed records removed' }),
    ).toBeInTheDocument();
    await user.click(actions.getByLabelText('Choose task panels'));
    await user.click(screen.getByLabelText('Review', { selector: '[role="switch"]' }));
    expect(save).toHaveBeenCalledWith({ values: { task_panels: { review: false, checks: true } } });
  });
});
describe('send back with a note', () => {
  const stoppedState = { state: 'stopped' as const, was: { state: 'in_review' as const } };
  function withTask(changes: Partial<Task>) {
    const source = taskSource(),
      task = source.getSnapshot().tasks.find((t) => t.id === 'resume')!;
    source.set({ tasks: [{ ...task, ...changes }] });
    return { source, task: { ...task, ...changes } };
  }
  const sendBack = () => screen.queryByRole('button', { name: 'Send back with a note' });
  it.each([
    ['a stopped task with a PR', { state: stoppedState }],
    ['an in-review task with a PR', {}],
  ])('is offered on %s', (_, changes) => {
    const { source } = withTask(changes);
    mount(<TaskPage id="resume" />, source);
    expect(sendBack()).toBeInTheDocument();
  });
  it.each([
    ['a stopped task without a PR', { state: stoppedState, pr: null }],
    ['an in-review task without a PR', { pr: null, head: null }],
    ['a done task', { state: { state: 'done' as const } }],
  ])('is not offered on %s', (_, changes) => {
    const { source } = withTask(changes);
    mount(<TaskPage id="resume" />, source);
    expect(sendBack()).not.toBeInTheDocument();
  });
  it('sends the note and shows the task back with its builder', async () => {
    const { source, task } = withTask({ state: stoppedState });
    const retry = vi
      .spyOn(source.getSnapshot(), 'retryTask')
      .mockResolvedValue({ ok: true, value: { ...task, state: { state: 'preparing' } } });
    const { user } = mount(<TaskPage id="resume" />, source);
    await user.click(sendBack()!);
    const field = screen.getByLabelText('Note for the builder');
    expect(field).toHaveFocus();
    expect(screen.getByRole('button', { name: 'Send back' })).toBeDisabled();
    await user.type(field, '  Remove the PDF; generate it in the test.  ');
    await user.click(screen.getByRole('button', { name: 'Send back' }));
    expect(retry).toHaveBeenCalledWith('resume', {
      text: 'Remove the PDF; generate it in the test.',
    });
    expect(screen.queryByLabelText('Note for the builder')).not.toBeInTheDocument();
    expect(screen.getByRole('status')).toHaveTextContent(
      'Sent back to its builder with your note.',
    );
    expect(screen.getByRole('status')).toHaveFocus();
    expect(sendBack()).not.toBeInTheDocument();
    act(() => source.set({ tasks: [{ ...task, state: { state: 'running' } }] }));
    expect(document.querySelector('[data-task-state]')).toHaveTextContent('Working');
    expect(screen.getByRole('status')).toHaveTextContent('Sent back to its builder');
    act(() => source.set({ tasks: [{ ...task, state: { state: 'in_review' } }] }));
    expect(screen.queryByRole('status')).not.toBeInTheDocument();
  });
  it('shows the engine refusal and keeps the note', async () => {
    const { source } = withTask({});
    vi.spyOn(source.getSnapshot(), 'retryTask').mockResolvedValue({
      ok: false,
      refusal: { code: 'ALREADY_STOPPED', detail: 'This task cannot be sent back now.' },
    });
    const { user } = mount(<TaskPage id="resume" />, source);
    await user.click(sendBack()!);
    await user.type(screen.getByLabelText('Note for the builder'), 'Change X');
    await user.click(screen.getByRole('button', { name: 'Send back' }));
    expect(await screen.findByRole('alert')).toHaveTextContent(
      'This task cannot be sent back now.',
    );
    expect(screen.getByLabelText('Note for the builder')).toHaveValue('Change X');
    await user.keyboard('{Escape}');
    expect(screen.queryByLabelText('Note for the builder')).not.toBeInTheDocument();
    expect(sendBack()).toHaveFocus();
  });
});
describe('cancelled dropped failures in task lists', () => {
  it.each(['hydrated', 'live'] as const)(
    'shows Cancelled and the dropped reason after %s drop',
    async (mode) => {
      const source = taskSource(),
        s = source.getSnapshot(),
        seed = s.tasks.find((t) => t.project === 'harbour')!;
      const task = {
        ...seed,
        id: 'dropped-failure',
        title: 'Dropped failure',
        goal: 'dropped',
        head: 'head',
        state: { state: 'failed' as const, reason: 'push failed' },
      };
      source.set({
        goals: [
          {
            id: 'dropped',
            project: 'harbour',
            text: 'Dropped goal',
            state: mode === 'hydrated' ? 'dropped' : 'failed',
          },
        ],
        tasks: [
          mode === 'hydrated'
            ? { ...task, state: { state: 'cancelled' }, reason: 'its goal was dropped' }
            : task,
          { ...task, id: 'kept', goal: 'kept', title: 'Kept failure' },
          {
            ...task,
            id: 'stopped',
            title: 'Stopped control',
            state: { state: 'stopped', was: { state: 'running' } },
          },
        ],
        questions: [],
        events: [],
      });
      const { rerender } = mount(
        <>
          <Home />
          <TaskBoard project="harbour" />
        </>,
        source,
      );
      if (mode !== 'hydrated') {
        const drop: Event = {
          id: 100,
          at: 100,
          actor: 'owner',
          project: 'harbour',
          kind: 'goal.dropped',
          payload: { goal: 'dropped' },
        };
        await act(async () =>
          source.set({
            goals: [{ ...source.getSnapshot().goals[0], state: 'dropped' }],
            tasks: source
              .getSnapshot()
              .tasks.map((t) =>
                t.id === task.id
                  ? { ...t, state: { state: 'cancelled' }, reason: 'its goal was dropped' }
                  : t,
              ),
            events: [drop],
          }),
        );
      }
      expect(document.querySelector('.attention-section')).not.toBeInTheDocument();
      expect(screen.queryByRole('button', { name: /Show all/ })).not.toBeInTheDocument();
      expect(screen.getByText('Cancelled: its goal was dropped')).toBeInTheDocument(); // Old failing CI is still evidence, but cannot turn a cancelled card into Blocked.
      await act(async () =>
        source.set({
          events: [
            ...source.getSnapshot().events,
            {
              id: 102,
              at: 102,
              actor: 'engine',
              project: 'harbour',
              kind: 'checks.observed',
              payload: { task: task.id, head: 'head', status: 'failed', names: ['rust'] },
            },
          ],
        }),
      );
      expect(screen.getByText('Cancelled: its goal was dropped')).toBeInTheDocument();
      rerender(<TaskPage id={task.id} />);
      expect(document.querySelector('[data-task-state]')).toHaveTextContent('Cancelled');
      expect(screen.getByText('its goal was dropped')).toBeInTheDocument();
      expect(
        screen.queryByRole('button', { name: /Retry task|Resume task|Stop task/ }),
      ).not.toBeInTheDocument();
      rerender(<TaskPage id="kept" />);
      expect(screen.getByRole('button', { name: 'Retry task' })).toBeInTheDocument();
      rerender(<TaskPage id="stopped" />);
      expect(screen.getByRole('button', { name: 'Resume task' })).toBeInTheDocument();
    },
  );
});
describe('evidence', () => {
  function decision(id: number, run: string, title: string, project = 'harbour'): Event {
    return {
      id,
      at: 1,
      actor: { run },
      project,
      kind: 'decision.recorded',
      payload: {
        by: 'harbour-backend',
        title,
        options: ['SQLite', 'Files'],
        choice: 'SQLite',
        reason: 'Atomic writes',
        swot: 'Strength: transactions\nWeakness: one writer',
      },
    };
  }
  function started(id: number, run: string, task: string): Event {
    return {
      id,
      at: 1,
      actor: 'engine',
      project: 'harbour',
      kind: 'run.started',
      payload: {
        run,
        task,
        role: 'harbour-backend',
        hire: 'h',
        model: 'gpt-6.1-sol',
        vendor: 'codex',
        effort: 'high',
        reason: 'Build',
      },
    };
  }
  describe('task evidence and recorded decisions', () => {
    it('shows API diff, verdict, current-head CI and the recorded PR URL in one click', async () => {
      const source = taskSource(),
        task = source.getSnapshot().tasks.find((t) => t.id === 'resume')!;
      const diff = vi.fn(async () => ({
        ok: true as const,
        value: {
          task: task.id,
          base: 'base',
          head: task.head!,
          diff: '--- a/live.txt\n+++ b/live.txt\n+Actual change',
        },
      }));
      const verdict = vi.fn(async () => ({
        ok: true as const,
        value: {
          task: task.id,
          verdict: 'v',
          head: task.head!,
          run: 'r',
          role: 'harbour-code-reviewer',
          model: 'gpt-6.1-sol',
          vendor: 'codex' as const,
          open_findings: 0,
          summary: 'Actual review',
          findings: [],
        },
      }));
      const pr: Event = {
        id: 400,
        at: 1,
        actor: 'engine',
        project: 'harbour',
        kind: 'pr.opened',
        payload: {
          task: task.id,
          pr: task.pr!,
          url: 'https://example.invalid/example-org/harbour/pull/123',
        },
      };
      const check = (
        id: number,
        head: string,
        status: 'failure' | 'success',
        names: string[],
        taskId = task.id,
      ): Event => ({
        id,
        at: 1,
        actor: 'engine',
        project: 'harbour',
        kind: 'checks.observed',
        payload: { task: taskId, head, status, names },
      });
      source.set({
        diff,
        verdict,
        events: [
          pr,
          check(401, task.head!, 'success', ['Current test']),
          check(402, 'old', 'failure', ['Old test']),
          check(403, task.head!, 'failure', ['Other task test'], 'other'),
        ],
      });
      source.set({
        eventPage: async () => ({
          ok: true,
          value: {
            cursor: 403,
            before: null,
            activity: [],
            events: source.getSnapshot().events.filter((e) => [400, 401, 402].includes(e.id)),
          },
        }),
      });
      const { user } = mount(<TaskPage id={task.id} />, source);
      expect(await screen.findByText('+Actual change')).toBeInTheDocument();
      expect(diff).toHaveBeenCalledWith(task.id);
      expect(screen.getByRole('link', { name: /PR .* on GitHub/ })).toHaveAttribute(
        'href',
        pr.payload.url,
      );
      await user.click(screen.getByRole('tab', { name: 'Review' }));
      expect(await screen.findByText('Actual review')).toBeInTheDocument();
      expect(verdict).toHaveBeenCalledWith(task.id);
      await user.click(screen.getByRole('tab', { name: 'Checks' }));
      expect(screen.getByText('Current test')).toBeInTheDocument();
      expect(screen.queryByText('Old test')).not.toBeInTheDocument();
      expect(screen.queryByText('Other task test')).not.toBeInTheDocument();
      await act(async () =>
        source.set({
          tasks: source
            .getSnapshot()
            .tasks.map((t) => (t.id === task.id ? { ...t, head: 'new-head' } : t)),
        }),
      );
      expect(await screen.findByText('No checks reported on this change.')).toBeInTheDocument();
      expect(diff).toHaveBeenCalledTimes(2);
      expect(verdict).toHaveBeenCalledTimes(2);
    });
    it('lists decisions from all of the task’s runs, and project decisions on the project page', async () => {
      const source = taskSource();
      source.set({
        events: [
          started(1, 'first', 'resume'),
          started(2, 'second', 'resume'),
          started(3, 'other', 'checks'),
          decision(4, 'first', 'Earlier build choice'),
          decision(5, 'second', 'Latest build choice'),
          decision(6, 'other', 'Project choice'),
          decision(7, 'second', 'Foreign choice', 'fieldnotes'),
        ],
      });
      const projectRead = source.getSnapshot().eventPage;
      source.set({
        eventPage: async (filter) =>
          filter?.task
            ? {
                ok: true,
                value: {
                  cursor: 7,
                  before: null,
                  activity: [],
                  events: source.getSnapshot().events.filter((e) => [1, 2, 4, 5].includes(e.id)),
                },
              }
            : projectRead(filter),
      });
      const view = mount(<TaskPage id="resume" tab="decisions" />, source);
      await screen.findByText('Earlier build choice');
      const list = within(screen.getByLabelText('Recorded decisions'));
      expect(list.getByText('Earlier build choice')).toBeInTheDocument();
      expect(list.getByText('Latest build choice')).toBeInTheDocument();
      expect(list.getAllByText('Backend builder')).toHaveLength(2);
      expect(list.getAllByText('Atomic writes')).toHaveLength(2);
      expect(list.queryByText('Project choice')).not.toBeInTheDocument();
      expect(list.queryByText('Foreign choice')).not.toBeInTheDocument();
      view.unmount();
      mount(<ProjectPage id="harbour" tab="decisions" />, source);
      const projectList = within(screen.getByLabelText('Recorded decisions'));
      expect(projectList.getByText('Project choice')).toBeInTheDocument();
      expect(projectList.queryByText('Foreign choice')).not.toBeInTheDocument();
    });
    it('never shows engine words on the task page, in any state', async () => {
      const source = taskSource(),
        base = source.getSnapshot().tasks.find((t) => t.id === 'resume')!;
      const states = [
        { state: 'planned' },
        { state: 'running' },
        { state: 'in_review' },
        { state: 'merging' },
        { state: 'done', pr: 1, sha: 'abc' },
        { state: 'failed', reason: 'Build did not pass' },
        { state: 'stopped', was: { state: 'running' } },
        { state: 'unknown', last: { state: 'running' } },
      ];
      const e = (id: number, kind: string, payload: object): Event =>
        ({ id, at: 1, actor: 'engine', project: 'harbour', kind, payload }) as unknown as Event;
      const events = [
        started(1, 'r1', base.id),
        e(2, 'run.preflight', { run: 'r1', checks: [{ code: 'PF_UID', ok: true }] }),
        e(3, 'run.collected', { run: 'r1', head: 'h1' }),
        e(4, 'run.ended', { run: 'r1', reason: 'completed' }),
        e(5, 'verdict.recorded', {
          task: base.id,
          run: 'r1',
          head: 'h1',
          findings: [],
          summary: 'Fine',
          model: 'm',
          vendor: 'codex',
          role: 'harbour-code-reviewer',
          verdict: 'v',
        }),
        e(6, 'merge.refused', { task: base.id, reason: 'Checks are not finished' }),
        e(7, 'pr.merged', { task: base.id, pr: 1, sha: 'abc' }),
        e(8, 'effect.retry', { task: base.id, effect: 'merge', attempt: 1, at: 1 }),
      ];
      for (const state of states) {
        source.set({
          events,
          tasks: source
            .getSnapshot()
            .tasks.map((t) => (t.id === base.id ? ({ ...t, state } as typeof t) : t)),
        });
        const view = mount(<TaskPage id={base.id} tab="feed" />, source);
        await screen.findByLabelText('Activity feed');
        expect(document.body.textContent).not.toMatch(/\b(lease|grant|revision|heartbeat)s?\b/i);
        view.unmount();
      }
    });
  });
});
describe('historical evidence', () => {
  it('reads older task context separately from a recent feed and preserves visible evidence', async () => {
    const t = { ...task('t', 'g', { state: 'done' }), pr: 5, head: 'h' };
    const details = [
      event(1, 'plan.proposed', {
        goal: 'g',
        tasks: [
          {
            task: 't',
            title: t.title,
            role: 'frontend',
            review: 'design',
            summary: 'Stored task summary',
          },
        ],
      }),
      event(2, 'run.started', {
        task: 't',
        run: 'builder',
        role: 'frontend',
        hire: 'hire',
        model: 'fake-1',
        vendor: 'fake',
        effort: 'medium',
        reason: 'Stored builder reason',
      }),
      event(3, 'pr.opened', { task: 't', pr: 5, url: 'https://example.invalid/pull/5' }),
      event(4, 'checks.observed', {
        task: 't',
        head: 'h',
        status: 'success',
        names: ['Stored check'],
      }),
      {
        ...event(5, 'decision.recorded', {
          by: 'frontend',
          title: 'Stored task decision',
          choice: 'Simple',
          reason: 'Less code',
        }),
        actor: { run: 'builder' },
      } as Event,
    ];
    let send!: ReadableStreamDefaultController<Uint8Array>;
    const body = new ReadableStream<Uint8Array>({
      start(c) {
        send = c;
        c.enqueue(new TextEncoder().encode(': live\n\n'));
      },
    });
    let refreshed = false;
    const fetcher = vi.fn<typeof fetch>(async (input) => {
      const url = new URL(String(input), 'http://localhost');
      if (url.pathname === '/events')
        return new Response(body, { headers: { 'dvb-version': '0.1.0' } });
      if (url.pathname === '/events/recent') {
        if (!url.searchParams.get('task')) return json(page([]));
        if (refreshed)
          return json({
            ...page(
              url.searchParams.has('before')
                ? details
                : [event(101, 'run.collected', { run: 'builder', head: 'h' })],
              url.searchParams.has('before') ? null : 100,
            ),
            cursor: 101,
          });
        return json(
          page(
            url.searchParams.has('before') ? details.slice(0, 2) : details.slice(2),
            url.searchParams.has('before') ? null : 3,
          ),
        );
      }
      if (url.pathname === '/version') return json({ version: '0.1.0' });
      if (url.pathname === '/projects') return json([project]);
      if (url.pathname === '/tasks') return json([t]);
      if (url.pathname === '/goals') return json([goal('g', 'done')]);
      if (url.pathname === '/status') return json({ running: 0, needs_you: 0 });
      if (url.pathname === '/models') return json({ models: [] });
      if (url.pathname === '/settings') return json({ values: {} });
      if (url.pathname === '/runs/builder')
        return json({ model: 'fake-1', effort: 'medium', reason: 'Stored builder reason' });
      if (url.pathname === '/runs/builder/output')
        return json([{ id: 6, at: 1, item: { text: 'Stored output' } }]);
      if (url.pathname === '/tasks/t/diff') return json({ head: 'h', diff: '+Stored change' });
      if (url.pathname === '/tasks/t/verdict')
        return json({ head: 'h', findings: [], open_findings: 0, summary: 'Stored verdict' });
      return json([]);
    });
    const live = createLiveSource({ base: '', fetch: fetcher, version: '0.1.0' });
    const { user, unmount } = mount(<TaskPage id="t" />, live);
    expect(await screen.findByText('Stored task summary')).toBeInTheDocument();
    expect(screen.queryByText('Stored output')).toBeNull();
    expect(screen.getByRole('link', { name: 'PR #5 on GitHub' })).toHaveAttribute(
      'href',
      'https://example.invalid/pull/5',
    );
    await waitFor(() => expect(live.getSnapshot().connection.state).toBe('connected'));
    const taskReads = () =>
      fetcher.mock.calls.filter(
        ([input]) => new URL(String(input), 'http://localhost').searchParams.get('task') === 't',
      );
    const beforeRefresh = taskReads().length;
    refreshed = true;
    act(() => live.set({ tasks: [{ ...t, state: { state: 'running' } }] }));
    await waitFor(() => expect(taskReads()).toHaveLength(beforeRefresh + 1));
    await act(async () => {});
    expect(taskReads()).toHaveLength(beforeRefresh + 1);
    expect(screen.getByText('Stored task summary')).toBeInTheDocument();
    expect(await screen.findByText('Stored output')).toBeInTheDocument();
    await user.click(screen.getByRole('tab', { name: 'Checks' }));
    expect(screen.getByText('Stored check')).toBeInTheDocument();
    await user.click(screen.getByRole('tab', { name: 'Review' }));
    expect(await screen.findByText('Stored verdict')).toBeInTheDocument();
    await user.click(screen.getByRole('tab', { name: 'Team decisions' }));
    expect(screen.getByText('Stored task decision')).toBeInTheDocument();
    expect(fetcher.mock.calls.some(([url]) => String(url).includes('/events/recent?task=t'))).toBe(
      true,
    );
    unmount();
    send.close();
  });
});
describe('live updates', () => {
  it('shows Done on the live task screen from SSE without a reload', async () => {
    const { createLiveSource } = await import('@/lib/data/api');
    let send!: ReadableStreamDefaultController<Uint8Array>;
    const stream = new ReadableStream<Uint8Array>({
      start(controller) {
        send = controller;
      },
    });
    const task = {
      id: 'task',
      project: 'p',
      goal: 'g',
      title: 'Build the change',
      role: 'backend',
      review: 'code',
      state: { state: 'planned' as 'planned' | 'done' },
    };
    const fetcher = vi.fn<typeof fetch>().mockImplementation(async (input) => {
      const path = String(input);
      const headers = { 'dvb-version': '0.1.0' };
      if (path.startsWith('/api/events/recent'))
        return new Response(JSON.stringify({ cursor: 0, events: [], before: null, activity: [] }), {
          headers,
        });
      if (path.startsWith('/events?')) return new Response(stream, { headers });
      if (path === '/api/tasks') return new Response(JSON.stringify([task]), { headers });
      if (path === '/api/version')
        return new Response(JSON.stringify({ version: '0.1.0' }), { headers });
      if (path === '/api/settings')
        return new Response(JSON.stringify({ values: {} }), { headers });
      if (path === '/api/status')
        return new Response(JSON.stringify({ running: 0, needs_you: 0 }), { headers });
      if (path.includes('/task/'))
        return new Response(JSON.stringify({ code: 'NOT_IMPLEMENTED', detail: 'Not built yet' }), {
          status: 501,
          headers,
        });
      return new Response('[]', { headers });
    });
    const source = createLiveSource({ fetch: fetcher });
    const view = render(
      <StoreProvider source={source}>
        <AppearanceProvider>
          <TooltipProvider>
            <TaskPage id="task" />
          </TooltipProvider>
        </AppearanceProvider>
      </StoreProvider>,
    );
    await waitFor(() =>
      expect(view.container.querySelector('[data-task-state]')).toHaveTextContent('Planned'),
    );
    await act(async () => {
      task.state = { state: 'done' };
      send.enqueue(
        new TextEncoder().encode(
          ': live\n\nid: 1\ndata: {"id":1,"at":1,"actor":"engine","project":"p","kind":"task.done","payload":{"task":"task"}}\n\n',
        ),
      );
    });
    await waitFor(() =>
      expect(view.container.querySelector('[data-task-state]')).toHaveTextContent('Done'),
    );
    view.unmount();
    send.close();
  });
});
describe('task panel memory', () => {
  it('restores panel choices and files placement after a fresh live app restart', async () => {
    // Only this mocked engine data survives the restart, never a client snapshot.
    const task = structuredClone(taskSource().getSnapshot().tasks[0]);
    const settings: Settings = { values: { motion: 'Off' } };
    let eventId = 0;
    vi.spyOn(globalThis, 'fetch').mockImplementation(async () => new Response(':root {}'));
    function startApp() {
      let events: ReadableStreamDefaultController<Uint8Array>;
      const fetcher = vi.fn<typeof fetch>(async (input, init) => {
        const url = String(input);
        if (url.startsWith('/events/recent'))
          return json({ cursor: 0, events: [], before: null, activity: [] });
        if (url.startsWith('/events?')) {
          const body = new ReadableStream<Uint8Array>({
            start(controller) {
              events = controller;
              controller.enqueue(new TextEncoder().encode(': live\n\n'));
              init?.signal?.addEventListener('abort', () => controller.close(), { once: true });
            },
          });
          return new Response(body, { headers: { 'dvb-version': '0.1.0' } });
        }
        if (url === '/settings' && init?.method === 'PUT') {
          const { values } = JSON.parse(String(init.body)) as Settings;
          for (const [key, value] of Object.entries(values) as [
            keyof Settings['values'],
            unknown,
          ][]) {
            const old = settings.values[key];
            settings.values[key] = structuredClone(value);
            const event = {
              id: ++eventId,
              at: 1,
              actor: 'owner',
              kind: 'settings.changed',
              payload: { key, old, new: value },
            };
            events.enqueue(
              new TextEncoder().encode(`id: ${event.id}\ndata: ${JSON.stringify(event)}\n\n`),
            );
          }
          return json({ event: eventId });
        }
        if (url === '/version') return json({ version: '0.1.0' });
        if (url === '/settings') return json(settings);
        if (url === '/tasks') return json([task]);
        if (url === '/status') return json({ running: 0, needs_you: 0 });
        if (url === `/tasks/${task.id}/diff`)
          return json({
            base: 'main',
            head: task.head,
            diff: '--- a/example.ts\n+++ b/example.ts\n+saved layout',
          });
        if (url === `/tasks/${task.id}/verdict`)
          return json({ head: task.head, findings: [], open_findings: 0, summary: 'Reviewed' });
        if (['/goals', '/models', '/projects', '/questions', '/hires'].includes(url))
          return json([]);
        throw new Error(`Unexpected request: ${url}`);
      });
      const source = createLiveSource({ base: '', fetch: fetcher });
      return { ...mount(<TaskPage id={task.id} />, source), fetcher };
    }
    function expectFilesOnRight(container: HTMLElement) {
      const files = screen.getByText('Changed files').closest('aside')!;
      const diff = screen
        .getByLabelText('Changes', { selector: '.diff-code' })
        .closest('.diff-panel')!;
      expect(files.parentElement).toBe(diff.parentElement);
      expect(container.querySelector('.diff-layout')).toHaveClass('reversed');
    }
    let app = startApp();
    const tabs = () => within(app.container.querySelector('.page-tabs')! as HTMLElement);
    try {
      await screen.findByText('Changed files');
      expect(tabs().getByRole('tab', { name: 'Review' })).toBeVisible();
      expect(tabs().getByRole('tab', { name: 'Checks' })).toBeVisible();
      expect(app.container.querySelector('.diff-layout')).not.toHaveClass('reversed');
      await app.user.click(screen.getByLabelText('Choose task panels'));
      for (const name of ['Review', 'Checks', 'Files on the right']) {
        const control = screen.getByLabelText(name, { selector: '[role="switch"]' });
        await waitFor(() => expect(control).toBeEnabled());
        await app.user.click(control);
        await waitFor(() => {
          expect(control).toBeEnabled();
          if (name === 'Files on the right') expect(control).toBeChecked();
          else expect(control).not.toBeChecked();
        });
      }
      expect(settings.values.task_panels).toEqual({ review: false, checks: false });
      expect(settings.values.task_files_right).toBe(true);
      expect(app.fetcher.mock.calls.filter(([, init]) => init?.method === 'PUT')).toHaveLength(3);
      expectFilesOnRight(app.container);
      app.unmount(); // StoreProvider disposes the source and aborts its event stream.
      app = startApp(); // New provider, source, snapshot, subscriptions and HTTP/event transport.
      await screen.findByText('Changed files');
      expect(tabs().getByRole('tab', { name: 'Changes' })).toBeVisible();
      expect(tabs().queryByRole('tab', { name: 'Review' })).not.toBeInTheDocument();
      expect(tabs().queryByRole('tab', { name: 'Checks' })).not.toBeInTheDocument();
      expectFilesOnRight(app.container);
      await app.user.click(screen.getByLabelText('Choose task panels'));
      expect(screen.getByLabelText('Review', { selector: '[role="switch"]' })).not.toBeChecked();
      expect(screen.getByLabelText('Checks', { selector: '[role="switch"]' })).not.toBeChecked();
      expect(
        screen.getByLabelText('Files on the right', { selector: '[role="switch"]' }),
      ).toBeChecked();
      expect(app.fetcher.mock.calls.filter(([, init]) => init?.method === 'PUT')).toHaveLength(0);
    } finally {
      app.unmount();
    }
  });
  it('writes panel choices to the saved settings and reads them back', async () => {
    const source = taskSource(),
      task = source.getSnapshot().tasks[0];
    const change = vi.spyOn(source.getSnapshot(), 'changeSettings');
    const { user } = mount(<TaskPage id={task.id} />, source);
    expect(screen.getByRole('tab', { name: 'Review' })).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Choose task panels' }));
    await user.click(screen.getByRole('switch', { name: 'Review' }));
    expect(change).toHaveBeenCalledWith({
      values: { task_panels: { review: false, checks: true } },
    });
    await user.click(screen.getByRole('switch', { name: 'Files on the right' }));
    expect(change).toHaveBeenCalledWith({ values: { task_files_right: true } });
    act(() =>
      source.set({
        settings: {
          values: { task_panels: { review: false, checks: true }, task_files_right: true },
        },
      }),
    );
    await waitFor(() => expect(screen.queryByRole('tab', { name: 'Review' })).toBeNull());
  });
  it('starts with the saved choice, not a default', () => {
    const source = taskSource(),
      task = source.getSnapshot().tasks[0];
    source.set({
      settings: {
        values: {
          ...source.getSnapshot().settings.values,
          task_panels: { review: false, checks: false },
        },
      },
    });
    mount(<TaskPage id={task.id} />, source);
    expect(screen.queryByRole('tab', { name: 'Review' })).toBeNull();
    expect(screen.queryByRole('tab', { name: 'Checks' })).toBeNull();
  });
});
describe('reroute refresh', () => {
  afterEach(() => {
    history.replaceState({}, '', '/');
  });
  it('reads the same builder run again when a reroute arrives after the first response', async () => {
    const source = taskSource(),
      task = source.getSnapshot().tasks.find((t) => t.id === 'checks')!;
    const started: Event = {
      id: 1,
      at: 1,
      actor: 'engine',
      project: task.project,
      kind: 'run.started',
      payload: {
        run: 'rb',
        task: task.id,
        role: task.role,
        hire: 'h',
        model: 'old-model',
        vendor: 'codex',
        effort: 'low',
        reason: 'Waiting.',
      },
    };
    const run = vi
      .fn()
      .mockResolvedValueOnce({
        ok: true,
        value: { model: 'old-model', vendor: 'codex', effort: 'low', reason: 'Waiting.' },
      })
      .mockResolvedValue({
        ok: true,
        value: { model: 'new-model', vendor: 'claude', effort: 'high', reason: 'Rerouted.' },
      });
    source.replies['/runs/rb/output'] = [];
    source.set({ events: [started], evidence: [], run });
    const view = mount(<TaskPage id={task.id} />, source);
    await waitFor(() =>
      expect(view.container.querySelector('[data-run-line]')).toHaveTextContent('old-model'),
    );
    expect(run).toHaveBeenCalledExactlyOnceWith('rb');
    await act(async () =>
      source.set({
        events: [
          started,
          {
            id: 2,
            at: 2,
            actor: 'engine',
            project: task.project,
            kind: 'run.rerouted',
            payload: {
              run: 'rb',
              task: task.id,
              role: task.role,
              from_model: 'old-model',
              to_model: 'new-model',
              vendor: 'claude',
              effort: 'high',
              limited: 'codex',
            },
          },
        ],
      }),
    );
    await waitFor(() =>
      expect(view.container.querySelector('[data-run-line]')).toHaveTextContent(
        'new-model Backend builder high Rerouted.',
      ),
    );
    expect(run).toHaveBeenCalledTimes(2);
    expect(run).toHaveBeenLastCalledWith('rb');
  });
});
describe('run evidence', () => {
  const at = (n: number) => ({ id: n, at: 1, actor: 'owner' as const });
  it('shows the model, effort and reason on one line and the fixed rerouted wording', async () => {
    mount(<TaskPage id="checks" />);
    const line = await waitFor(() => {
      const el = document.querySelector('[data-run-line]');
      expect(el).not.toBeNull();
      return el!;
    });
    expect(line).toHaveTextContent(
      'claude-opus-5-5 Backend builder high Keep the catalogue across reloads.',
    );
    expect(document.querySelector('[data-rerouted]')).toHaveTextContent(
      'codex at its limit: Backend builder runs on claude-opus-5-5 instead',
    );
  });
  it('shows the builder run on the task page after a review run has started', async () => {
    const source = taskSource(),
      base = { hire: 'h', vendor: 'codex' as const, task: 'checks' };
    const view = (id: string) =>
      id === 'rb'
        ? { model: 'builder-model', effort: 'high', reason: 'Builder reason.' }
        : { model: 'gpt-6.1-sol', effort: 'medium', reason: 'Review 77ea396' };
    source.set({
      events: [
        {
          ...at(1),
          kind: 'run.started',
          payload: {
            ...base,
            run: 'rb',
            role: 'dev',
            model: 'builder-model',
            effort: 'high',
            reason: 'Builder reason.',
          },
        },
        {
          ...at(2),
          kind: 'run.started',
          payload: {
            ...base,
            run: 'rr',
            role: 'rev',
            model: 'gpt-6.1-sol',
            effort: 'medium',
            reason: 'Review 77ea396',
            reviews: '77ea396',
          },
        },
      ] as Event[],
      run: (async (id: string) => ({ ok: true, value: view(id) })) as never,
    });
    mount(<TaskPage id="checks" />, source);
    const line = await waitFor(() => {
      const el = document.querySelector('[data-run-line]');
      expect(el).not.toBeNull();
      return el!;
    });
    expect(line).toHaveTextContent('builder-model Backend builder high Builder reason.');
  });
  it.each([
    ['builder', undefined],
    ['reviewer', '77ea396'],
  ])(
    'clears the mismatch alert once a %s retry starts, keeping it in the feed',
    async (_, reviews) => {
      const source = taskSource(),
        project = source.getSnapshot().tasks.find((t) => t.id === 'checks')!.project;
      const base = {
        hire: 'h',
        vendor: 'codex' as const,
        task: 'checks',
        role: 'dev',
        model: 'm',
        effort: 'high' as const,
        reason: 'r',
        ...(reviews ? { reviews } : {}),
      };
      source.replies['/runs/old/output'] = [];
      source.replies['/runs/new/output'] = [];
      source.set({
        events: [
          { ...at(1), project, kind: 'run.started', payload: { ...base, run: 'old' } },
          {
            ...at(2),
            project,
            kind: 'run.model',
            payload: { run: 'old', requested: 'asked-model', served: { mismatch: 'served-model' } },
          },
        ],
      });
      const { user } = mount(<TaskPage id="checks" />, source);
      const alert = await screen.findByRole('alert');
      expect(alert).toBeVisible();
      expect(alert).toHaveTextContent('Model mismatch: asked for asked-model, served served-model');
      await act(async () =>
        source.set({
          events: [
            ...source.getSnapshot().events,
            { ...at(3), project, kind: 'run.started', payload: { ...base, run: 'new' } },
          ],
        }),
      );
      await waitFor(() => expect(screen.queryByRole('alert')).not.toBeInTheDocument());
      await user.click(screen.getByRole('tab', { name: 'Feed' }));
      const feed = within(await screen.findByRole('tabpanel', { name: 'Feed' })).getByLabelText(
        'Activity feed',
      );
      await waitFor(() =>
        expect(
          within(feed).getByText('Model mismatch: asked for asked-model, served served-model'),
        ).toBeVisible(),
      );
    },
  );
  it('shows the mismatch alert for the latest run', async () => {
    const source = taskSource(),
      project = source.getSnapshot().tasks.find((t) => t.id === 'checks')!.project;
    source.set({
      events: [
        {
          ...at(1),
          project,
          kind: 'run.started',
          payload: {
            hire: 'h',
            vendor: 'codex',
            task: 'checks',
            run: 'r',
            role: 'dev',
            model: 'm',
            effort: 'high',
            reason: 'r',
          },
        },
        {
          ...at(2),
          project,
          kind: 'run.model',
          payload: { run: 'r', requested: 'asked-model', served: { mismatch: 'served-model' } },
        },
      ] as Event[],
    });
    mount(<TaskPage id="checks" />, source);
    expect(await screen.findByRole('alert')).toHaveTextContent(
      'asked for asked-model, served served-model',
    );
  });
});
describe('summary', () => {
  it('Task page shows the planned summary and the role beside the model', async () => {
    const source = taskSource(),
      snap = source.getSnapshot(),
      task = snap.tasks.find((t) => t.id === 'resume')!;
    const role = snap.roles[task.project].find((r) => r.id === task.role || r.slug === task.role)!;
    source.set({
      events: [
        ...snap.events,
        {
          id: 9001,
          at: 1,
          project: task.project,
          actor: 'engine',
          kind: 'plan.proposed',
          payload: {
            goal: task.goal,
            tasks: [
              {
                task: task.id,
                title: task.title,
                role: task.role,
                review: task.review,
                summary: 'Keep removed records removed when resuming.',
              },
            ],
          },
        } as never,
      ],
    });
    mount(<TaskPage id="resume" />, source);
    expect(
      await screen.findByText('Keep removed records removed when resuming.'),
    ).toBeInTheDocument();
    expect(
      await screen.findByText(new RegExp(role.name), { selector: '[data-run-line]' }),
    ).toBeInTheDocument();
  });
});

it.each(['preparing', 'in_review'] as const)(
  'displays and clears the memory reason from task snapshots for %s',
  async (state) => {
    const source = taskSource(),
      task = source.getSnapshot().tasks[0];
    source.set({
      tasks: [{ ...task, state: { state }, reason: 'Waiting for memory' }],
      events: [],
    });
    mount(
      <>
        <TaskBoard project={task.project} />
        <TaskPage id={task.id} />
      </>,
      source,
    );
    expect(screen.getAllByText('Waiting for memory', { exact: true })).toHaveLength(2);
    await act(async () => {
      source.set({ tasks: [{ ...task, state: { state }, reason: undefined }] });
    });
    expect(screen.queryByText('Waiting for memory', { exact: true })).not.toBeInTheDocument();
  },
);

it('refreshes output during the same run even when detail history excludes output', async () => {
  const source = createFixtureSource();
  source.replies['/runs/run-resume/output'] = [
    { id: 200, at: 200, item: { text: 'First line' } },
  ] satisfies components['schemas']['RunOutputLine'][];
  mount(<TaskPage id="resume" />, source);
  expect(await screen.findByText('First line')).toBeInTheDocument();
  source.replies['/runs/run-resume/output'] = [
    { id: 201, at: 201, item: { text: 'New line' } },
  ] satisfies components['schemas']['RunOutputLine'][];
  act(() =>
    source.set({
      events: [
        ...source.getSnapshot().events,
        {
          id: 999,
          at: 999,
          actor: 'engine',
          project: 'harbour',
          kind: 'run.output',
          payload: { run: 'run-resume', item: { text: 'New line' } },
        },
      ],
    }),
  );
  expect(await screen.findByText('New line')).toBeInTheDocument();
});

it('keeps the last run lines on a failed task and drops them once it is done', async () => {
  const source = createFixtureSource();
  source.replies['/runs/run-resume/output'] = [
    { id: 200, at: 200, item: { tool_call: { name: 'dvb run verdict' } } },
  ] satisfies components['schemas']['RunOutputLine'][];
  const seed = source.getSnapshot().tasks.find((t) => t.id === 'resume')!;
  source.set({ tasks: [{ ...seed, state: { state: 'failed', reason: 'Build did not pass' } }] });
  const { container } = mount(<TaskPage id="resume" />, source);
  const card = container.querySelector('.task-live')!;
  expect(await within(card as HTMLElement).findByText('Tool: dvb run verdict')).toBeInTheDocument();
  act(() => source.set({ tasks: [{ ...seed, state: { state: 'done' } }] }));
  await waitFor(() => expect(card).toHaveTextContent('This task is finished.'));
  expect(card).not.toHaveTextContent('Tool: dvb run verdict');
});

describe('task headline', () => {
  it.each([
    ['done', 'This task is finished.'],
    ['cancelled', 'This task was cancelled.'],
    ['running', 'Following the latest work.'],
  ] as const)('a %s task without a reason shows %s', async (state, headline) => {
    const source = createFixtureSource();
    const seed = source.getSnapshot().tasks.find((t) => t.id === 'resume')!;
    source.set({ tasks: [{ ...seed, reason: undefined, state: { state } } as Task] });
    const { container } = mount(<TaskPage id="resume" />, source);
    const h2 = container.querySelector('.task-live h2')!;
    await waitFor(() => expect(h2).toHaveTextContent(headline));
    if (state !== 'running') expect(h2).not.toHaveTextContent('Following the latest work.');
  });
});
describe('task order and builder outcomes', () => {
  function focus(change: Partial<Task>) {
    const source = taskSource();
    source.set({
      tasks: source.getSnapshot().tasks.map((t) => (t.id === 'focus' ? { ...t, ...change } : t)),
    });
    mount(<TaskPage id="focus" />, source);
    return document.querySelector('.task-live') as HTMLElement;
  }
  it('shows a waiting task as waiting for its prerequisite title', () => {
    const live = focus({ state: { state: 'planned' }, waiting_for: ['resume'] });
    expect(within(live).getByText('Waiting')).toHaveAttribute('data-task-state');
    expect(within(live).getByRole('heading', { level: 2 })).toHaveTextContent(
      'Waiting for Keep removed records removed',
    );
  });
  it.each([
    [
      { status: 'partial', reason: 'out_of_role', detail: 'the API needs a backend task' },
      'Stopped: out of my role: the API needs a backend task',
    ],
    [
      { status: 'partial', reason: 'completion_unmet', detail: 'verify passed twice' },
      'Stopped: done-when unmet: verify passed twice',
    ],
    [{ status: 'blocked', prerequisite: 'resume' }, 'Blocked on: Keep removed records removed'],
    [{ status: 'blocked', prerequisite: 'the import API' }, 'Blocked on: the import API'],
  ] as const)('shows the builder outcome %o in plain words', (outcome, text) => {
    const live = focus({
      state: { state: 'failed', reason: 'no changes: the run changed nothing' },
      outcome,
    });
    expect(within(live).getByRole('heading', { level: 2 })).toHaveTextContent(text);
  });
  it.each([
    [{ state: 'in_review' }, 'In review'],
    [{ state: 'merging' }, 'Ready to merge'],
  ] as const)('keeps the usual wording for %o after the owner waives done-when', (state, label) => {
    const live = focus({
      state,
      outcome: { status: 'partial', reason: 'completion_unmet', detail: 'verify passed twice' },
    });
    expect(live.querySelector('[data-task-state]')).toHaveTextContent(label);
    expect(within(live).getByRole('heading', { level: 2 })).not.toHaveTextContent('Stopped');
  });
  it('keeps the usual wording for a complete outcome', () => {
    const live = focus({ outcome: { status: 'complete' } });
    expect(within(live).getByRole('heading', { level: 2 })).toHaveTextContent(
      'This task is finished.',
    );
  });
});
