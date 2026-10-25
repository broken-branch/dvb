import { mount } from '../support/render';
import { describe, it, expect, vi } from 'vitest';
import { screen, within, waitFor } from '@testing-library/react';
import { createFixtureSource } from '../support/render';
import { createLiveSource } from '@/lib/data/api';
import { Company } from '@/screens/company';
import { Settings } from '@/screens/settings';
describe('history and restore', () => {
  it('refocuses its History button when the sheet closes', async () => {
    const source = createFixtureSource();
    const { user } = mount(<Settings tab="preferences" />, source);
    const opener = screen.getByRole('button', { name: 'History of Response style' });
    await user.click(opener);
    await screen.findByLabelText('History');
    // The padding and width rules in design-system.css target this slot and side.
    const sheet = screen.getByRole('dialog', { name: 'History: Response style' });
    expect(sheet).toHaveAttribute('data-slot', 'sheet-content');
    expect(sheet).toHaveAttribute('data-side', 'right');
    await user.keyboard('{Escape}');
    await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument());
    await waitFor(() => expect(opener).toHaveFocus());
  });
  it('restores an earlier role value from the role card', async () => {
    const source = createFixtureSource();
    source.set({
      settings: { values: { ...source.getSnapshot().settings.values, motion: 'Off' } },
    });
    const role = source.getSnapshot().roles.harbour[2];
    source.replies[`POST /roles/${role.id}/restore`] = {
      ...role,
      job: 'Build screens and their tests',
    };
    const restore = vi.spyOn(source.getSnapshot(), 'restoreRole');
    const { user } = mount(<Company projectId="harbour" />, source);
    await user.click(within(screen.getByRole('tablist')).getByRole('tab', { name: 'Cards' }));
    await user.click(screen.getByLabelText('History of Frontend builder', { selector: 'button' }));
    const list = await screen.findByLabelText('History');
    expect(await within(list).findByText('Current')).toBeInTheDocument();
    await user.click((await within(list).findAllByRole('button', { name: /^Restore/ }))[0]);
    await screen.findByText('Restored.');
    expect(restore).toHaveBeenCalledWith(role.id, { event: 104, current: 105 });
  });
  it('sends the event and current value, and reloads with a message on VALUE_CHANGED', async () => {
    const source = createFixtureSource();
    const restore = vi.spyOn(source.getSnapshot(), 'restoreSettings');
    source.replies['POST /settings/restore'] = { code: 'VALUE_CHANGED', detail: 'Changed' };
    source.statuses['POST /settings/restore'] = 409;
    const { user } = mount(<Settings tab="preferences" />, source);
    await user.click(screen.getByRole('button', { name: 'History of Response style' }));
    const list = await screen.findByLabelText('History');
    source.replies['/settings/history'] = {
      key: 'response_style',
      versions: [
        { event: 200, at: 200, actor: 'owner', value: 'Expert' },
        { event: 109, at: 109, actor: 'owner', value: 'Clear' },
      ],
    };
    await user.click((await within(list).findAllByRole('button', { name: /^Restore/ }))[0]);
    await screen.findByText(/This changed while you were looking/);
    expect(restore).toHaveBeenCalledWith({ key: 'response_style', event: 108, current: 109 });
    await waitFor(() =>
      expect(within(screen.getByLabelText('History')).getByText('Expert')).toBeInTheDocument(),
    );
    source.replies['POST /settings/restore'] = { values: { response_style: 'Clear' } };
    source.statuses['POST /settings/restore'] = 200;
    await user.click(
      within(screen.getByLabelText('History')).getAllByRole('button', { name: /^Restore/ })[0],
    );
    await screen.findByText('Restored.');
    expect(restore).toHaveBeenLastCalledWith({ key: 'response_style', event: 109, current: 200 });
  });
  it('restores a hire from its row in the role profile', async () => {
    const source = createFixtureSource();
    source.set({
      settings: { values: { ...source.getSnapshot().settings.values, motion: 'Off' } },
    });
    const hire = source.getSnapshot().hires[2];
    source.replies[`POST /hires/${hire.id}/restore`] = {
      ...hire,
      model: 'claude-opus-5-5',
      effort: 'medium',
    };
    const restore = vi.spyOn(source.getSnapshot(), 'restoreHire');
    const { user } = mount(<Company profileId="harbour-frontend" projectId="harbour" />, source);
    await user.click(
      await screen.findByLabelText(/^History of Frontend builder: /, { selector: 'button' }),
    );
    await user.click(
      (
        await within(await screen.findByLabelText('History')).findAllByRole('button', {
          name: /^Restore/,
        })
      )[0],
    );
    await screen.findByText('Restored.');
    expect(restore).toHaveBeenCalledWith(hire.id, { event: 106, current: 107 });
  });
  it('posts restore bodies as {event, current} to the history routes', async () => {
    const calls: { url: string; method: string; body?: unknown }[] = [];
    const fetch = (async (url: string, init?: RequestInit) => {
      calls.push({
        url,
        method: init?.method || 'GET',
        body: init?.body ? JSON.parse(String(init.body)) : undefined,
      });
      return new Response(JSON.stringify(url.includes('history') ? { versions: [] } : {}), {
        status: 200,
        headers: { 'dvb-version': '0.1.0', 'content-type': 'application/json' },
      });
    }) as typeof globalThis.fetch;
    const live = createLiveSource({ fetch });
    const s = live.getSnapshot(),
      connected = () => live.set({ connection: { state: 'connected', engineVersion: '0.1.0' } });
    await s.roleHistory('r1');
    await s.hireHistory('h1');
    await s.settingsHistory('model_mode');
    connected();
    await s.restoreRole('r1', { event: 3, current: 5 });
    connected();
    await s.restoreHire('h1', { event: 3, current: 5 });
    connected();
    await s.restoreSettings({ key: 'model_mode', event: 3, current: 5 });
    expect(calls.map((c) => `${c.method} ${c.url}`)).toEqual([
      'GET /api/roles/r1/history',
      'GET /api/hires/h1/history',
      'GET /api/settings/history?key=model_mode',
      'POST /api/roles/r1/restore',
      'POST /api/hires/h1/restore',
      'POST /api/settings/restore',
    ]);
    expect(calls[3].body).toEqual({ event: 3, current: 5 });
    expect(calls[5].body).toEqual({ key: 'model_mode', event: 3, current: 5 });
  });
});
