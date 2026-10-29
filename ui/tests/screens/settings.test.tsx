import { flush, json } from '../support/api';
import type { Settings as SavedSettings } from '@/lib/data/types';
import { expect, it, vi, describe } from 'vitest';
import { act, fireEvent, screen, waitFor, within } from '@testing-library/react';
import { createFixtureSource } from '../support/render';
import { Settings } from '@/screens/settings';
import { mount } from '../support/render';
import { createLiveSource } from '@/lib/data/api';
describe('model mode', () => {
  it('selects model mode with Radix keyboard navigation and restores the saved choice', async () => {
    const { source, user } = mount(<Settings tab="models" />);
    const save = vi.spyOn(source.getSnapshot(), 'changeSettings');
    act(() => screen.getByRole('radio', { name: 'Custom' }).focus());
    await user.keyboard('{ArrowRight>}');
    await waitFor(() => expect(save).toHaveBeenCalledWith({ values: { model_mode: 'Auto' } }));
    await user.keyboard('{/ArrowRight}');
    act(() => source.set({ settings: { values: { model_mode: 'Auto' } } }));
    expect(screen.getByRole('radio', { name: 'Auto' })).toBeChecked();
    expect(screen.getByRole('radio', { name: 'Auto' })).toHaveFocus();
    expect(screen.getByRole('radio', { name: 'Custom' })).toHaveAttribute('tabindex', '-1');
  });
  it('switches Custom and Auto and turns vendor fallback off', async () => {
    const { source, user } = mount(<Settings tab="models" />);
    const save = vi.spyOn(source.getSnapshot(), 'changeSettings');
    expect(screen.getByRole('radio', { name: 'Custom' })).toBeChecked();
    await user.click(screen.getByRole('radio', { name: 'Auto' }));
    expect(save).toHaveBeenCalledWith({ values: { model_mode: 'Auto' } });
    act(() => source.set({ settings: { values: { model_mode: 'Auto' } } }));
    expect(screen.getByRole('radio', { name: 'Auto' })).toBeChecked();
    await user.click(screen.getByRole('switch', { name: /vendor is at its limit/ }));
    expect(save).toHaveBeenCalledWith({ values: { vendor_fallback: false } });
  });
});
describe('models', () => {
  it('Settings lists the server model pool including Fake and saves the response_style key', async () => {
    const source = createFixtureSource();
    source.set({
      models: { models: [{ id: 'fake-1', vendor: 'fake', efforts: ['medium'] }] },
      settings: { values: { model_pool: ['fake-1'] } },
    });
    mount(<Settings tab="models" />, source);
    expect(screen.getByRole('switch', { name: 'fake-1' })).toBeChecked();
    expect(screen.queryByText('claude-opus-5-5')).not.toBeInTheDocument();
  });
});
describe('preferences', () => {
  it.each([
    {
      tab: 'preferences',
      label: 'Response style',
      option: 'Technical',
      key: 'response_style',
      value: 'Technical',
    },
    {
      tab: 'appearance',
      label: 'Colour theme',
      option: 'Catppuccin Latte',
      key: 'theme',
      value: 'catppuccin-latte',
    },
  ])('saves $label through changeSettings', async ({ tab, label, option, key, value }) => {
    const source = createFixtureSource();
    source.set({
      settings: { values: { ...source.getSnapshot().settings.values, motion: 'Off' } },
    });
    const changeSettings = vi.spyOn(source.getSnapshot(), 'changeSettings');
    const { user } = mount(<Settings tab={tab} />, source);
    await user.click(screen.getByLabelText(label, { selector: '[role="combobox"]' }));
    await user.click(within(screen.getByRole('listbox')).getByRole('option', { name: option }));
    expect(changeSettings).toHaveBeenCalledWith({ values: { [key]: value } });
    expect(source.fetch).toHaveBeenCalledWith(
      '/api/settings',
      expect.objectContaining({
        method: 'PUT',
        body: JSON.stringify({ values: { [key]: value } }),
      }),
    );
  });
});
describe('text size', () => {
  it.each([
    ['S', '.6'],
    ['M', '.8'],
    ['L', '1'],
    ['XL', '1.25'],
  ])('saves %s and restores its scale after fresh settings hydration', async (size, scale) => {
    const settings: SavedSettings = { values: { text_size: 'L', motion: 'Off' } };
    const seed = createFixtureSource().getSnapshot();
    vi.spyOn(globalThis, 'fetch').mockImplementation(async () => new Response(':root {}'));
    async function start() {
      let events: ReadableStreamDefaultController<Uint8Array>;
      const fetcher = vi.fn<typeof fetch>(async (input, init) => {
        const path = String(input);
        if (path.startsWith('/events/recent'))
          return json({ cursor: 0, events: [], before: null, activity: [] });
        if (path.startsWith('/events?'))
          return new Response(
            new ReadableStream({
              start(c) {
                events = c;
                c.enqueue(new TextEncoder().encode(': live\n\n'));
                init?.signal?.addEventListener('abort', () => c.close(), { once: true });
              },
            }),
          );
        if (path === '/settings' && init?.method === 'PUT') {
          const { values } = JSON.parse(String(init.body));
          Object.assign(settings.values, values);
          events.enqueue(
            new TextEncoder().encode(
              `id: 1\ndata: ${JSON.stringify({ id: 1, at: 1, actor: 'owner', kind: 'settings.changed', payload: { key: 'text_size', old: 'L', new: values.text_size } })}\n\n`,
            ),
          );
        }
        if (path === '/settings') return json(settings);
        if (path === '/version') return json({ version: '0.1.0' });
        if (path === '/status') return json(seed.status);
        if (path === '/update') return json({ current: '0.1.0' });
        if (path === '/models') return json(seed.models);
        if (path === '/logins') return json({});
        if (path === '/projects') return json([]);
        if (['/tasks', '/goals', '/questions', '/hires'].includes(path)) return json([]);
        throw new Error(`Unexpected request ${path}`);
      });
      const source = createLiveSource({ base: '', fetch: fetcher });
      source.start();
      await waitFor(() => expect(source.getSnapshot().connection.state).toBe('connected'));
      return mount(<Settings />, source);
    }
    let app = await start();
    const choices = within(await screen.findByRole('radiogroup', { name: 'Text size' }));
    const choice = choices.getByRole('radio', { name: size });
    await waitFor(() => expect(choice).toBeEnabled());
    await app.user.click(choice);
    await waitFor(() =>
      expect(Number(document.documentElement.style.getPropertyValue('--written-text-scale'))).toBe(
        Number(scale),
      ),
    );
    expect(settings.values.text_size).toBe(size);
    app.unmount();
    app = await start();
    const restoredChoices = within(await screen.findByRole('radiogroup', { name: 'Text size' }));
    await waitFor(() =>
      expect(restoredChoices.getByRole('radio', { name: size })).toHaveAttribute(
        'aria-checked',
        'true',
      ),
    );
    expect(Number(document.documentElement.style.getPropertyValue('--written-text-scale'))).toBe(
      Number(scale),
    );
  });
  it('text size supports arrow-key radio selection', async () => {
    const source = createFixtureSource();
    const save = vi.spyOn(source.getSnapshot(), 'changeSettings');
    const { user } = mount(<Settings />, source);
    const selected = screen.getByRole('radio', { name: 'L' });
    selected.focus();
    await user.keyboard('{ArrowRight>}');
    await waitFor(() => expect(save).toHaveBeenCalledWith({ values: { text_size: 'XL' } }));
    await user.keyboard('{/ArrowRight}');
    act(() => source.set({ settings: { values: { text_size: 'XL' } } }));
    await waitFor(() =>
      expect(screen.getByRole('radio', { name: 'XL' })).toHaveAttribute('aria-checked', 'true'),
    );
    expect(screen.getByRole('radio', { name: 'XL' })).toHaveFocus();
  });
});

