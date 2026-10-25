import { project, goal, task, event, page } from '../../support/rows';
import type { EventPage, DataStore, Event } from '@/lib/data/types';
import { act, screen, waitFor } from '@testing-library/react';
import { expect, it, vi, describe } from 'vitest';
import { mount } from '../../support/render';
import { createSource } from '@/lib/data/source';
import { Feed, OlderEvents, PagedFeed } from '@/components/dvb/data-feedback';
import { useEventPages } from '@/lib/data/pages';
describe('history', () => {
  function FeedHarness() {
    const page = useEventPages({ limit: 12 });
    return (
      <>
        <Feed events={page.events} />
        <OlderEvents page={page} />
      </>
    );
  }
  it('loads older pages without rolling state back or duplicating concurrent feed entries', async () => {
    const s = createSource();
    let resolve!: (value: { ok: true; value: EventPage }) => void;
    const read = vi
      .fn<DataStore['eventPage']>()
      .mockResolvedValueOnce({
        ok: true,
        value: page([event(100, 'task.done', { task: 't' })], 100),
      })
      .mockImplementationOnce(
        () =>
          new Promise((r) => {
            resolve = r;
          }),
      );
    s.set({
      projects: [project],
      goals: [goal('g', 'done')],
      tasks: [task('t', 'g', { state: 'done' })],
      activity: { p: { latest: 100, fresh: 9 } },
      eventPage: read,
      settings: { values: {} },
      connection: { state: 'connected' },
    });
    const { user } = mount(<FeedHarness />, s);
    await user.click(await screen.findByRole('button', { name: 'Show older' }));
    await act(async () =>
      s.set({
        events: [event(101, 'goal.stated', { goal: 'new', text: 'Concurrent goal' })],
        goals: [goal('g', 'done'), goal('new', 'stated')],
      }),
    );
    await act(async () =>
      resolve({
        ok: true,
        value: page([event(99, 'project.paused', {}), event(100, 'task.done', { task: 't' })]),
      }),
    );
    expect(s.getSnapshot().goals.at(-1)?.text).toBe('Goal new');
    expect(s.getSnapshot().tasks[0].state.state).toBe('done');
    expect(screen.getAllByText('Task done: Task t')).toHaveLength(1);
    expect(screen.getByText('Paused', { selector: '.activity-list p' })).toBeInTheDocument();
    expect(s.getSnapshot().activity.p.fresh).toBe(9);
    expect(read).toHaveBeenLastCalledWith({ limit: 12, before: 100 }); // A reconnect can deliver more than the bounded stream window in one batch.
    // Paging must fill that gap before continuing into already loaded history.
    read.mockResolvedValueOnce({ ok: true, value: page([event(150, 'project.resumed', {})]) });
    await act(async () =>
      s.set({
        events: [
          event(200, 'run.output', { run: 'r', item: { text: 'New tail' } }),
          event(201, 'run.output', { run: 'r', item: { text: 'Latest tail' } }),
        ],
      }),
    );
    await user.click(await screen.findByRole('button', { name: 'Show older' }));
    expect(read).toHaveBeenLastCalledWith({ limit: 12, before: 200 });
    expect(screen.getByText('Resumed', { selector: '.activity-list p' })).toBeInTheDocument();
    expect(s.getSnapshot().goals.at(-1)?.text).toBe('Goal new');
  });
});
describe('reconnect', () => {
  function FeedHarness() {
    const page = useEventPages({ limit: 12 });
    return (
      <>
        <Feed events={page.events} />
        <OlderEvents page={page} />
      </>
    );
  }
  it.each([null, 30])(
    'keeps a reconnect gap when a pending historical page finishes with before=%s',
    async (historicalBefore) => {
      const s = createSource();
      let finish!: (value: { ok: true; value: EventPage }) => void;
      const read = vi
        .fn<DataStore['eventPage']>()
        .mockResolvedValueOnce({ ok: true, value: page([event(100, 'project.paused', {})], 100) })
        .mockResolvedValueOnce({ ok: true, value: page([event(50, 'project.resumed', {})], 50) })
        .mockImplementationOnce(
          () =>
            new Promise((resolve) => {
              finish = resolve;
            }),
        )
        .mockResolvedValueOnce({ ok: true, value: page([event(150, 'project.paused', {})], 140) });
      s.set({ eventPage: read, settings: { values: {} } });
      const { user } = mount(<FeedHarness />, s);
      await user.click(await screen.findByRole('button', { name: 'Show older' }));
      await waitFor(() => expect(screen.getByRole('button', { name: 'Show older' })).toBeEnabled());
      await user.click(screen.getByRole('button', { name: 'Show older' }));
      await act(async () =>
        s.set({ events: [event(300, 'project.resumed', {}), event(301, 'project.paused', {})] }),
      );
      await act(async () =>
        finish({ ok: true, value: page([event(40, 'project.paused', {})], historicalBefore) }),
      );
      await user.click(await screen.findByRole('button', { name: 'Show older' }));
      expect(read).toHaveBeenLastCalledWith({ limit: 12, before: 300 });
      expect(screen.getAllByText('Paused', { selector: '.activity-list p' })).toHaveLength(4);
    },
  );
});

