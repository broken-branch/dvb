import type { Route } from '@/lib/routes';
import { afterEach, expect, it, vi, describe } from 'vitest';
import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import userEvent, { PointerEventsCheckLevel } from '@testing-library/user-event';
import App from '@/App';
import { createFixtureSource } from '../support/render';
import { buildRoute, parseRoute, routes } from '@/lib/routes';
describe('company history', () => {
  afterEach(() => {
    history.replaceState({}, '', '/');
  });
  it('keeps Company on the profile project after switching, Back and closing the profile', async () => {
    const source = createFixtureSource(),
      role = source.getSnapshot().roles.fieldnotes[0];
    source.set({
      projects: source
        .getSnapshot()
        .projects.filter((p) => ['harbour', 'fieldnotes'].includes(p.id)),
      roles: { harbour: [], fieldnotes: [role] },
      hires: [],
      goals: [],
      tasks: [],
      questions: [],
      events: [],
      activity: {},
      settings: { values: { motion: 'Off' } },
    });
    history.replaceState({}, '', `/#${routes.company(role.id, 'fieldnotes')}`);
    render(<App source={source} />);
    const company = within(document.querySelector('.page') as HTMLElement);
    fireEvent.click(company.getByLabelText('Company project'));
    fireEvent.click(
      within(await screen.findByRole('listbox')).getByRole('option', { name: /Harbour/ }),
    );
    await waitFor(() => expect(location.hash).toBe('#company/project/harbour'));
    expect(company.getByLabelText('Company project')).toHaveTextContent('Harbour');
    act(() => history.back());
    const dialog = await screen.findByRole('dialog');
    expect(dialog).toBeInTheDocument();
    const user = userEvent.setup({ pointerEventsCheck: PointerEventsCheckLevel.Never });
    await user.click(within(dialog).getByLabelText('Close'));
    await waitFor(() => expect(location.hash).toBe('#company/project/fieldnotes'));
    expect(company.getByLabelText('Company project')).toHaveTextContent('Dispatch');
  });
  it('adds a role to the project restored by Back after switching projects', async () => {
    const source = createFixtureSource();
    source.set({
      projects: source
        .getSnapshot()
        .projects.filter((p) => ['harbour', 'fieldnotes'].includes(p.id)),
      roles: { harbour: [], fieldnotes: [] },
      hires: [],
      tasks: [],
      questions: [],
      settings: { values: { motion: 'Off' } },
    });
    const addRole = vi.spyOn(source.getSnapshot(), 'addRole');
    history.replaceState({}, '', '/#company/project/fieldnotes');
    render(<App source={source} />);
    const company = within(document.querySelector('.page') as HTMLElement),
      user = userEvent.setup({ pointerEventsCheck: PointerEventsCheckLevel.Never });
    await user.click(company.getByLabelText('Company project'));
    await user.click(screen.getByRole('option', { name: /Harbour/ }));
    await waitFor(() => expect(location.hash).toBe('#company/project/harbour'));
    act(() => history.back());
    await waitFor(() => expect(location.hash).toBe('#company/project/fieldnotes'));
    await user.click(company.getByRole('button', { name: 'Add a role' }));
    const form = within(screen.getByRole('dialog'));
    await user.click(form.getByLabelText('Role name'));
    await user.paste('New role');
    await user.click(form.getByRole('button', { name: 'Add role' }));
    expect(addRole).toHaveBeenCalledWith(
      'fieldnotes',
      expect.objectContaining({ name: 'New role' }),
    );
  });
});
describe('routes', () => {
  const values: Route[] = [
    { page: 'home' },
    { page: 'projects' },
    { page: 'usage' },
    { page: 'setup' },
    { page: 'project', id: 'new' },
    { page: 'project', id: 'project/a', tab: 'activity' },
    { page: 'task', id: 'a b', tab: 'review' },
    { page: 'decision', id: 'q/#' },
    { page: 'decision', id: 'q', from: { page: 'project', id: 'p/b' } },
    { page: 'decision', id: 'q', from: { page: 'task', id: 't' } },
    { page: 'decision' },
    { page: 'settings', tab: 'accessibility' },
    { page: 'settings' },
    { page: 'company', profile: 'role/a', project: 'p b' },
    { page: 'company', project: 'p' },
    { page: 'company', profile: 'r' },
    { page: 'company' },
  ];
  it.each(values)('round trips $page with encoded segments', (route) => {
    expect(parseRoute(`#${buildRoute(route)}`)).toEqual(route);
  });
  it('parses explicit Company destinations and invalid routes', () => {
    expect(routes.company(undefined, 'p')).toBe('company/project/p');
    expect(routes.company('r', 'p')).toBe('company/project/p/role/r');
    expect(parseRoute('nonsense')).toEqual({ page: 'home' });
    expect(parseRoute('task/%invalid/review')).toEqual({
      page: 'task',
      id: '%invalid',
      tab: 'review',
    });
  });
});
describe('screen tabs', () => {
  it('Company without a project selects the first, and project and Settings tabs follow the address', async () => {
    const source = createFixtureSource(),
      seed = source.getSnapshot();
    source.set({
      projects: [{ ...seed.projects[1], id: 'first', name: 'First' }],
      roles: { first: seed.roles.fieldnotes },
      goals: [],
      tasks: [],
      questions: [],
      settings: { values: { motion: 'Off' } },
    });
    history.replaceState({}, '', '/#company');
    render(<App source={source} />);
    expect(screen.getByLabelText('Company project')).toHaveTextContent('First');
    for (const route of [
      'project/first',
      'project/first/activity',
      'settings/appearance',
      'settings/updates',
    ]) {
      act(() => {
        history.replaceState({}, '', `/#${route}`);
        window.dispatchEvent(new HashChangeEvent('hashchange'));
      });
      const tab = route.split('/').at(-1) === 'first' ? 'Board' : route.split('/').at(-1)!;
      await waitFor(() =>
        expect(
          within(document.querySelector<HTMLElement>('.page-tabs')!).getByRole('tab', {
            name: new RegExp(`^${tab}$`, 'i'),
          }),
        ).toHaveAttribute('aria-selected', 'true'),
      );
    }
    history.replaceState({}, '', '/');
  });
});
describe('task history', () => {
  afterEach(() => {
    history.replaceState({}, '', '/');
  });
  it('follows Back and Forward between tabs within the same task', async () => {
    const source = createFixtureSource();
    source.set({ settings: { values: { motion: 'Off' } } });
    history.replaceState({}, '', '/#task/resume/changes');
    render(<App source={source} />);
    const user = userEvent.setup({ pointerEventsCheck: PointerEventsCheckLevel.Never });
    const tabs = within(screen.getByRole('tablist'));
    await user.click(tabs.getByRole('tab', { name: 'Review' }));
    await waitFor(() => expect(location.hash).toBe('#task/resume/review'));
    act(() => history.back());
    await waitFor(() => expect(location.hash).toBe('#task/resume/changes'));
    await waitFor(() =>
      expect(tabs.getByRole('tab', { name: 'Changes' })).toHaveAttribute('aria-selected', 'true'),
    );
    act(() => history.forward());
    await waitFor(() => expect(location.hash).toBe('#task/resume/review'));
    await waitFor(() =>
      expect(tabs.getByRole('tab', { name: 'Review' })).toHaveAttribute('aria-selected', 'true'),
    );
  });
});
