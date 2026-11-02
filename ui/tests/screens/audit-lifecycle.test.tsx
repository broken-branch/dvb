import { act, screen, waitFor } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { Setup, FIRST_GOAL } from '@/screens/setup';
import { AddProject, ProjectPage } from '@/screens/project';
import { Usage } from '@/screens/usage';
import { createLiveSource } from '@/lib/data/api';
import type { Project, Usage as UsageData } from '@/lib/data/types';
import { createFixtureSource, mount } from '../support/render';
import { flush, json } from '../support/api';

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}

const limited: UsageData = {
  rows: [],
  windows: [{ vendor: 'codex', window: 'weekly', limited: true, used_pct: 100 }],
};

describe('setup partial success', () => {
  it.each(['typed', 'default'])(
    'keeps the created project and retries only the refused %s first goal',
    async (draft) => {
      const source = createFixtureSource();
      source.set({ projects: [] });
      source.replies['/github'] = {
        connected: true,
        organization: 'example-org',
        install_url: null,
      };
      source.replies['/projects/project-example/issues'] = [];
      source.replies['/projects/project-example/issues/counts'] = {
        open: 0,
        trusted: 0,
        labels: [],
      };
      const goal = draft === 'typed' ? 'Keep this first goal' : FIRST_GOAL;
      const created = {
        ...(source.replies['POST /projects'] as Project),
        issues_found: { open: 2, trusted: 1, labels: [] },
      };
      const add = vi
        .spyOn(source.getSnapshot(), 'addProject')
        .mockResolvedValueOnce({ ok: true, value: created })
        .mockResolvedValue({
          ok: false,
          refusal: { code: 'INVALID_VALUE', detail: 'Name already exists' },
        });
      const state = vi.spyOn(source.getSnapshot(), 'stateGoal').mockResolvedValueOnce({
        ok: false,
        refusal: { code: 'INTERNAL_ERROR', detail: 'Goal could not be saved' },
      });
      const { user } = mount(<Setup />, source);
      await user.click(screen.getByRole('button', { name: 'Skip for now' }));
      await user.click(await screen.findByRole('button', { name: 'Continue' }));
      await user.click(screen.getByRole('button', { name: 'Skip' }));
      await user.type(screen.getByLabelText('Repository'), 'example-org/notes');
      if (draft === 'typed') {
        await user.clear(screen.getByLabelText('First goal'));
        await user.type(screen.getByLabelText('First goal'), goal);
      }
      await waitFor(() => expect(screen.getByRole('switch', { name: 'tests' })).toBeChecked());
      await user.click(screen.getByRole('button', { name: 'Create project & see plan' }));
      expect(await screen.findByRole('alert')).toHaveTextContent('Goal could not be saved');
      expect(screen.getByLabelText('First goal')).toHaveValue(goal);
      // No event is required to remember the successful POST /projects receipt.
      await user.click(
        screen.getByRole('button', { name: /^(Set goal|Create project) & see plan$/ }),
      );
      expect(add).toHaveBeenCalledTimes(1);
      await waitFor(() => expect(state).toHaveBeenCalledTimes(2));
      expect(state).toHaveBeenLastCalledWith('project-example', { text: goal });
    },
  );
});

