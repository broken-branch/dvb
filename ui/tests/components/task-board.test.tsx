import type { Event } from '@/lib/data/types';
import { it, expect, describe, vi } from 'vitest';
import { screen, act, within } from '@testing-library/react';
import { mount } from '../support/render';
import { TaskBoard } from '@/components/dvb/task-board';
import { createFixtureSource } from '../support/render';
import { ProjectPage } from '@/screens/project';
const renders = vi.hoisted(() => vi.fn());
vi.mock('@/components/dvb/card', async (original) => {
  const actual = await original<typeof import('@/components/dvb/card')>();
  return {
    ...actual,
    Card: (props: React.ComponentProps<typeof actual.Card>) => {
      if (props.className?.includes('kanban-card')) renders(props.className);
      return <actual.Card {...props} />;
    },
  };
});
describe('completed', () => {
  it('shows the ten newest Done cards, then all, then ten again', async () => {
    const source = createFixtureSource(),
      seed = source.getSnapshot().tasks[0];
    const tasks = Array.from({ length: 14 }, (_, i) => ({
      ...seed,
      id: `done-${i}`,
      title: `Completed ${i}`,
      state: { state: 'done' as const },
    }));
    const events: Event[] = tasks.map((t, i) => ({
      id: i + 1,
      at: i,
      actor: 'engine',
      kind: 'task.done',
      payload: { task: t.id },
      project: t.project,
    }));
    source.set({ tasks: tasks.toReversed(), events });
    const { user } = mount(<TaskBoard />, source);
    expect(screen.getAllByRole('link', { name: /Completed/ })).toHaveLength(10);
    expect(screen.queryByRole('link', { name: 'Completed 3' })).not.toBeInTheDocument();
    expect(screen.getAllByRole('link', { name: /Completed/ })[0]).toHaveTextContent('Completed 13');
    await user.click(screen.getByRole('button', { name: /Show all 14/ }));
    expect(screen.getAllByRole('link', { name: /Completed/ })).toHaveLength(14);
    await user.click(screen.getByRole('button', { name: /Show fewer/ }));
    expect(screen.getAllByRole('link', { name: /Completed/ })).toHaveLength(10);
  });
});
describe('mirroring', () => {
  it('board notices count actual mirror failures once and retain the latest observation', async () => {
    const source = createFixtureSource(),
      seed = source.getSnapshot().tasks[0];
    const tasks = Array.from({ length: 21 }, (_, i) => ({ ...seed, id: `mirror-${i}` }));
    const events: Event[] = tasks.map((t, i) => ({
      id: i + 1,
      at: i,
      actor: 'engine',
      project: t.project,
      kind: 'github.mirror_observed',
      payload: { goal: t.goal, task: t.id, mirrored: false, detail: 'Unavailable' },
    }));
    source.set({
      tasks,
      events,
      projects: source.getSnapshot().projects.map((p) => ({ ...p, ci: 'no_ci' })),
    });
    mount(<TaskBoard />, source);
    expect(screen.getAllByText('21 not mirrored on GitHub')).toHaveLength(1);
    expect(screen.getAllByText('No CI · Review required')).toHaveLength(1);
    await act(async () =>
      source.set({
        events: [
          ...events,
          {
            id: 22,
            at: 22,
            actor: 'engine',
            project: seed.project,
            kind: 'github.mirror_observed',
            payload: { goal: seed.goal, task: tasks[0].id, mirrored: true },
          },
        ],
      }),
    );
    expect(screen.getByText('20 not mirrored on GitHub')).toBeInTheDocument();
  });
});
describe('GitHub publication modes', () => {
  it('shows no CI prominently and preserves independent review wording on cards', () => {
    const source = createFixtureSource(),
      snapshot = source.getSnapshot();
    source.set({
      projects: snapshot.projects.map((p) => (p.id === 'harbour' ? { ...p, ci: 'no_ci' } : p)),
    });
    mount(<ProjectPage id="harbour" />, source);
    expect(screen.getByText('No CI')).toBeInTheDocument();
    expect(screen.getAllByText('No CI · Review required').length).toBeGreaterThan(0);
  });
  it('shows the actual unmirrored task count above the board and clears it after recovery', async () => {
    const source = createFixtureSource(),
      task = source.getSnapshot().tasks.find((t) => t.project === 'harbour')!;
    const event = {
      id: 900,
      at: 0,
      actor: 'engine' as const,
      project: 'harbour',
      kind: 'github.mirror_observed' as const,
      payload: { goal: task.goal, task: task.id, mirrored: false, detail: 'SCAN_HIT' },
    };
    source.set({ events: [event] });
    mount(<ProjectPage id="harbour" />, source);
    expect(screen.getByText('1 not mirrored on GitHub')).toBeInTheDocument();
    await act(async () => {
      source.set({
        events: [event, { ...event, id: 901, payload: { ...event.payload, mirrored: true } }],
      });
    });
    expect(screen.queryByText('1 not mirrored on GitHub')).not.toBeInTheDocument();
  });
});
describe('rendering', () => {
  it('unrelated event rows leave cards alone; task changes update only the affected card', async () => {
    const { source } = mount(<TaskBoard />);
    const count = document.querySelectorAll('.kanban-card').length;
    expect(count).toBeGreaterThan(1);
    renders.mockClear();
    await act(async () =>
      source.set({
        events: [
          {
            id: 900,
            at: 0,
            actor: 'engine',
            kind: 'project.seen',
            project: 'harbour',
            payload: {},
          },
        ],
      }),
    );
    expect(renders).not.toHaveBeenCalled();
    const tasks = source.getSnapshot().tasks;
    await act(async () =>
      source.set({ tasks: tasks.map((t, i) => (i === 0 ? { ...t, title: 'Changed title' } : t)) }),
    );
    expect(renders).toHaveBeenCalledTimes(1);
    expect(screen.getByRole('link', { name: /Changed title/ })).toBeInTheDocument();
  });
});
describe('role', () => {
  it('missing board role never borrows another role', () => {
    const source = createFixtureSource(),
      task = source.getSnapshot().tasks[0];
    source.set({ tasks: [{ ...task, role: 'missing-role' }], events: [] });
    mount(<TaskBoard />, source);
    expect(document.querySelector('.kanban-agent')).toHaveTextContent('Unknown role');
    expect(document.querySelector('.kanban-agent')).not.toHaveTextContent('Orchestrator');
  });
});
describe('routine', () => {
  const board = (task: Partial<import('@/lib/data/types').Task>) => {
    const source = createFixtureSource(),
      snapshot = source.getSnapshot(),
      seed = snapshot.tasks.find((t) => t.role === 'harbour-frontend')!;
    source.set({
      tasks: [{ ...seed, ...task }],
      events: [],
      hires: [
        ...snapshot.hires,
        {
          id: 'hire-harbour-frontend-routine',
          role: 'harbour-frontend',
          model: 'gpt-6.1-sol',
          effort: 'medium',
          status: 'active',
          routine: true,
        },
      ],
    });
    mount(<TaskBoard />, source);
    return document.querySelector('.kanban-card')!;
  };
  it('shows the default hire and no routine mark on a task that is not routine', () => {
    const card = board({});
    expect(card).not.toHaveTextContent('Routine');
    expect(card.querySelector('.kanban-agent')).toHaveTextContent('claude-sonnet-5-5');
  });
  it('marks a routine task and names the routine hire before its first round', () => {
    const card = board({ routine: true });
    expect(within(card as HTMLElement).getByText('Routine')).toBeInTheDocument();
    expect(card.querySelector('.kanban-agent')).toHaveTextContent('gpt-6.1-sol');
  });
  it('shows the model the last round ran on', () => {
    expect(
      board({ routine: true, round_model: 'claude-opus-5-5' }).querySelector('.kanban-agent'),
    ).toHaveTextContent('claude-opus-5-5');
    document.body.innerHTML = '';
    expect(board({ round_model: 'gpt-6.1-sol' }).querySelector('.kanban-agent')).toHaveTextContent(
      'gpt-6.1-sol',
    );
  });
});
