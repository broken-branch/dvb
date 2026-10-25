import { project, event, page } from '../support/rows';
import type { components } from '@/api/schema';
import { fireEvent } from '@testing-library/react';
import { OrgChart } from '@/components/dvb/org-chart';
import type { DataStore } from '@/lib/data/types';
import { mount } from '../support/render';
import { it, expect, vi, describe } from 'vitest';
import { screen, act, waitFor } from '@testing-library/react';
import { createFixtureSource } from '../support/render';
import { Company } from '@/screens/company';
import { createSource } from '@/lib/data/source';
describe('roles and hires', () => {
  it('saves review assignment without changing reporting', async () => {
    const source = createFixtureSource();
    const role = source.getSnapshot().roles.harbour[2];
    source.replies[`PATCH /roles/${role.id}`] = { ...role, reviewed_by: 'harbour-design-reviewer' };
    const save = vi.spyOn(source.getSnapshot(), 'changeRole');
    const { user } = mount(<Company profileId={role.id} projectId="harbour" />, source);
    await user.click(screen.getByRole('combobox', { name: 'Reviewed by' }));
    await user.click(screen.getByRole('option', { name: 'Design reviewer' }));
    await user.click(screen.getByRole('button', { name: 'Save role' }));
    expect(save).toHaveBeenCalledWith(role.id, { reviewed_by: 'harbour-design-reviewer' });
  });
  it('saves reporting independently of review assignment', async () => {
    const source = createFixtureSource();
    const role = {
      ...source.getSnapshot().roles.harbour[2],
      reviewed_by: 'harbour-design-reviewer',
    };
    source.set({
      roles: {
        harbour: [
          source.getSnapshot().roles.harbour[0],
          source.getSnapshot().roles.harbour[1],
          role,
          ...source.getSnapshot().roles.harbour.slice(3),
        ],
      },
    });
    source.replies[`PATCH /roles/${role.id}`] = { ...role, reports_to: 'harbour-architect' };
    const save = vi.spyOn(source.getSnapshot(), 'changeRole');
    const { user } = mount(<Company profileId={role.id} projectId="harbour" />, source);
    await user.click(screen.getByRole('combobox', { name: 'Reports to' }));
    await user.click(screen.getByRole('option', { name: 'Architect' }));
    await user.click(screen.getByRole('button', { name: 'Save role' }));
    expect(save).toHaveBeenCalledWith(role.id, { reports_to: 'harbour-architect' });
  });
  it('Company hires and retires through the data layer', async () => {
    const source = createFixtureSource();
    const role = source.getSnapshot().roles.harbour[2];
    const hire = source.getSnapshot().hires.find((h) => h.role === role.id)!;
    source.replies['POST /hires'] = { ...hire, id: 'new-hire', retired: [] };
    source.replies[`POST /hires/${hire.id}/retire`] = { ...hire, status: 'retired' };
    const add = vi.spyOn(source.getSnapshot(), 'hire');
    const retire = vi.spyOn(source.getSnapshot(), 'retire');
    const { user } = mount(<Company profileId={role.id} projectId="harbour" />, source);
    await user.click(screen.getByRole('button', { name: 'Hire another model' }));
    expect(add).toHaveBeenCalledWith({ role: role.id, model: hire.model, effort: hire.effort });
    await user.click(screen.getAllByRole('button', { name: /^Retire / })[0]);
    expect(retire).toHaveBeenCalledWith(hire.id);
  });
  it('uses a selected hire for new work and retires the previous selection', async () => {
    const source = createFixtureSource();
    const role = source.getSnapshot().roles.harbour[2];
    const hire = source.getSnapshot().hires.find((h) => h.role === role.id)!;
    source.replies['POST /hires'] = { ...hire, id: 'new-hire', retired: [] };
    source.replies[`POST /hires/${hire.id}/retire`] = { ...hire, status: 'retired' };
    const retire = vi.spyOn(source.getSnapshot(), 'retire');
    const { user } = mount(<Company profileId={role.id} projectId="harbour" />, source);
    await user.click(screen.getByRole('button', { name: 'Use for new work' }));
    await waitFor(() => expect(retire).toHaveBeenCalledWith(hire.id));
  });
});
describe('scoped activity', () => {
  it('reads role activity outside the global recent window and refreshes role usage', async () => {
    const role = {
      id: 'frontend',
      slug: 'frontend',
      name: 'Frontend builder',
      job: 'Build',
      lane: 'UI',
      lead: false,
      reports_to: null,
      reviewed_by: null,
      review_instructions: '',
    };
    const read = vi.fn<DataStore['eventPage']>().mockResolvedValue({
      ok: true,
      value: page([
        event(1, 'run.started', { run: 'old', role: role.id, reason: 'Older role work' }),
        event(2, 'run.output', { run: 'old', item: { text: 'Older role activity' } }),
      ]),
    });
    const usage = vi
      .fn<DataStore['usage']>()
      .mockResolvedValue({ ok: true, value: { rows: [], windows: [] } });
    const s = createSource();
    s.set({
      projects: [project],
      roles: { p: [role] },
      eventPage: read,
      usage,
      events: [event(100, 'project.paused', {})],
    });
    mount(<Company projectId="p" profileId={role.id} />, s);
    expect(await screen.findByText('Older role activity')).toBeInTheDocument();
    expect(read).toHaveBeenCalledWith({ project: 'p', role: role.id, limit: 12 });
    expect(s.getSnapshot().events).toHaveLength(1);
    read.mockResolvedValueOnce({
      ok: true,
      value: page([
        event(101, 'run.output', { run: 'old', item: { text: 'Reconnected role activity' } }),
      ]),
    });
    await act(async () => s.set({ connection: { state: 'connected' } }));
    expect(await screen.findByText('Reconnected role activity')).toBeInTheDocument();
    expect(read).toHaveBeenCalledTimes(2);
    await waitFor(() => expect(usage).toHaveBeenCalledTimes(1));
    await act(async () => s.set({ usageVersion: 101 }));
    await waitFor(() => expect(usage).toHaveBeenCalledTimes(2));
  });
});

it('saves only edited role fields after a concurrent update', async () => {
  const source = createFixtureSource();
  source.replies['PATCH /roles/harbour-frontend'] = {
    ...source.getSnapshot().roles.harbour[2],
    name: 'Interface builder',
    job: 'Updated job',
  } satisfies components['schemas']['RoleRecord'];
  const save = vi.spyOn(source.getSnapshot(), 'changeRole');
  const { user } = mount(<Company projectId="harbour" profileId="harbour-frontend" />, source);
  fireEvent.change(screen.getByRole('textbox', { name: 'Role name' }), {
    target: { value: 'Interface builder' },
  });
  act(() =>
    source.set({
      roles: {
        ...source.getSnapshot().roles,
        harbour: source
          .getSnapshot()
          .roles.harbour.map((r) =>
            r.id === 'harbour-frontend' ? { ...r, job: 'Updated job' } : r,
          ),
      },
    }),
  );
  await user.click(screen.getByRole('button', { name: 'Save role' }));
  expect(save).toHaveBeenCalledWith('harbour-frontend', { name: 'Interface builder' });
});

it('opens the root role profile with Enter', async () => {
  const { user } = mount(<OrgChart project="harbour" />);
  const root = screen.getByRole('button', { name: 'Open Orchestrator profile' });
  root.focus();
  await user.keyboard('{Enter}');
  expect(location.hash).toBe('#company/project/harbour/role/harbour-orchestrator');
});
