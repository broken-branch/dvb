import { event, page } from '../support/rows';
import { json } from '../support/api';
import type { DataStore, Usage as EngineUsage } from '@/lib/data/types';
import { it, expect, vi, describe } from 'vitest';
import { screen, waitFor, within, act } from '@testing-library/react';
import { mount } from '../support/render';
import { Usage, averageTokenCost } from '@/screens/usage';
import { createFixtureSource } from '../support/render';
import { createLiveSource } from '@/lib/data/api';
describe('costs', () => {
  it('shows $0.66 per million tokens for $4.69 over 7,135,353 tokens', async () => {
    const source = createFixtureSource();
    source.set({
      usage: async () => ({
        ok: true,
        value: {
          windows: [],
          rows: [
            {
              project: 'harbour',
              role: 'harbour-frontend',
              model: 'example',
              vendor: 'codex',
              uncached_in: 7_135_353,
              cached_in: 0,
              out: 0,
              est_cost: 4.69,
            },
          ],
        },
      }),
    });
    mount(<Usage />, source);
    expect(await screen.findByText('$0.66')).toBeInTheDocument();
    expect(averageTokenCost(0, 0)).toBe('—');
  });
  it.each(['pointer', 'keyboard'])(
    'associates row costs with their labels and opens their explanations by %s',
    async (input) => {
      const source = createFixtureSource();
      source.set({
        usage: async () => ({
          ok: true,
          value: {
            windows: [],
            rows: [
              {
                project: 'harbour',
                role: 'harbour-frontend',
                model: 'example',
                vendor: 'codex',
                uncached_in: 7_135_353,
                cached_in: 0,
                out: 0,
                est_cost: 4.69,
              },
            ],
          },
        }),
      });
      const { user } = mount(<Usage />, source);
      const equivalent = await screen.findByRole('group', { name: 'Est. equivalent' });
      const average = screen.getByRole('group', { name: 'Avg. per 1M tokens' });
      expect(within(equivalent).getByText('$4.69')).toBeInTheDocument();
      expect(within(average).getByText('$0.66')).toBeInTheDocument();
      const equivalentTip = within(equivalent).getByLabelText('About Est. equivalent');
      const averageTip = within(average).getByLabelText('About Avg. per 1M tokens');
      if (input === 'pointer') {
        await user.hover(equivalentTip);
        expect(await screen.findByRole('tooltip')).toHaveTextContent(
          "What these tokens would cost at the vendor's published API prices. For comparison only; your subscriptions are not charged this.",
        );
        await user.unhover(equivalentTip);
      } else {
        // jsdom does not track keyboard modality for :focus-visible like Chrome.
        const matches = averageTip.matches.bind(averageTip);
        vi.spyOn(averageTip, 'matches').mockImplementation(
          (selector) => selector === ':focus-visible' || matches(selector),
        );
        act(() => equivalentTip.focus());
        await user.tab();
        expect(averageTip).toHaveFocus();
        expect(await screen.findByRole('tooltip')).toHaveTextContent(
          "What one million of these tokens costs on average at the vendor's published API prices: Est. equivalent divided by tokens, times 1,000,000. For comparison only.",
        );
      }
      await user.keyboard('{Escape}');
      await waitFor(() => expect(screen.queryByRole('tooltip')).not.toBeInTheDocument());
      if (input === 'keyboard') expect(averageTip).toHaveFocus();
    },
  );
});
describe('filters', () => {
  it('Usage filters rows by project and shows server refusal text', async () => {
    const { source, user } = mount(<Usage />);
    await screen.findAllByText('claude-opus-5-5');
    const rows = (source.replies['/usage'] as EngineUsage).rows.slice(7);
    source.replies['/usage'] = { rows, windows: [] } satisfies EngineUsage;
    await user.click(screen.getByLabelText('Usage project'));
    await user.click(within(screen.getByRole('listbox')).getByRole('option', { name: 'Dispatch' }));
    await waitFor(() => expect(document.querySelectorAll('.role-table-row')).toHaveLength(7));
    act(() =>
      source.set({
        usage: async () => ({ ok: false, refusal: { code: 'NOT_IMPLEMENTED', detail: 'Not yet' } }),
      }),
    );
    expect(await screen.findByRole('alert')).toHaveTextContent('Not yet');
  });
});
describe('refresh', () => {
  it('refreshes usage after eviction and every reconnect usage update', async () => {
    const streams: ReadableStreamDefaultController<Uint8Array>[] = [];
    let reads = 0;
    const usageEvent = (id: number) =>
      event(id, 'run.usage', { run: 'r', uncached_in: id, cached_in: 0, out: 0, est_cost: 0 });
    const fetcher = vi.fn<typeof fetch>(async (input) => {
      const url = new URL(String(input), 'http://localhost');
      if (url.pathname === '/events')
        return new Response(
          new ReadableStream<Uint8Array>({
            start(c) {
              streams.push(c);
              c.enqueue(new TextEncoder().encode(': live\n\n'));
            },
          }),
          { headers: { 'dvb-version': '0.1.0' } },
        );
      if (url.pathname === '/events/recent')
        return json({
          ...page(Array.from({ length: 200 }, (_, i) => usageEvent(i + 1))),
          cursor: 200,
        });
      if (url.pathname === '/usage') {
        reads++;
        return json({ rows: [], windows: [] });
      }
      if (url.pathname === '/version') return json({ version: '0.1.0' });
      if (url.pathname === '/status') return json({ running: 0, needs_you: 0 });
      if (url.pathname === '/models') return json({ models: [] });
      if (url.pathname === '/settings') return json({ values: {} });
      return json([]);
    });
    const live = createLiveSource({ base: '', fetch: fetcher, version: '0.1.0', reconnectMs: 1 });
    const { unmount } = mount(<Usage />, live);
    await waitFor(() => expect(live.getSnapshot().connection.state).toBe('connected'));
    const prior = reads;
    const emit = (stream: number, id: number) =>
      streams[stream].enqueue(
        new TextEncoder().encode(`id: ${id}\ndata: ${JSON.stringify(usageEvent(id))}\n\n`),
      );
    await act(async () => emit(0, 201));
    await waitFor(() => expect(reads).toBeGreaterThan(prior));
    expect(live.getSnapshot().events.filter((e) => e.kind === 'run.usage')).toHaveLength(200);
    const afterEviction = reads;
    streams[0].close();
    await waitFor(() => expect(streams).toHaveLength(2));
    expect(fetcher.mock.calls.some(([url]) => url === '/events?after=201')).toBe(true);
    await act(async () => emit(1, 202));
    await waitFor(() => expect(reads).toBeGreaterThan(afterEviction));
    expect(live.getSnapshot().usageVersion).toBe(202);
    unmount();
    streams[1].close();
  });
});
describe('check again', () => {
  const windows: EngineUsage['windows'] = [
    { vendor: 'claude', window: '5-hour', used_pct: 40, limited: false },
    { vendor: 'codex', window: 'weekly', used_pct: 100, limited: true, resets_at: 1793802600000 },
  ];
  function paused(checkUsage: DataStore['checkUsage']) {
    let current = windows;
    const source = createFixtureSource();
    source.set({
      usage: async () => ({ ok: true, value: { rows: [], windows: current } }),
      checkUsage,
    });
    // Stands in for the reread a vendor.limited or vendor.resumed event causes.
    const reread = async (codex: Partial<EngineUsage['windows'][number]>) => {
      act(() => {
        current = [windows[0], { ...windows[1], ...codex }];
        source.set({ usageVersion: source.getSnapshot().usageVersion + 1 });
      });
      await screen.findByRole('heading', { name: 'codex' });
    };
    return { ...mount(<Usage />, source), reread };
  }
  const card = (vendor: string) =>
    screen.getByRole('heading', { name: vendor }).closest('.usage-window') as HTMLElement;

  it('shows one button, on the paused row only', async () => {
    paused(async () => ({ ok: true, value: windows[1] }));
    await screen.findByRole('heading', { name: 'codex' });
    expect(screen.getAllByRole('button', { name: 'Check again' })).toHaveLength(1);
    expect(within(card('codex')).getByRole('button', { name: 'Check again' })).toBeInTheDocument();
    expect(within(card('claude')).queryByRole('button')).not.toBeInTheDocument();
  });

  it('shows it is checking, takes one press, then says the vendor is running again', async () => {
    let reply!: () => void;
    const checkUsage = vi.fn(
      () =>
        new Promise<{ ok: true; value: EngineUsage['windows'][number] }>((resolve) => {
          reply = () =>
            resolve({ ok: true, value: { ...windows[1], used_pct: 3, limited: false } });
        }),
    );
    const { user, reread } = paused(checkUsage);
    await user.click(await screen.findByRole('button', { name: 'Check again' }));
    const checking = within(card('codex')).getByRole('button', { name: 'Checking…' });
    expect(checking).toBeDisabled();
    await user.click(checking);
    expect(checkUsage).toHaveBeenCalledTimes(1);
    expect(checkUsage).toHaveBeenCalledWith('codex');
    await act(async () => reply());
    await reread({ used_pct: 3, limited: false });
    expect(await within(card('codex')).findByRole('status')).toHaveTextContent(
      'Running again: the usage limit has lifted.',
    );
    expect(within(card('codex')).getByText('Available')).toBeInTheDocument();
  });

  it('says the vendor is still limited until the reset time the reply gives', async () => {
    const resets = 1794000000000;
    const { user } = paused(async () => ({
      ok: true,
      value: { ...windows[1], resets_at: resets },
    }));
    await user.click(await screen.findByRole('button', { name: 'Check again' }));
    expect(await within(card('codex')).findByRole('status')).toHaveTextContent(
      `Still limited until ${new Date(resets).toLocaleString()}.`,
    );
    expect(within(card('codex')).getByRole('button', { name: 'Check again' })).toBeEnabled();
  });

  it('drops a result once a reread changes whether the vendor is paused', async () => {
    let limited = true;
    const { user, reread } = paused(async () => ({
      ok: true,
      value: { ...windows[1], limited, resets_at: 1794000000000 },
    }));
    await user.click(await screen.findByRole('button', { name: 'Check again' }));
    expect(await within(card('codex')).findByRole('status')).toHaveTextContent('Still limited');
    await reread({ used_pct: 0, limited: false, resets_at: undefined });
    expect(await within(card('codex')).findByText('Available')).toBeInTheDocument();
    expect(within(card('codex')).queryByRole('status')).not.toBeInTheDocument();
    await reread({ limited: true });
    expect(await within(card('codex')).findByText('Usage paused')).toBeInTheDocument();
    expect(within(card('codex')).queryByRole('status')).not.toBeInTheDocument();

    limited = false;
    await user.click(screen.getByRole('button', { name: 'Check again' }));
    await reread({ used_pct: 3, limited: false });
    expect(await within(card('codex')).findByRole('status')).toHaveTextContent('Running again');
    await reread({ limited: true });
    expect(await within(card('codex')).findByText('Usage paused')).toBeInTheDocument();
    expect(within(card('codex')).queryByRole('status')).not.toBeInTheDocument();
    await reread({ limited: false });
    expect(await within(card('codex')).findByText('Available')).toBeInTheDocument();
    expect(within(card('codex')).queryByRole('status')).not.toBeInTheDocument();
  });

  it('shows a refusal sentence on the row', async () => {
    const detail = 'codex gave no usage reading within 45 seconds';
    const { user } = paused(async () => ({
      ok: false,
      refusal: { code: 'NO_USAGE_READING', detail },
    }));
    await user.click(await screen.findByRole('button', { name: 'Check again' }));
    expect(await within(card('codex')).findByRole('alert')).toHaveTextContent(detail);
  });
});