describe('run capacity', () => {
  const memoryWarning =
    'Several runs building at once can use this much memory and freeze the desktop.';
  function memorySource(limit: number | null = 8) {
    const source = createFixtureSource();
    source.set({
      settings: { values: { max_runs: 3, runs_memory_gb: limit }, ram_gb: 32, half_ram_gb: 16 },
    });
    return source;
  }
  it('keeps the confirmed memory limit and warning when an older settings refresh completes', async () => {
    const fixture = memorySource();
    fixture.replies['/settings'] = fixture.getSnapshot().settings;
    fixture.replies['PUT /settings'] = {
      values: { max_runs: 4, runs_memory_gb: 20 },
      ram_gb: 32,
      half_ram_gb: 16,
    };
    let events!: ReadableStreamDefaultController<Uint8Array>;
    let hold = false;
    let finishRead: ((response: Response) => void) | undefined;
    const source = createLiveSource({
      fetch: async (input, init) => {
        if (String(input).startsWith('/events?'))
          return new Response(
            new ReadableStream<Uint8Array>({
              start(controller) {
                events = controller;
                controller.enqueue(new TextEncoder().encode(': live\n\n'));
                init?.signal?.addEventListener('abort', () => controller.close(), { once: true });
              },
            }),
          );
        if (input === '/api/settings' && init?.method === 'GET' && hold) {
          hold = false;
          return new Promise<Response>((resolve) => {
            finishRead = resolve;
          });
        }
        return fixture.fetch(input, init);
      },
    });
    const { user, unmount } = mount(<Settings tab="preferences" />, source);
    try {
      await waitFor(() => expect(source.getSnapshot().connection.state).toBe('connected'));
      hold = true;
      act(() => {
        events.enqueue(
          new TextEncoder().encode(
            `id: 1\ndata: ${JSON.stringify({ id: 1, at: 1, actor: 'owner', kind: 'settings.changed', payload: { key: 'max_runs', old: 3, new: 4 } })}\n\n`,
          ),
        );
      });
      await waitFor(() => expect(finishRead).toBeDefined());
      const input = screen.getByRole('spinbutton', { name: 'Memory for all runs (GiB)' });
      await user.clear(input);
      await user.type(input, '20');
      await user.keyboard('{Enter}');
      expect(await screen.findByText(memoryWarning)).toBeInTheDocument();
      expect(source.getSnapshot().settings.values.runs_memory_gb).toBe(20);
      await act(async () => {
        finishRead!(
          json({ values: { max_runs: 4, runs_memory_gb: 8 }, ram_gb: 32, half_ram_gb: 16 }),
        );
        await flush();
      });
      expect(source.getSnapshot().settings.values).toMatchObject({
        max_runs: 4,
        runs_memory_gb: 20,
      });
      expect(screen.getByRole('spinbutton', { name: 'Memory for all runs (GiB)' })).toHaveValue(20);
      expect(screen.getByText(memoryWarning)).toBeInTheDocument();
      fixture.replies['/settings'] = {
        values: { max_runs: 5, runs_memory_gb: 16 },
        ram_gb: 32,
        half_ram_gb: 16,
      };
      act(() => {
        events.enqueue(
          new TextEncoder().encode(
            `id: 2\ndata: ${JSON.stringify({ id: 2, at: 2, actor: 'owner', kind: 'settings.changed', payload: { key: 'max_runs', old: 4, new: 5 } })}\n\n`,
          ),
        );
      });
      await waitFor(() => expect(screen.queryByText(memoryWarning)).not.toBeInTheDocument());
      expect(screen.getByRole('spinbutton', { name: 'Memory for all runs (GiB)' })).toHaveValue(16);
      expect(screen.getByRole('spinbutton', { name: 'Maximum runs' })).toHaveValue(5);
    } finally {
      unmount();
    }
  });
  it('shows the live memory limit beside Maximum runs and saves without needing an event', async () => {
    const source = memorySource();
    const save = vi.spyOn(source.getSnapshot(), 'changeSettings');
    const { user } = mount(<Settings tab="preferences" />, source);
    const input = screen.getByRole('spinbutton', { name: 'Memory for all runs (GiB)' });
    expect(
      within(input.closest('[data-slot="surface"]')!).getByRole('spinbutton', {
        name: 'Maximum runs',
      }),
    ).toBeInTheDocument();
    expect(input).toHaveValue(8);
    expect(input).toHaveAttribute('min', '2');
    expect(input).toHaveAttribute('max', '32');
    expect(input).toHaveAttribute('step', '1');
    fireEvent.blur(input);
    expect(save).not.toHaveBeenCalled();
    source.replies['PUT /settings'] = {
      values: { max_runs: 3, runs_memory_gb: 20 },
      ram_gb: 32,
      half_ram_gb: 16,
    };
    await user.clear(input);
    await user.type(input, '20');
    await user.keyboard('{Enter}');
    await waitFor(() => expect(save).toHaveBeenCalledWith({ values: { runs_memory_gb: 20 } }));
    expect(source.fetch).toHaveBeenCalledWith(
      '/api/settings',
      expect.objectContaining({
        method: 'PUT',
        body: JSON.stringify({ values: { runs_memory_gb: 20 } }),
      }),
    );
    expect(await screen.findByText(memoryWarning)).toBeInTheDocument();
    expect(screen.getByRole('spinbutton', { name: 'Memory for all runs (GiB)' })).toHaveValue(20);
    expect(source.getSnapshot().settings).toMatchObject({ ram_gb: 32, half_ram_gb: 16 });
    source.replies['PUT /settings'] = {
      values: { max_runs: 3, runs_memory_gb: 16 },
      ram_gb: 32,
      half_ram_gb: 16,
    };
    const saved = screen.getByRole('spinbutton', { name: 'Memory for all runs (GiB)' });
    await user.clear(saved);
    await user.type(saved, '16');
    await user.tab();
    await waitFor(() => expect(screen.queryByText(memoryWarning)).not.toBeInTheDocument());
  });
  it.each([8, 16, 17])('warns only above the API threshold for a %i GiB limit', (limit) => {
    const source = memorySource(limit);
    source.set({ settings: { ...source.getSnapshot().settings, ram_gb: 64 } });
    mount(<Settings tab="preferences" />, source);
    expect(screen.getByRole('spinbutton', { name: 'Memory for all runs (GiB)' })).toHaveValue(
      limit,
    );
    const warning = screen.queryByText(memoryWarning);
    if (limit > 16) {
      expect(warning).toBeInTheDocument();
      expect(
        screen.getByRole('spinbutton', { name: 'Memory for all runs (GiB)' }),
      ).toHaveAccessibleDescription(memoryWarning);
    } else expect(warning).not.toBeInTheDocument();
  });
  it('rejects invalid memory edits and disables writes while disconnected', async () => {
    const source = memorySource();
    const save = vi.spyOn(source.getSnapshot(), 'changeSettings');
    mount(<Settings tab="preferences" />, source);
    const input = screen.getByRole('spinbutton', { name: 'Memory for all runs (GiB)' });
    for (const value of ['1', '33', '2.5', '']) {
      fireEvent.change(input, { target: { value } });
      fireEvent.blur(input);
      expect(save).not.toHaveBeenCalled();
      expect(input).toHaveValue(8);
    }
    act(() => source.set({ connection: { state: 'reconnecting' } }));
    await waitFor(() => expect(input).toBeDisabled());
  });
  it('shows an absent limit as unavailable with the missing-unit reason', () => {
    const source = createFixtureSource();
    source.set({ settings: { values: { max_runs: 3 }, ram_gb: 32, half_ram_gb: 16 } });
    const save = vi.spyOn(source.getSnapshot(), 'changeSettings');
    mount(<Settings tab="preferences" />, source);
    const input = screen.getByRole('spinbutton', { name: 'Memory for all runs (GiB)' });
    expect(input).toBeDisabled();
    expect(input).toHaveValue(null);
    expect(input).toHaveAttribute('placeholder', 'Unavailable');
    expect(input).toHaveAccessibleDescription(
      'This engine has no dvb.slice; install the engine units to set this limit.',
    );
    expect(screen.queryByText(memoryWarning)).not.toBeInTheDocument();
    expect(save).not.toHaveBeenCalled();
  });
  it('keeps an installed unlimited limit editable', async () => {
    const source = memorySource(null);
    source.replies['PUT /settings'] = {
      values: { max_runs: 3, runs_memory_gb: 8 },
      ram_gb: 32,
      half_ram_gb: 16,
    };
    const save = vi.spyOn(source.getSnapshot(), 'changeSettings');
    const { user } = mount(<Settings tab="preferences" />, source);
    const input = screen.getByRole('spinbutton', { name: 'Memory for all runs (GiB)' });
    expect(input).toBeEnabled();
    expect(input).toHaveValue(null);
    expect(input).toHaveAttribute('placeholder', 'Unlimited');
    await user.type(input, '8');
    await user.tab();
    await waitFor(() => expect(save).toHaveBeenCalledWith({ values: { runs_memory_gb: 8 } }));
    expect(screen.getByRole('spinbutton', { name: 'Memory for all runs (GiB)' })).toHaveValue(8);
  });
  it('keeps the memory draft and last confirmed value when saving fails', async () => {
    const source = memorySource();
    source.replies['PUT /settings'] = {
      code: 'INVALID_VALUE',
      detail: 'Unable to set the run memory limit.',
    };
    source.statuses['PUT /settings'] = 409;
    const { user } = mount(<Settings tab="preferences" />, source);
    const input = screen.getByRole('spinbutton', { name: 'Memory for all runs (GiB)' });
    await user.clear(input);
    await user.type(input, '20');
    await user.tab();
    expect(await screen.findByRole('alert')).toHaveTextContent(
      'Unable to set the run memory limit.',
    );
    expect(input).toHaveValue(20);
    expect(source.getSnapshot().settings.values.runs_memory_gb).toBe(8);
  });
  it('refuses invalid edits before saving and disables writes while disconnected', async () => {
    const source = createFixtureSource();
    const write = vi.fn(source.getSnapshot().changeSettings);
    source.set({ changeSettings: write });
    mount(<Settings tab="preferences" />, source);
    const input = screen.getByRole('spinbutton', { name: 'Maximum runs' });
    for (const value of ['0', '1.5', '']) {
      fireEvent.change(input, { target: { value } });
      fireEvent.blur(input);
      expect(write).not.toHaveBeenCalled();
      expect(input).toHaveValue(3);
    }
    act(() => source.set({ mode: 'live', connection: { state: 'reconnecting' } }));
    await waitFor(() => expect(input).toBeDisabled());
  });
  it('shows the default, saves a whole number, and restores the saved control', async () => {
    const { source, user, unmount } = mount(<Settings tab="preferences" />);
    const save = vi.spyOn(source.getSnapshot(), 'changeSettings');
    const input = screen.getByRole('spinbutton', { name: 'Maximum runs' });
    expect(input).toHaveValue(3);
    expect(input).toHaveAttribute('min', '1');
    expect(input).toHaveAttribute('step', '1');
    expect(input).not.toHaveAttribute('max');
    expect(screen.getByText(/Each run may use up to 8 GB/)).toHaveTextContent(
      /memory, disk and load holds still apply/,
    );
    await user.clear(input);
    await user.type(input, '6');
    await user.tab();
    await waitFor(() => expect(save).toHaveBeenCalledWith({ values: { max_runs: 6 } }));
    expect(source.fetch).toHaveBeenCalledWith(
      '/api/settings',
      expect.objectContaining({ method: 'PUT', body: JSON.stringify({ values: { max_runs: 6 } }) }),
    );
    act(() => source.set({ settings: { values: { max_runs: 6 } } }));
    expect(screen.getByRole('spinbutton', { name: 'Maximum runs' })).toHaveValue(6);
    const saved = structuredClone(source.getSnapshot().settings);
    unmount();
    const restored = createFixtureSource();
    restored.set({ settings: saved });
    mount(<Settings tab="preferences" />, restored);
    expect(screen.getByRole('spinbutton', { name: 'Maximum runs' })).toHaveValue(6);
  });
});