describe('engine-scoped freshness', () => {
  it('keeps server-associated role rows across live refresh, older reads and reconnect', async () => {
    const source = createSource();
    const row = (id: number, title: string): Event => ({
      id,
      at: id,
      project: 'p',
      actor: { run: 'not-in-client-history' },
      kind: 'run.output',
      payload: { run: 'not-in-client-history', item: { text: title } },
    });
    const reply = (cursor: number, events: Event[], before: number | null = null): EventPage => ({
      cursor,
      events,
      before,
      activity: [],
    });
    const read = vi
      .fn<DataStore['eventPage']>()
      .mockResolvedValueOnce({ ok: true, value: reply(100, [row(90, 'Initial')], 90) })
      .mockResolvedValueOnce({ ok: true, value: reply(110, [row(105, 'Live')], 90) })
      .mockResolvedValueOnce({ ok: true, value: reply(110, [row(40, 'Older')]) })
      .mockResolvedValueOnce({ ok: true, value: reply(300, [row(290, 'Reconnected')], 200) })
      .mockResolvedValueOnce({ ok: true, value: reply(300, [row(180, 'Gap')], 105) });
    source.set({ eventPage: read, connection: { state: 'connected', engineVersion: '0.1.0' } });
    function RoleFeed() {
      const page = useEventPages({ project: 'p', role: 'role', limit: 2 });
      return (
        <>
          <Feed events={page.events} />
          <OlderEvents page={page} />
        </>
      );
    }
    const { user } = mount(<RoleFeed />, source);
    expect(await screen.findByText('Initial')).toBeInTheDocument();
    act(() => source.set({ events: [row(110, 'Unfiltered stream row')] }));
    expect(await screen.findByText('Live')).toBeInTheDocument();
    expect(screen.queryByText('Unfiltered stream row')).not.toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Show older' }));
    expect(await screen.findByText('Older')).toBeInTheDocument();
    expect(read).toHaveBeenLastCalledWith({ project: 'p', role: 'role', limit: 2, before: 90 });
    act(() => source.set({ events: [row(300, 'Another unfiltered row')] }));
    expect(await screen.findByText('Reconnected')).toBeInTheDocument();
    expect(screen.getByText('Older')).toBeInTheDocument();
    expect(screen.getByText('Gap')).toBeInTheDocument();
    expect(read).toHaveBeenLastCalledWith({ project: 'p', role: 'role', limit: 2, before: 200 });
    expect(screen.queryByRole('button', { name: 'Show older' })).not.toBeInTheDocument();
  });
});
describe('paged feed', () => {
  const line = (id: number): Event =>
    ({
      id,
      at: id,
      project: 'p',
      actor: 'engine',
      kind: 'run.output',
      payload: { run: 'r', item: { text: `Line ${id}` } },
    }) as Event;
  const page = (events: Event[], before: number | null): EventPage => ({
    cursor: 100,
    events,
    before,
    activity: [],
  });
  function Harness() {
    const page = useEventPages({ limit: 2 });
    return <PagedFeed page={page} events={page.events} />;
  }
  it('puts Show older above the list and keeps the earlier first event in place', async () => {
    const s = createSource();
    let resolve!: (value: { ok: true; value: EventPage }) => void;
    const read = vi
      .fn<DataStore['eventPage']>()
      .mockResolvedValueOnce({ ok: true, value: page([line(11), line(12)], 11) })
      .mockImplementationOnce(() => new Promise((r) => (resolve = r)));
    s.set({ eventPage: read, settings: { values: {} }, connection: { state: 'connected' } });
    // Rows sit 40px apart in document order; scrolling moves them up the viewport.
    let scrolled = 0;
    vi.mocked(window.scrollBy).mockImplementation(((o: ScrollToOptions) => {
      scrolled += o.top ?? 0;
    }) as typeof window.scrollBy);
    const rect = vi.spyOn(Element.prototype, 'getBoundingClientRect').mockImplementation(function (
      this: Element,
    ) {
      const index = Array.from(this.parentElement?.children ?? []).indexOf(this);
      return { top: 200 + index * 40 - scrolled } as DOMRect;
    });
    try {
      const { user } = mount(<Harness />, s);
      const button = await screen.findByRole('button', { name: 'Show older' });
      const feed = screen.getByRole('region', { name: 'Activity feed' });
      const lines = () => Array.from(feed.querySelectorAll('p'), (p) => p.textContent);
      expect(button.compareDocumentPosition(feed) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
      await waitFor(() => expect(lines()).toEqual(['Line 11', 'Line 12']));
      const first = screen.getByText('Line 11').closest('.activity-list > *')!;
      expect(first.getBoundingClientRect().top).toBe(200);
      await user.click(button);
      expect(read).toHaveBeenLastCalledWith({ limit: 2, before: 11 });
      await act(async () => resolve({ ok: true, value: page([line(9), line(10)], null) }));
      expect(lines()).toEqual(['Line 9', 'Line 10', 'Line 11', 'Line 12']);
      expect(first.getBoundingClientRect().top).toBe(200);
      expect(scrolled).toBe(80);
      expect(screen.queryByRole('button', { name: 'Show older' })).not.toBeInTheDocument();
      expect(feed).toHaveFocus();
    } finally {
      rect.mockRestore();
      vi.mocked(window.scrollBy).mockReset();
    }
  });
});
