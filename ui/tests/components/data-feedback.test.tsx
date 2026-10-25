import { act, renderHook, screen } from '@testing-library/react';
import { expect, it, vi, describe } from 'vitest';
import { useAction, Feed } from '@/components/dvb/data-feedback';
import { eventText } from '@/lib/data/display';
import type { Event } from '@/lib/data/types';
import { mount } from '../support/render';
describe('actions', () => {
  it('excludes duplicate calls in the same React turn and releases after errors', async () => {
    const { result } = renderHook(useAction);
    let release!: () => void;
    const operation = vi.fn(
      () =>
        new Promise<{ ok: true; value: number }>((resolve) => {
          release = () => resolve({ ok: true, value: 1 });
        }),
    );
    let first!: Promise<unknown>;
    act(() => {
      first = result.current.act(operation);
      void result.current.act(operation);
    });
    expect(operation).toHaveBeenCalledTimes(1);
    await act(async () => {
      release();
      await first;
    });
    await act(async () => {
      await result.current.act(async () => {
        throw new Error('offline');
      });
    });
    expect(result.current.busy).toBe(false);
    await act(async () => {
      await result.current.act(async () => ({ ok: true, value: 2 }));
    });
    expect(result.current.error).toBe('');
  });
});
describe('event wording', () => {
  const at = (n: number) => ({ id: n, at: 1, actor: 'owner' as const });
  it('words restores, reroutes and review stages in the feed', () => {
    const rerouted = {
      ...at(1),
      kind: 'run.rerouted',
      payload: {
        run: 'r',
        task: 't',
        role: 'Backend',
        from_model: 'a',
        to_model: 'b',
        vendor: 'claude',
        effort: 'high',
        limited: 'codex',
      },
    } as Event;
    expect(eventText(rerouted)).toBe('codex at its limit: Backend runs on b instead');
    const code: Extract<Event, { kind: 'run.started' }> = {
      ...at(5),
      project: 'harbour',
      kind: 'run.started',
      payload: {
        run: 'review',
        task: 'catalogue',
        role: 'reviewer',
        hire: 'hire',
        model: 'example',
        vendor: 'fake',
        effort: 'medium',
        reason: 'Review imports',
        reviews: 'head',
        review_stage: 'code',
      },
    };
    expect(eventText(code)).toBe('Code review started: catalogue');
    expect(
      eventText({ ...code, id: 6, payload: { ...code.payload, review_stage: 'design' } }),
    ).toBe('Design review started: catalogue');
    const verdict: Extract<Event, { kind: 'verdict.recorded' }> = {
      ...at(7),
      project: 'harbour',
      kind: 'verdict.recorded',
      payload: {
        verdict: 'verdict',
        task: 'catalogue',
        head: 'head',
        run: 'review',
        role: 'reviewer',
        model: 'example',
        vendor: 'fake',
        summary: 'Imports checked',
        findings: [],
        review_stage: 'design',
      },
    };
    expect(eventText(verdict)).toBe('Design review: 0 findings');
    expect(
      eventText({ ...verdict, id: 8, payload: { ...verdict.payload, review_stage: null } }),
    ).toBe('Review: 0 findings');

    expect(
      eventText({
        ...at(1),
        kind: 'run.model',
        payload: { run: 'r', requested: 'a', served: { mismatch: 'b' } },
      }),
    ).toBe('Model mismatch: asked for a, served b');
    expect(
      eventText({
        ...at(2),
        kind: 'settings.changed',
        payload: { key: 'theme', old: 1, new: 2, restored_from: 1 },
      }),
    ).toBe('Setting theme restored');
    expect(
      eventText({
        ...at(3),
        kind: 'hire.changed',
        payload: { id: 'h', role: 'Backend', model: 'm', effort: 'low', status: 'active' },
      }),
    ).toBe('Backend: now m');
    expect(
      eventText({
        ...at(4),
        kind: 'hire.changed',
        payload: {
          id: 'h',
          role: 'Backend',
          model: 'm',
          effort: 'low',
          status: 'active',
          restored_from: 2,
        },
      }),
    ).toBe('Backend: m restored');
  });
});
describe('feed dates', () => {
  it('feed includes a date for non-today events and only a time today', () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date('2026-10-10T12:00:00Z'));
    const event = (id: number, at: number): Event => ({
      id,
      at,
      actor: 'owner',
      project: 'harbour',
      kind: 'project.paused',
      payload: {},
    });
    const old = new Date('2026-10-09T12:00:00Z'),
      today = new Date('2026-10-10T11:00:00Z');
    mount(<Feed events={[event(1, old.getTime()), event(2, today.getTime())]} />);
    expect(screen.getByText(old.toLocaleString())).toBeInTheDocument();
    expect(screen.getByText(today.toLocaleTimeString())).toBeInTheDocument();
  });
});
describe('feed names', () => {
  it('shows the store role name and task title instead of their ids', async () => {
    mount(
      <Feed
        events={[
          {
            id: 1,
            at: Date.now(),
            actor: 'engine',
            project: 'harbour',
            kind: 'run.started',
            payload: {
              run: 'r',
              task: 'next',
              role: 'harbour-orchestrator',
              hire: 'hire-harbour-orchestrator',
              model: 'm',
              vendor: 'fake',
              effort: 'medium',
              reason: '',
            },
          },
        ]}
      />,
    );
    expect(
      await screen.findByText('Orchestrator started Add a catalogue search'),
    ).toBeInTheDocument();
  });
});