describe('commit author', () => {
  const effect =
    'Commits will show Example Owner <owner@example.invalid> and count on that GitHub account.';
  it('publishes as the owner once they turn the toggle on and enter their identity', async () => {
    const { source, user } = mount(<Settings tab="preferences" />);
    expect(await screen.findByText('Commits show dvb <dvb@dvb.invalid>.')).toBeInTheDocument();
    const toggle = screen.getByRole('switch', { name: 'Publish as you' });
    expect(toggle).not.toBeChecked();
    expect(screen.queryByRole('textbox', { name: 'Your name' })).not.toBeInTheDocument();
    await user.click(toggle);
    expect(screen.queryByText(effect)).not.toBeInTheDocument();
    await user.type(screen.getByRole('textbox', { name: 'Your name' }), 'Example Owner');
    await user.type(screen.getByRole('textbox', { name: 'Your email' }), 'owner@example.invalid');
    expect(screen.getByText(effect)).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Save' }));
    expect(
      await screen.findByText('Commits show Example Owner <owner@example.invalid>.'),
    ).toBeInTheDocument();
    expect(source.fetch).toHaveBeenCalledWith(
      '/api/settings/publication',
      expect.objectContaining({
        method: 'PUT',
        body: JSON.stringify({
          name: 'Example Owner',
          email: 'owner@example.invalid',
          as_owner: true,
        }),
      }),
    );
    expect(screen.getByRole('switch', { name: 'Publish as you' })).toBeChecked();
    expect(screen.getByText(effect)).toBeInTheDocument();
  });
  it("goes back to the engine's default identity when the toggle is turned off", async () => {
    const source = createFixtureSource();
    source.replies['/settings/publication'] = source.replies['PUT /settings/publication'];
    source.replies['PUT /settings/publication'] = {
      name: 'dvb',
      email: 'dvb@dvb.invalid',
      is_default: true,
      as_owner: false,
    };
    const { user } = mount(<Settings tab="preferences" />, source);
    expect(await screen.findByText(effect)).toBeInTheDocument();
    await user.click(screen.getByRole('switch', { name: 'Publish as you' }));
    expect(await screen.findByText('Commits show dvb <dvb@dvb.invalid>.')).toBeInTheDocument();
    expect(source.fetch).toHaveBeenCalledWith(
      '/api/settings/publication',
      expect.objectContaining({
        method: 'PUT',
        body: JSON.stringify({ name: '', email: '', as_owner: false }),
      }),
    );
    expect(screen.getByRole('switch', { name: 'Publish as you' })).not.toBeChecked();
    expect(screen.queryByText(effect)).not.toBeInTheDocument();
  });
  it("shows the engine's refusal as it is next to the fields", async () => {
    const source = createFixtureSource();
    const detail =
      "The commit author name can't contain a secret: dvb never publishes secrets. Use your name.";
    source.replies['PUT /settings/publication'] = { code: 'INVALID_VALUE', detail };
    source.statuses['PUT /settings/publication'] = 409;
    const { user } = mount(<Settings tab="preferences" />, source);
    await user.click(await screen.findByRole('switch', { name: 'Publish as you' }));
    const name = screen.getByRole('textbox', { name: 'Your name' });
    await user.type(name, 'ghp_secret');
    await user.type(screen.getByRole('textbox', { name: 'Your email' }), 'owner@example.invalid');
    await user.click(screen.getByRole('button', { name: 'Save' }));
    const refusal = await screen.findByRole('alert');
    expect(refusal.textContent).toBe(detail);
    expect(name).toHaveAttribute('aria-invalid', 'true');
    expect(name).toHaveAccessibleDescription(detail);
    expect(name).toHaveValue('ghp_secret');
    expect(screen.getByText('Commits show dvb <dvb@dvb.invalid>.')).toBeInTheDocument();
  });
});