describe('project edits during a save', () => {
  it('keeps brief text typed after the submitted text and does not label it saved', async () => {
    const source = createFixtureSource();
    source.replies['/projects/harbour/brief'] = { text: 'Original' };
    const pending = deferred<Response>();
    const fetch = source.fetch.getMockImplementation()!;
    source.fetch.mockImplementation((url, options) =>
      String(url).endsWith('/brief') && options?.method === 'PUT'
        ? pending.promise
        : fetch(url, options),
    );
    const { user } = mount(<ProjectPage id="harbour" tab="brief" />, source);
    const input = await screen.findByLabelText('Project brief');
    await waitFor(() => expect(input).toHaveValue('Original'));
    await user.clear(input);
    await user.type(input, 'Submitted');
    await user.click(screen.getByRole('button', { name: 'Save brief' }));
    await user.clear(input);
    await user.type(input, 'New unsaved work');
    await act(async () => pending.resolve(new Response(JSON.stringify({ text: 'Submitted' }))));
    expect(input).toHaveValue('New unsaved work');
    expect(screen.queryByText('Brief saved.')).not.toBeInTheDocument();
  });

  it.each(['keep editing', 'close and reopen'])(
    'keeps a newer goal draft when the previous save completes: %s',
    async (mode) => {
      const source = createFixtureSource();
      const pending = deferred<Response>();
      const fetch = source.fetch.getMockImplementation()!;
      source.fetch.mockImplementation((url, options) =>
        String(url).endsWith('/redirect') && options?.method === 'POST'
          ? pending.promise
          : fetch(url, options),
      );
      const { user } = mount(<ProjectPage id="harbour" />, source);
      await user.click(screen.getByRole('button', { name: 'Change course' }));
      await user.clear(screen.getByLabelText('Goal'));
      await user.type(screen.getByLabelText('Goal'), 'Submitted');
      await user.click(screen.getByRole('button', { name: 'Save direction' }));
      if (mode === 'close and reopen') {
        await user.click(screen.getByRole('button', { name: 'Cancel' }));
        await user.click(screen.getByRole('button', { name: 'Change course' }));
      }
      await user.clear(screen.getByLabelText('Goal'));
      await user.type(screen.getByLabelText('Goal'), 'New unsaved goal');
      await act(async () =>
        pending.resolve(
          new Response(
            JSON.stringify({
              ...source.getSnapshot().goals[0],
              text: 'Submitted',
            }),
          ),
        ),
      );
      expect(screen.getByLabelText('Goal')).toHaveValue('New unsaved goal');
      expect(screen.getByRole('button', { name: 'Save direction' })).toBeEnabled();
    },
  );
});

describe('usage across reconnect', () => {
  it('retries a failed usage read when the engine reconnects without a usage event', async () => {
    const source = createFixtureSource();
    const usage = vi
      .fn(source.getSnapshot().usage)
      .mockRejectedValueOnce(new Error('Engine restarting'))
      .mockResolvedValue({ ok: true, value: limited });
    source.set({ connection: { state: 'reconnecting' }, usage });
    mount(<Usage />, source);
    expect(await screen.findByRole('alert')).toHaveTextContent('Engine restarting');
    await act(async () =>
      source.set({ connection: { state: 'connected', engineVersion: '0.1.0' } }),
    );
    expect(await screen.findByRole('button', { name: 'Check again' })).toBeEnabled();
    expect(usage).toHaveBeenCalledTimes(2);
  });

  it('releases an old check after reconnect and ignores its late body while a new check is pending', async () => {
    const source = createFixtureSource();
    source.replies['/usage'] = limited;
    const old = deferred<UsageData['windows'][number]>();
    const oldResponse = new Response();
    vi.spyOn(oldResponse, 'json').mockImplementation(() => old.promise);
    const current = deferred<Response>();
    const fetch = source.fetch.getMockImplementation()!;
    let checks = 0;
    source.fetch.mockImplementation((url, options) =>
      String(url).endsWith('/usage/codex/check')
        ? ++checks === 1
          ? Promise.resolve(oldResponse)
          : current.promise
        : fetch(url, options),
    );
    const { user } = mount(<Usage />, source);
    await user.click(await screen.findByRole('button', { name: 'Check again' }));
    expect(screen.getByRole('button', { name: 'Checking…' })).toBeDisabled();
    await act(async () => source.set({ connection: { state: 'reconnecting' } }));
    await act(async () =>
      source.set({ connection: { state: 'connected', engineVersion: '0.1.0' } }),
    );
    await user.click(await screen.findByRole('button', { name: 'Check again' }));
    expect(checks).toBe(2);
    await act(async () => old.resolve(limited.windows[0]));
    expect(screen.getByRole('button', { name: 'Checking…' })).toBeDisabled();
    expect(screen.queryByText('Still limited.')).not.toBeInTheDocument();
    await act(async () => current.resolve(new Response(JSON.stringify(limited.windows[0]))));
    expect(await screen.findByText('Still limited.')).toBeInTheDocument();
  });
});

