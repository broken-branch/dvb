import type { Question } from '@/lib/data/types';
import { beforeEach } from 'vitest';
import { railGroups, needs } from '@/lib/data/work';
import { expect, it, describe, vi } from 'vitest';
import { screen, within, render, waitFor, act, fireEvent } from '@testing-library/react';
import App from '@/App';
import { createFixtureSource } from '../support/render';
import { mount } from '../support/render';
import userEvent, { PointerEventsCheckLevel } from '@testing-library/user-event';
beforeEach(() => vi.spyOn(Date, 'now').mockReturnValue(1790811300000));
describe('keyboard guide', () => {
  it('uses keycaps for every keyboard guide row, including the G + 1–5 sequence', async () => {
    history.replaceState({}, '', '/#home');
    const source = createFixtureSource(),
      { user } = mount(<App source={source} />, source);
    await user.click(screen.getByRole('button', { name: 'Keyboard guide' }));
    const guide = await screen.findByRole('dialog');
    const rows = guide.querySelectorAll('.keyboard-list > div');
    expect(
      [...rows].map((row) => [...row.querySelectorAll('dt kbd')].map((key) => key.textContent)),
    ).toEqual([
      ['J', 'K'],
      ['Enter', 'O'],
      ['A'],
      ['1–9'],
      ['Ctrl', 'Enter'],
      ['P'],
      ['X'],
      ['G', '1–5'],
      ['Ctrl', 'K', '/'],
      ['['],
      ['?'],
      ['Esc'],
    ]);
    const sequenceRow = within(guide).getByText(
      'Now, Overview, Usage & models, Company, Settings',
    ).parentElement!;
    expect(within(sequenceRow).getByText('+')).toHaveClass('keycap-join');
    expect(within(sequenceRow).queryByText(/then/)).not.toBeInTheDocument();
  });
  it('opens with no tooltip, closes with one Escape and refocuses the rail button', async () => {
    history.replaceState({}, '', '/#home');
    const source = createFixtureSource(),
      { user } = mount(<App source={source} />, source);
    // The page focuses its heading shortly after it opens.
    await waitFor(() => expect(screen.getByRole('heading', { level: 1 })).toHaveFocus());
    const opener = screen.getByRole('button', { name: 'Keyboard guide' });
    opener.focus();
    await user.keyboard('{Escape}');
    await user.keyboard('{Enter}');
    const guide = await screen.findByRole('dialog');
    expect(guide).toHaveFocus();
    expect(screen.queryByRole('tooltip')).not.toBeInTheDocument();
    await user.keyboard('{Escape}');
    await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument());
    await waitFor(() => expect(opener).toHaveFocus());
  });
  it('keeps the info button reachable by Tab', async () => {
    history.replaceState({}, '', '/#home');
    const source = createFixtureSource(),
      { user } = mount(<App source={source} />, source);
    await user.keyboard('?');
    const guide = await screen.findByRole('dialog');
    await user.tab();
    expect(within(guide).getByRole('button', { name: 'About this dialog' })).toHaveFocus();
  });
  it('refocuses the element focused before Go anywhere opened the guide', async () => {
    history.replaceState({}, '', '/#home');
    const source = createFixtureSource(),
      { user } = mount(<App source={source} />, source);
    // The page focuses its heading shortly after it opens.
    await waitFor(() => expect(screen.getByRole('heading', { level: 1 })).toHaveFocus());
    const opener = screen.getByRole('button', { name: 'Collapse navigation' });
    opener.focus();
    await user.keyboard('{Control>}k{/Control}');
    const command = await screen.findByRole('dialog', { name: 'Go anywhere' });
    await user.click(within(command).getByRole('option', { name: /Keyboard guide/ }));
    const guide = await screen.findByRole('dialog', { name: /keyboard/ });
    await waitFor(() => expect(guide).toHaveFocus());
    await user.click(within(guide).getByRole('button', { name: 'Close', exact: true }));
    await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument());
    await waitFor(() => expect(opener).toHaveFocus());
  });
});
describe('navigation', () => {
  function mountApp(width = 1024) {
    vi.spyOn(window, 'innerWidth', 'get').mockReturnValue(width);
    history.replaceState({}, '', '/#home');
    const source = createFixtureSource();
    source.set({
      settings: { values: { ...source.getSnapshot().settings.values, motion: 'Off' } },
    });
    const view = render(<App source={source} />);
    return {
      ...view,
      user: userEvent.setup({ pointerEventsCheck: PointerEventsCheckLevel.Never }),
    };
  }
  describe('cockpit navigation primitives', () => {
    it('collapses and expands desktop navigation with the button and shortcut', async () => {
      const { user } = mountApp();
      const sidebar = document.querySelector('[data-slot=sidebar]');
      expect(sidebar).toHaveAttribute('data-state', 'expanded');
      await user.click(screen.getByRole('button', { name: 'Collapse navigation' }));
      expect(sidebar).toHaveAttribute('data-state', 'collapsed');
      expect(sidebar).toHaveAttribute('data-collapsible', 'icon');
      await user.keyboard('[[');
      expect(sidebar).toHaveAttribute('data-state', 'expanded');
      expect(sidebar).toHaveAttribute('data-side', 'left');
      expect(sidebar).toHaveAttribute('data-variant', 'sidebar');
    });
    it('shows only the shortcut in expanded nav hints and the label too when collapsed', async () => {
      const { user } = mountApp();
      const nav = screen.getByRole('navigation', { name: 'Main navigation' });
      const usage = within(nav).getByRole('link', { name: /Usage & models/ });
      async function hintFor() {
        await user.hover(usage);
        const tip = await screen.findByRole('tooltip');
        const keys = [...tip.querySelectorAll('kbd')].map((key) => key.textContent);
        const text = tip.textContent;
        await user.unhover(usage);
        await user.keyboard('{Escape}');
        await waitFor(() => expect(screen.queryByRole('tooltip')).not.toBeInTheDocument());
        return { keys, text };
      }
      const expanded = await hintFor();
      expect(expanded.keys).toEqual(['G', '3']);
      expect(expanded.text).not.toContain('Usage & models');
      await user.click(screen.getByRole('button', { name: 'Collapse navigation' }));
      const collapsed = await hintFor();
      expect(collapsed.keys).toEqual(['G', '3']);
      expect(collapsed.text).toContain('Usage & models');
    });
    it('opens mobile navigation, closes from its header and on navigation', async () => {
      const { user, container } = mountApp(390);
      expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
      await user.click(screen.getByRole('button', { name: 'Open navigation' }));
      let dialog = screen.getByRole('dialog', { name: 'Navigation' });
      expect(container).not.toContainElement(dialog);
      expect(dialog).toHaveAttribute('data-mobile', 'true');
      // Its own slot keeps the sheet padding and width rules off it.
      expect(dialog).toHaveAttribute('data-slot', 'sidebar');
      expect(dialog).toHaveAttribute('data-side', 'left');
      expect(dialog).toHaveAccessibleDescription('Displays the mobile sidebar.');
      await user.click(within(dialog).getByRole('button', { name: 'Close', exact: true }));
      await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument());
      await user.keyboard('[[');
      dialog = screen.getByRole('dialog', { name: 'Navigation' });
      await user.click(within(dialog).getByRole('link', { name: /Overview/ }));
      await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument());
      expect(location.hash).toBe('#projects');
    });
    it('refocuses the mobile navigation Keyboard guide button when the guide closes', async () => {
      const { user } = mountApp(390);
      await user.click(screen.getByRole('button', { name: 'Open navigation' }));
      const nav = screen.getByRole('dialog', { name: 'Navigation' });
      const opener = within(nav).getByRole('button', { name: 'Keyboard guide' });
      opener.focus();
      await user.keyboard('{Enter}');
      const guide = await screen.findByRole('dialog', { name: /keyboard/ });
      await waitFor(() => expect(guide).toHaveFocus());
      await user.keyboard('{Escape}');
      await waitFor(() => expect(guide).not.toBeInTheDocument());
      expect(screen.getByRole('dialog', { name: 'Navigation' })).toBeInTheDocument();
      await waitFor(() => expect(opener).toHaveFocus());
    });
    it('focuses search from the shortcut and icon, then selects a command by keyboard', async () => {
      const { user } = mountApp();
      await user.keyboard('{Control>}k{/Control}');
      const dialog = screen.getByRole('dialog', { name: 'Go anywhere' });
      expect(dialog).toHaveAccessibleDescription(
        'Search screens and actions. Keyboard shortcuts are listed beside each command.',
      );
      const input = within(dialog).getByRole('combobox');
      await waitFor(() => expect(input).toHaveFocus());
      const close = within(dialog).getByRole('button', { name: 'Close', exact: true });
      close.focus();
      await user.click(dialog.querySelector('[data-slot=input-group-addon] svg')!);
      expect(input).toHaveFocus();
      await user.paste('Usage');
      await user.keyboard('{ArrowDown}{Enter}');
      await waitFor(() => expect(location.hash).toBe('#usage'));
      expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
    });
    it('opens search with Slash and closes it with Escape', async () => {
      const { user } = mountApp();
      await user.keyboard('/');
      expect(screen.getByRole('dialog', { name: 'Go anywhere' })).toBeInTheDocument();
      await user.keyboard('{Escape}');
      await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument());
    });
    it('refocuses the rail search button when Go anywhere closes', async () => {
      const { user } = mountApp();
      const opener = screen.getByRole('button', { name: /Go anywhere/ });
      await user.click(opener);
      const dialog = screen.getByRole('dialog', { name: 'Go anywhere' });
      await waitFor(() => expect(within(dialog).getByRole('combobox')).toHaveFocus());
      await user.keyboard('{Escape}');
      await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument());
      await waitFor(() => expect(opener).toHaveFocus());
    });
    it('never drops focus to the body after a shortcut-opened Go anywhere closes', async () => {
      const { user } = mountApp();
      await waitFor(() => expect(screen.getByRole('heading', { level: 1 })).toHaveFocus());
      (document.activeElement as HTMLElement | null)?.blur();
      expect(document.body).toHaveFocus();
      await user.keyboard('/');
      const dialog = screen.getByRole('dialog', { name: 'Go anywhere' });
      await waitFor(() => expect(within(dialog).getByRole('combobox')).toHaveFocus());
      await user.keyboard('{Escape}');
      await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument());
      await waitFor(() => expect(document.getElementById('workspace')).toHaveFocus());
    });
    it('refocuses the rail search button when the Report sheet it opened closes', async () => {
      const { user } = mountApp();
      const opener = screen.getByRole('button', { name: /Go anywhere/ });
      await user.click(opener);
      const command = screen.getByRole('dialog', { name: 'Go anywhere' });
      await user.click(within(command).getByRole('option', { name: 'Report a bug' }));
      const sheet = await screen.findByRole('dialog', { name: 'Report a bug' });
      await waitFor(() => expect(sheet).toContainElement(document.activeElement as HTMLElement));
      await user.keyboard('{Escape}');
      await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument());
      await waitFor(() => expect(opener).toHaveFocus());
    });
    it('refocuses the element focused before a shortcut opened Go anywhere and the Report sheet', async () => {
      const { user } = mountApp();
      await waitFor(() => expect(screen.getByRole('heading', { level: 1 })).toHaveFocus());
      const opener = screen.getByRole('button', { name: 'Collapse navigation' });
      opener.focus();
      await user.keyboard('/');
      const command = screen.getByRole('dialog', { name: 'Go anywhere' });
      await user.click(within(command).getByRole('option', { name: 'Suggest a feature' }));
      const sheet = await screen.findByRole('dialog', { name: 'Suggest a feature' });
      await user.click(within(sheet).getByRole('button', { name: 'Close' }));
      await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument());
      await waitFor(() => expect(opener).toHaveFocus());
    });
    it('refocuses the Report sheet field after Ctrl+K from it, then the rail button', async () => {
      const { user } = mountApp();
      const opener = screen.getByRole('button', { name: /Go anywhere/ });
      await user.click(opener);
      const command = screen.getByRole('dialog', { name: 'Go anywhere' });
      await user.click(within(command).getByRole('option', { name: 'Report a bug' }));
      const sheet = await screen.findByRole('dialog', { name: 'Report a bug' });
      const title = within(sheet).getByRole('textbox', { name: 'Title' });
      await user.click(title);
      await user.keyboard('{Control>}k{/Control}');
      await screen.findByRole('dialog', { name: 'Go anywhere' });
      await user.keyboard('{Escape}');
      await waitFor(() =>
        expect(screen.queryByRole('dialog', { name: 'Go anywhere' })).not.toBeInTheDocument(),
      );
      await waitFor(() => expect(title).toHaveFocus());
      expect(screen.getByRole('dialog', { name: 'Report a bug' })).toBeInTheDocument();
      await user.keyboard('{Escape}');
      await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument());
      await waitFor(() => expect(opener).toHaveFocus());
    });
    it.each([
      ['pointer', fireEvent.pointerDown],
      ['key', (el: Element) => fireEvent.keyDown(el, { key: 'Shift' })],
    ])('keeps the focus chosen with a %s press before the page heading takes focus', (_, press) => {
      vi.useFakeTimers();
      mountApp();
      const opener = screen.getByRole('button', { name: /Go anywhere/ });
      press(opener);
      opener.focus();
      act(() => vi.advanceTimersByTime(1000));
      expect(opener).toHaveFocus();
    });
  });
});
describe('shortcuts', () => {
  it('Now shortcuts do nothing and palette hides the task action', async () => {
    history.replaceState({}, '', '/#home');
    const source = createFixtureSource();
    const pauseProject = vi.fn(),
      stopTask = vi.fn();
    source.set({ pauseProject, stopTask });
    render(<App source={source} />);
    fireEvent.keyDown(document.body, { key: 'p' });
    fireEvent.keyDown(document.body, { key: 'x' });
    expect(pauseProject).not.toHaveBeenCalled();
    expect(stopTask).not.toHaveBeenCalled();
    fireEvent.keyDown(document.body, { key: '/' });
    expect(screen.queryByText('Stop / resume task')).not.toBeInTheDocument();
  });
  it.each([
    { id: 'contains-done', state: { state: 'running' as const }, expected: 1 },
    { id: 'finished', state: { state: 'done' as const }, expected: 0 },
  ])('task shortcut uses actual state for $id', ({ id, state, expected }) => {
    history.replaceState({}, '', `/#task/${id}`);
    const source = createFixtureSource(),
      task = source.getSnapshot().tasks[0],
      stopTask = vi.fn();
    source.set({ tasks: [{ ...task, id, state }], stopTask });
    render(<App source={source} />);
    fireEvent.keyDown(document.body, { key: 'x' });
    expect(stopTask).toHaveBeenCalledTimes(expected);
  });
  it('the palette resumes only the displayed stopped task', async () => {
    const source = createFixtureSource(),
      task = source.getSnapshot().tasks[1];
    history.replaceState({}, '', `/#task/${task.id}`);
    const resumeTask = vi.fn().mockResolvedValue({ ok: true, value: task });
    source.set({
      tasks: source
        .getSnapshot()
        .tasks.map((t) =>
          t.id === task.id ? { ...t, state: { state: 'stopped', was: { state: 'running' } } } : t,
        ),
      resumeTask,
    });
    render(<App source={source} />);
    fireEvent.keyDown(document.body, { key: '/' });
    fireEvent.click(screen.getByText('Stop / resume task'));
    await act(async () => {});
    expect(resumeTask).toHaveBeenCalledExactlyOnceWith(task.id);
  });
  it('P affects the displayed project and never the first project', async () => {
    const source = createFixtureSource(),
      project = source.getSnapshot().projects[1];
    history.replaceState({}, '', `/#project/${project.id}`);
    const pauseProject = vi.fn().mockResolvedValue({ ok: true, value: project });
    source.set({
      pauseProject,
      settings: { values: { ...source.getSnapshot().settings.values, motion: 'Off' } },
    });
    render(<App source={source} />);
    fireEvent.keyDown(document.body, { key: 'p' });
    await act(async () => {});
    expect(pauseProject).toHaveBeenCalledExactlyOnceWith(project.id);
  });
  it('on a two-option question, key 3 selects Write a reply and shows the reply field', async () => {
    history.replaceState({}, '', '/#decision/local');
    render(<App source={createFixtureSource()} />);
    const reply = await screen.findByRole('radio', { name: 'Write a reply' });
    fireEvent.keyDown(document.body, { key: '2' });
    expect(screen.getByRole('radio', { name: /Include a second catalogue/ })).toBeChecked();
    fireEvent.keyDown(document.body, { key: '3' });
    expect(reply).toBeChecked();
    expect(screen.getByRole('textbox', { name: 'Your reply' })).toBeInTheDocument();
    fireEvent.keyDown(document.body, { key: '4' });
    expect(reply).toBeChecked();
  });
  it('on a three-option question, keys 1–3 select options and key 4 selects Write a reply', async () => {
    history.replaceState({}, '', '/#decision/local');
    const source = createFixtureSource(),
      q = source.getSnapshot().questions.find((q) => q.id === 'local')!;
    source.set({
      questions: [
        {
          ...q,
          options: [...q.options, { id: 3, text: 'Wait for the catalogue', effect: 'Later.' }],
        },
      ],
    });
    render(<App source={source} />);
    const reply = await screen.findByRole('radio', { name: 'Write a reply' });
    for (const [key, name] of [
      ['3', /Wait for the catalogue/],
      ['2', /Include a second catalogue/],
      ['1', /Start with one catalogue/],
    ] as const) {
      fireEvent.keyDown(document.body, { key });
      expect(screen.getByRole('radio', { name })).toBeChecked();
    }
    expect(screen.queryByRole('textbox', { name: 'Your reply' })).not.toBeInTheDocument();
    fireEvent.keyDown(document.body, { key: '4' });
    expect(reply).toBeChecked();
    expect(screen.getByRole('textbox', { name: 'Your reply' })).toBeInTheDocument();
    // Typing a digit in the reply field stays in the field.
    fireEvent.keyDown(screen.getByRole('textbox', { name: 'Your reply' }), { key: '1' });
    expect(reply).toBeChecked();
  });
  it('Add project is not a displayed project for P or the palette', () => {
    history.replaceState({}, '', '/#project/new');
    const source = createFixtureSource(),
      pauseProject = vi.fn();
    source.set({ pauseProject });
    render(<App source={source} />);
    fireEvent.keyDown(document.body, { key: 'p' });
    expect(pauseProject).not.toHaveBeenCalled();
    fireEvent.keyDown(document.body, { key: '/' });
    expect(screen.queryByText('Pause / resume project')).not.toBeInTheDocument();
  });
});
it('keeps J/K and Enter on compact needs rows', async () => {
  location.hash = '#home';
  const source = createFixtureSource(),
    view = mount(<App source={source} />, source);
  await waitFor(() => expect(screen.getByRole('heading', { level: 1, name: 'Now' })).toHaveFocus());
  fireEvent.keyDown(document.activeElement!, { key: 'j' });
  expect(document.activeElement).toHaveAttribute('href', '#decision/local');
  fireEvent.keyDown(document.activeElement!, { key: 'j' });
  expect(document.activeElement).toHaveAttribute('href', '#decision/atlas-format');
  fireEvent.keyDown(document.activeElement!, { key: 'k' });
  fireEvent.keyDown(document.activeElement!, { key: 'Enter' });
  await waitFor(() => expect(location.hash).toBe('#decision/local'));
  view.unmount();
});
it('groups every project once, working before needs, retaining overlapping needs and paused status', () => {
  const source = createFixtureSource(),
    s = source.getSnapshot();
  const groups = railGroups(s, Date.now());
  expect(groups.map((g) => [g.label, g.projects.length])).toEqual([
    ['Working now', 2],
    ['Needs you', 1],
    ['Recent', 1],
    ['Quiet', 4],
  ]);
  expect(groups[0].projects.map((p) => p.name)).toEqual(['Harbour', 'Dispatch']);
  expect(needs(s, groups[0].projects[0])).toBeGreaterThan(0);
  mount(<App source={source} />, source);
  const rail = document.querySelector('.rail-projects')!;
  for (const pulse of rail.querySelectorAll('[aria-label="Working"]'))
    expect(pulse).toHaveAttribute('role', 'img');
  expect(
    within(rail as HTMLElement)
      .getByRole('link', { name: /Harbour:/ })
      .querySelector('.project-needs'),
  ).toHaveTextContent('1');
  expect(
    within(rail as HTMLElement)
      .getByRole('link', { name: /Harbour:/ })
      .querySelector('.project-pulse'),
  ).toBeInTheDocument();
});
describe('key A and the decision page', () => {
  const answered = (q: Question, choice: number): Question => ({
    ...q,
    open: false,
    answer: { choice },
  });
  function withAnswers(source = createFixtureSource()) {
    const answer = vi.fn(async (id: string, body: { choice?: number }) => {
      const q = source.getSnapshot().questions.find((q) => q.id === id)!,
        value = answered(q, body.choice!);
      source.set({
        questions: source.getSnapshot().questions.map((o) => (o.id === id ? value : o)),
      });
      return { ok: true as const, value };
    });
    source.set({ answer, settings: { values: { motion: 'Off' } } });
    return { source, answer };
  }
  it.each([
    ['atlas', '#decision/atlas-format/project/atlas'],
    ['harbour', '#decision/local/project/harbour'],
  ])("on project %s's page opens that project's question", async (project, hash) => {
    history.replaceState({}, '', `/#project/${project}`);
    render(<App source={createFixtureSource()} />);
    fireEvent.keyDown(document.body, { key: 'a' });
    await waitFor(() => expect(location.hash).toBe(hash));
  });
  it("on a task page opens that task's question before its project's older one", async () => {
    history.replaceState({}, '', '/#task/resume');
    const source = createFixtureSource(),
      [local, atlas] = source.getSnapshot().questions;
    source.set({
      questions: [
        { ...local, task: 'resume' },
        { ...atlas, id: 'harbour-older', project: 'harbour', opened_at: 1 },
        atlas,
      ],
    });
    render(<App source={source} />);
    fireEvent.keyDown(document.body, { key: 'a' });
    await waitFor(() => expect(location.hash).toBe('#decision/local/task/resume'));
    // The back link names the task it returns to.
    fireEvent.click(await screen.findByRole('button', { name: 'Keep removed records removed' }));
    await waitFor(() => expect(location.hash).toBe('#task/resume'));
  });
  it('on Now opens the oldest open question', async () => {
    history.replaceState({}, '', '/#home');
    render(<App source={createFixtureSource()} />);
    fireEvent.keyDown(document.body, { key: 'a' });
    await waitFor(() => expect(location.hash).toBe('#decision/atlas-format'));
  });
  it('keeps the recommended option on a question opened with A', async () => {
    history.replaceState({}, '', '/#project/harbour');
    render(<App source={createFixtureSource()} />);
    fireEvent.keyDown(document.body, { key: 'a' });
    expect(await screen.findByRole('radio', { name: /Start with one catalogue/ })).toBeChecked();
  });
  it('pins bare #decision, shows the answer once sent and sends a second Ctrl+Enter nowhere', async () => {
    history.replaceState({}, '', '/#decision');
    const { source, answer } = withAnswers();
    render(<App source={source} />);
    await waitFor(() => expect(location.hash).toBe('#decision/atlas-format'));
    const geo = await screen.findByRole('radio', { name: /GeoJSON/ });
    // dvb chose this question, so nothing starts selected and Ctrl+Enter sends nothing.
    expect(screen.queryByRole('radio', { checked: true })).not.toBeInTheDocument();
    fireEvent.keyDown(document.body, { key: 'Enter', ctrlKey: true });
    expect(answer).not.toHaveBeenCalled();
    fireEvent.keyDown(document.body, { key: '1' });
    expect(geo).toBeChecked();
    fireEvent.keyDown(document.body, { key: 'Enter', ctrlKey: true });
    expect(await screen.findByText('Answered')).toBeInTheDocument();
    expect(screen.getByText('Which map format should imports accept first?')).toBeInTheDocument();
    expect(screen.getByText('GeoJSON')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Back to project' })).toBeInTheDocument();
    fireEvent.keyDown(document.body, { key: 'Enter', ctrlKey: true });
    await act(async () => {});
    expect(answer).toHaveBeenCalledExactlyOnceWith('atlas-format', { choice: 1 });
    expect(location.hash).toBe('#decision/atlas-format');
  });
  it.each([
    [
      'the Next question button',
      () => fireEvent.click(screen.getByRole('button', { name: /Next question/ })),
    ],
    ['J', () => fireEvent.keyDown(document.body, { key: 'j' })],
  ])('moves on with %s, with no option selected', async (_, next) => {
    history.replaceState({}, '', '/#decision/atlas-format');
    const { source } = withAnswers();
    render(<App source={source} />);
    // Opened by link: the recommended option is selected.
    expect(await screen.findByRole('radio', { name: /GeoJSON/ })).toBeChecked();
    fireEvent.keyDown(document.body, { key: 'Enter', ctrlKey: true });
    expect(await screen.findByText('Answered')).toBeInTheDocument();
    next();
    await waitFor(() => expect(location.hash).toBe('#decision/local'));
    expect(await screen.findByText('Import one catalogue first?')).toBeInTheDocument();
    expect(screen.queryByRole('radio', { checked: true })).not.toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Send answer' })).toBeDisabled();
  });
  it('moves focus into the reply box when the number after the last option picks Write a reply', async () => {
    history.replaceState({}, '', '/#decision/local');
    const source = createFixtureSource();
    source.set({ settings: { values: { motion: 'Off' } } });
    render(<App source={source} />);
    const user = userEvent.setup({ pointerEventsCheck: PointerEventsCheckLevel.Never });
    expect(await screen.findAllByRole('radio')).toHaveLength(3);
    // The page focuses its heading once it settles; the owner presses keys after that.
    await waitFor(() => expect(document.activeElement?.tagName).toBe('H1'), { timeout: 2000 });
    await user.keyboard('3');
    const box = screen.getByRole('textbox', { name: 'Your reply' });
    expect(box).toHaveFocus();
    await user.keyboard('Start locally');
    expect(box).toHaveValue('Start locally');
  });
  it('says what was sent when it answered the only open question', async () => {
    history.replaceState({}, '', '/#decision');
    const { source } = withAnswers();
    source.set({ questions: source.getSnapshot().questions.filter((q) => q.id === 'local') });
    render(<App source={source} />);
    fireEvent.click(await screen.findByRole('radio', { name: /Include a second catalogue/ }));
    fireEvent.keyDown(document.body, { key: 'Enter', ctrlKey: true });
    expect(await screen.findByText('Answered')).toBeInTheDocument();
    expect(screen.getByText('Include a second catalogue')).toBeInTheDocument();
    expect(screen.queryByText('No current question.')).not.toBeInTheDocument();
    expect(screen.queryByRole('button', { name: /Next question/ })).not.toBeInTheDocument();
  });
});