describe('updates', () => {
  it.each([
    { info: { current: '0.1.0' }, result: 'Up to date' },
    { info: { current: '0.1.0', available: '0.1.1' }, result: 'Available: 0.1.1' },
  ])('announces checking and then $result in the same polite region', async ({ info, result }) => {
    const source = createFixtureSource();
    let answer!: (response: Response) => void;
    source.fetch.mockImplementationOnce(
      (input) =>
        new Promise((resolve) => {
          expect(String(input)).toBe('/api/update?check=true');
          answer = resolve;
        }),
    );
    mount(<Settings tab="updates" />, source);
    const status = screen.getByRole('status');
    expect(status).toHaveAttribute('aria-live', 'polite');
    expect(status).toHaveAttribute('aria-atomic', 'true');
    expect(status).toHaveTextContent('Checking for a newer release…');
    expect(screen.queryByText('Up to date')).not.toBeInTheDocument();
    act(() => answer(json(info)));
    expect(await screen.findByText(result)).toBeInTheDocument();
    expect(screen.getByRole('status')).toBe(status);
    expect(status).toHaveTextContent(result);
    expect(screen.queryByText('Checking for a newer release…')).not.toBeInTheDocument();
  });
  it('names the newer release the check found and starts the update', async () => {
    const source = createFixtureSource();
    source.replies['/update'] = { current: '0.1.0', available: '0.1.1' };
    source.replies['POST /update'] = {
      current: '0.1.0',
      available: '0.1.1',
      updating: { to: '0.1.1', runs_active: 0 },
    };
    const { user } = mount(<Settings tab="updates" />, source);
    const status = screen.getByRole('status');
    expect(await screen.findByText('Available: 0.1.1')).toBeInTheDocument();
    expect(screen.getByRole('status')).toBe(status);
    expect(status).toHaveTextContent('Available: 0.1.1');
    await user.click(screen.getByRole('button', { name: 'Update dvb' }));
    expect(await screen.findByText('Updating to 0.1.1')).toBeInTheDocument();
    expect(screen.getByRole('status')).toBe(status);
    expect(status).toHaveTextContent('Updating to 0.1.1');
    expect(screen.getByRole('button', { name: 'Update dvb' })).toBeDisabled();
  });
  it('shows an update under way from the check answer', async () => {
    const source = createFixtureSource();
    source.replies['/update'] = {
      current: '0.1.0',
      available: '0.1.1',
      updating: { to: '0.1.1', runs_active: 2 },
    };
    mount(<Settings tab="updates" />, source);
    const status = screen.getByRole('status');
    expect(await screen.findByText('Updating to 0.1.1')).toBeInTheDocument();
    expect(screen.getByRole('status')).toBe(status);
    expect(status).toHaveTextContent('Updating to 0.1.1');
    expect(screen.getByRole('button', { name: 'Update dvb' })).toBeDisabled();
  });
  it('shows why the check failed and never says up to date', async () => {
    const source = createFixtureSource();
    source.replies['/update'] = {
      code: 'UPDATE_REFUSED',
      detail: 'the release list could not be read',
    };
    source.statuses['/update'] = 502;
    mount(<Settings tab="updates" />, source);
    const status = screen.getByRole('status');
    expect(await screen.findByRole('alert')).toHaveTextContent(
      'The update check failed: the release list could not be read',
    );
    expect(screen.queryByText('Up to date')).not.toBeInTheDocument();
    expect(screen.getByRole('status')).toBe(status);
    expect(status).toBeEmptyDOMElement();
  });
});