it('does not navigate away from the new page when an abandoned add-project reply arrives', async () => {
  const source = createFixtureSource();
  source.replies['/github'] = { connected: true, organization: 'example-org', install_url: null };
  const pending = deferred<Response>();
  const fetch = source.fetch.getMockImplementation()!;
  source.fetch.mockImplementation((url, options) =>
    String(url) === '/api/projects' && options?.method === 'POST'
      ? pending.promise
      : fetch(url, options),
  );
  const { user, unmount } = mount(<AddProject />, source);
  await user.type(await screen.findByLabelText('Name'), 'notes');
  await user.type(screen.getByLabelText('Repository'), 'example-org/notes');
  await waitFor(() => expect(screen.getByRole('switch', { name: 'tests' })).toBeChecked());
  await user.click(screen.getByRole('button', { name: 'Add project' }));
  await waitFor(() =>
    expect(
      source.fetch.mock.calls.some(
        ([url, options]) => url === '/api/projects' && options?.method === 'POST',
      ),
    ).toBe(true),
  );
  unmount();
  history.replaceState({}, '', '/#usage');
  await act(async () =>
    pending.resolve(new Response(JSON.stringify(source.replies['POST /projects']))),
  );
  expect(location.hash).toBe('#usage');
});

describe.each(['usage', 'githubPrefill'] as const)('late %s headers', (readMethod) => {
  it.each(['reconnect', 'version event'])(
    'keeps the version mismatch after a %s',
    async (transition) => {
      vi.useFakeTimers();
      let version = '0.1.0';
      const streams: ReadableStreamDefaultController<Uint8Array>[] = [];
      const pending = deferred<Response>();
      const fetcher = vi.fn<typeof fetch>(async (input) => {
        const url = new URL(String(input), 'http://localhost');
        if (url.pathname === (readMethod === 'usage' ? '/usage' : '/github/prefill'))
          return pending.promise;
        if (url.pathname === '/events')
          return new Response(
            new ReadableStream<Uint8Array>({
              start(c) {
                streams.push(c);
                c.enqueue(new TextEncoder().encode(': live\n\n'));
              },
            }),
            { headers: { 'dvb-version': version } },
          );
        const value =
          url.pathname === '/version'
            ? { version }
            : url.pathname === '/events/recent'
              ? { cursor: 20, events: [], before: null, activity: [] }
              : url.pathname === '/settings'
                ? { values: {} }
                : url.pathname === '/status'
                  ? { running: 0, needs_you: 0 }
                  : url.pathname === '/models'
                    ? { models: [] }
                    : [];
        return json(value, version);
      });
      const source = createLiveSource({
        base: '',
        fetch: fetcher,
        version: '0.1.0',
        reconnectMs: 10,
      });
      source.start();
      await flush();
      expect(source.getSnapshot().connection.state).toBe('connected');
      const read = source.getSnapshot()[readMethod]();
      version = '0.1.1';
      if (transition === 'reconnect') {
        streams[0].close();
        await flush();
        await vi.advanceTimersByTimeAsync(10);
      } else {
        streams[0].enqueue(
          new TextEncoder().encode(
            `data: ${JSON.stringify({
              id: 21,
              at: 21,
              actor: 'engine',
              kind: 'engine.started',
              payload: { version },
            })}\n\n`,
          ),
        );
      }
      await flush();
      expect(source.getSnapshot().connection.state).toBe('version-mismatch');
      pending.resolve(
        json(
          readMethod === 'usage'
            ? limited
            : { organization: null, owner_name: null, owner_email: null },
          '0.1.0',
        ),
      );
      await read.catch(() => {});
      const connection = source.getSnapshot().connection;
      const before = fetcher.mock.calls.length;
      const answer = await source.getSnapshot().answer('q', { choice: 1 });
      source.dispose();
      streams.at(-1)!.close();
      expect(connection).toEqual({
        state: 'version-mismatch',
        engineVersion: '0.1.1',
        appVersion: '0.1.0',
      });
      expect(answer).toMatchObject({ ok: false, refusal: { code: 'VERSION_MISMATCH' } });
      expect(fetcher).toHaveBeenCalledTimes(before);
    },
  );
});
