import { mount } from '../support/render';
import { describe, it, expect, vi, afterEach } from 'vitest';
import { act, fireEvent, screen, waitFor, within } from '@testing-library/react';
import { createFixtureSource } from '../support/render';
import { ProjectPage, AddProject } from '@/screens/project';
import type { Event, Task } from '@/lib/data/types';
import { needs } from '@/lib/data/work';
describe('project brief', () => {
  it('reads the saved brief, saves an edit and clears it with empty text', async () => {
    const source = createFixtureSource();
    source.replies['/projects/harbour/brief'] = { text: 'Keep it small.' };
    const save = vi.spyOn(source.getSnapshot(), 'setBrief');
    source.replies['PUT /projects/harbour/brief'] = { text: 'Prefer plain words.' };
    const { user } = mount(<ProjectPage id="harbour" tab="brief" />, source);
    await user.click(screen.getByRole('tab', { name: 'Brief' }));
    const box = await screen.findByLabelText('Project brief');
    await waitFor(() => expect(box).toHaveValue('Keep it small.'));
    await user.clear(box);
    await user.type(box, 'Prefer plain words.');
    await user.click(screen.getByRole('button', { name: 'Save brief' }));
    expect(await screen.findByText('Brief saved.')).toBeInTheDocument();
    expect(save).toHaveBeenCalledWith('harbour', { text: 'Prefer plain words.' });
    source.replies['PUT /projects/harbour/brief'] = { text: '' };
    await user.clear(box);
    await user.click(screen.getByRole('button', { name: 'Save brief' }));
    await waitFor(() => expect(save).toHaveBeenLastCalledWith('harbour', { text: '' }));
  });
});
describe('project autonomy', () => {
  it('is off by default, turns on and off, and shows the saved value after reload', async () => {
    const source = createFixtureSource();
    source.set({ settings: { values: { autonomous_projects: ['Dispatch'] } } });
    const save = vi.spyOn(source.getSnapshot(), 'changeSettings');
    const { user, unmount } = mount(<ProjectPage id="harbour" tab="settings" />, source);
    const toggle = screen.getByRole('switch', { name: 'Autonomous' });
    expect(toggle).not.toBeChecked();
    expect(toggle).toHaveAccessibleDescription(
      'dvb approves its own plans and starts proposed work from trusted issues. Failures, merge-anyway and scope changes still come to you.',
    );
    await user.click(toggle);
    expect(save).toHaveBeenCalledWith({
      values: { autonomous_projects: ['Dispatch', 'Harbour'] },
    });
    act(() =>
      source.set({ settings: { values: { autonomous_projects: ['Dispatch', 'Harbour'] } } }),
    );
    unmount();
    mount(<ProjectPage id="harbour" tab="settings" />, source);
    const saved = screen.getByRole('switch', { name: 'Autonomous' });
    expect(saved).toBeChecked();
    await user.click(saved);
    expect(save).toHaveBeenLastCalledWith({ values: { autonomous_projects: ['Dispatch'] } });
  });
});
describe('project conversation toggles', () => {
  const ev = (id: number, kind: string, payload: object): Event =>
    ({ id, at: 1, actor: 'engine', project: 'harbour', kind, payload }) as Event;
  it('shows only each toggled category plus the question, including task completion as a task update', async () => {
    const source = createFixtureSource();
    source.set({
      events: [
        ev(1, 'question.opened', {
          question: 'q1',
          subject: 'Which way?',
          format: 'simple',
          options: [],
          recommended: 1,
          lapses_on: 'never',
        }),
        ev(2, 'task.started', { task: 'alpha' }),
        ev(3, 'task.done', { task: 'beta' }),
        ev(4, 'pr.merged', { task: 'beta', pr: 42 }),
        ev(5, 'run.output', { item: { text: 'Storage implementation update' } }),
        ev(6, 'goal.approved', { goal: 'g' }),
        ev(7, 'goal.done', { goal: 'g' }),
        ev(8, 'decision.recorded', {
          by: 'harbour-backend',
          title: 'Pick storage',
          choice: 'SQLite',
          reason: 'r',
        }),
      ],
    });
    source.replies['/events/recent'] = { cursor: 0, events: [], before: null, activity: [] };
    const { user } = mount(<ProjectPage id="harbour" tab="conversation" />, source);
    const conversation = within(screen.getByRole('tabpanel', { hidden: false }));
    const question = 'Needs you: Which way?';
    const categories = [
      {
        name: 'Task updates',
        lines: [
          'Started alpha',
          'Task done: beta',
          'PR #42 merged',
          'Storage implementation update',
        ],
      },
      { name: 'Phase completions', lines: ['Goal approved', 'Goal done'] },
      { name: 'Team decisions', lines: ['harbour-backend decided: Pick storage'] },
    ];
    const expectFeed = (lines: string[]) => {
      const feed = conversation.getByLabelText('Activity feed');
      expect(Array.from(feed.querySelectorAll('p'), (p) => p.textContent)).toEqual([
        question,
        ...lines,
      ]);
      expect(within(feed).getByText(question)).toBeInTheDocument();
    };
    expectFeed(categories.flatMap((category) => category.lines));
    for (const { name } of categories) {
      const toggle = conversation.getByRole('switch', { name });
      expect(toggle).toBeChecked();
      await user.click(toggle);
      expect(toggle).not.toBeChecked();
    }
    expectFeed([]);
    for (const { name, lines } of categories) {
      const toggle = conversation.getByRole('switch', { name });
      await user.click(toggle);
      expect(toggle).toBeChecked();
      expectFeed(lines);
      await user.click(toggle);
      expect(toggle).not.toBeChecked();
      expectFeed([]);
    }
  });
});
describe('create', () => {
  it('adds a project with explicitly chosen CI mode and records a new goal', async () => {
    const source = createFixtureSource();
    source.replies['/github'] = { connected: true, organization: 'example-org', install_url: null };
    source.replies['/github/checks'] = {
      status: 'discovered',
      default_branch: 'main',
      head: 'abc',
      checks: [],
    };
    const { user, unmount } = mount(<AddProject />, source);
    const add = vi.spyOn(source.getSnapshot(), 'addProject');
    await user.type(await screen.findByRole('textbox', { name: 'Name' }), 'Example');
    await user.type(screen.getByRole('textbox', { name: 'Repository' }), 'example-org/example');
    await user.click(await screen.findByRole('switch', { name: 'Continue without CI' }));
    await user.click(screen.getByRole('button', { name: 'Add project' }));
    expect(add).toHaveBeenCalledWith({ name: 'Example', repo: 'example-org/example', ci: 'no_ci' });
    const project = { ...source.getSnapshot().projects[0], id: 'project-example', name: 'Example' };
    source.set({ projects: [project], goals: [] });
    unmount();
    const state = vi.spyOn(source.getSnapshot(), 'stateGoal');
    mount(<ProjectPage id={project.id} />, source);
    await user.click(screen.getByRole('button', { name: 'Set a goal' }));
    await user.type(screen.getByRole('textbox', { name: 'Goal' }), 'Create an importer');
    await user.click(screen.getByRole('button', { name: 'Save direction' }));
    expect(state).toHaveBeenCalledWith(project.id, { text: 'Create an importer' });
  });
});
describe('goals', () => {
  it('Project board uses task state and pauses through the owner API', async () => {
    const { source, user } = mount(<ProjectPage id="harbour" />);
    expect(screen.getByRole('link', { name: 'Keep removed records removed' })).toHaveAttribute(
      'href',
      '#task/resume',
    );
    const pause = vi.spyOn(source.getSnapshot(), 'pauseProject');
    await user.click(screen.getByRole('button', { name: 'Pause' }));
    expect(pause).toHaveBeenCalledWith('harbour');
    act(() =>
      source.set({
        projects: [
          { ...source.getSnapshot().projects[0], paused: true },
          ...source.getSnapshot().projects.slice(1),
        ],
      }),
    );
    expect(screen.getByText('Project paused')).toBeInTheDocument();
  });
  it('keeps a running task in Working when its project is paused', () => {
    const source = createFixtureSource(),
      s = source.getSnapshot(),
      task = s.tasks.find((t) => t.project === 'harbour')!;
    source.set({
      projects: s.projects.map((p) => ({ ...p, paused: p.id === 'harbour' })),
      tasks: [{ ...task, state: { state: 'running' } }],
      events: [],
    });
    mount(<ProjectPage id="harbour" />, source);
    expect(screen.getByText('Project paused')).toBeInTheDocument();
    expect(
      within(screen.getByRole('region', { name: 'Working, 1 task' })).getByRole('link', {
        name: task.title,
      }),
    ).toBeInTheDocument();
    expect(screen.queryByRole('region', { name: /^Stopped,/ })).not.toBeInTheDocument();
    expect(source.getSnapshot().tasks[0].state.state).toBe('running');
  });
  it('states a fresh goal after the latest goal completes and keeps the completed goal', async () => {
    const source = createFixtureSource(),
      completed = { ...source.getSnapshot().goals[0], state: 'done' as const };
    source.set({
      goals: [
        { ...completed, id: 'earlier', state: 'done' },
        completed,
        source.getSnapshot().goals[1],
      ],
    });
    const stateGoal = vi.spyOn(source.getSnapshot(), 'stateGoal'),
      redirect = vi.spyOn(source.getSnapshot(), 'redirect');
    const { user } = mount(<ProjectPage id="harbour" />, source);
    expect(screen.getByText('Last goal done')).toBeInTheDocument();
    // The ended goal is named under its label, which keeps it apart from the question below.
    const name = screen.getByRole('heading', {
      level: 2,
      name: 'Keep imported catalogue records consistent.',
    });
    expect(screen.getByText('Last goal done').closest('.goal-copy')?.children[1]).toBe(
      name.parentElement,
    );
    expect(
      name.compareDocumentPosition(
        screen.getByRole('button', { name: 'Import one catalogue first?' }),
      ),
    ).toBe(Node.DOCUMENT_POSITION_FOLLOWING);
    expect(screen.queryByText('Current goal')).not.toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Set a new goal' }));
    expect(screen.getByRole('textbox', { name: 'New goal' })).toHaveValue('');
    // With no goal in progress there is nothing to start after.
    expect(screen.queryByRole('radiogroup')).not.toBeInTheDocument();
    expect(screen.queryByText('Start after the current goal')).not.toBeInTheDocument();
    await user.type(screen.getByRole('textbox', { name: 'New goal' }), 'Ship the next release');
    await user.click(screen.getByRole('button', { name: 'Save new goal' }));
    expect(stateGoal).toHaveBeenCalledExactlyOnceWith('harbour', { text: 'Ship the next release' });
    expect(redirect).not.toHaveBeenCalled();
    expect(source.getSnapshot().goals.find((g) => g.id === completed.id)).toEqual(completed);
    act(() =>
      source.set({
        goals: [
          ...source.getSnapshot().goals,
          { id: 'next-goal', project: 'harbour', text: 'Ship the next release', state: 'stated' },
        ],
      }),
    );
    expect(screen.getByText('Waiting for a plan')).toBeInTheDocument();
  });
  it('says the plan waits for the owner while its approval question is open', () => {
    const source = createFixtureSource(),
      [goal, ...goals] = source.getSnapshot().goals,
      questions = source.getSnapshot().questions;
    source.set({ goals: [{ ...goal, state: 'stated' }, ...goals] });
    mount(<ProjectPage id="harbour" />, source);
    expect(screen.getByText('Waiting for your answer')).toBeInTheDocument();
    expect(screen.queryByText('Waiting for a plan')).not.toBeInTheDocument();
    act(() => source.set({ questions: questions.map((q) => ({ ...q, open: false })) }));
    expect(screen.getByText('Waiting for a plan')).toBeInTheDocument();
    expect(screen.queryByText('Waiting for your answer')).not.toBeInTheDocument();
    act(() =>
      source.set({
        questions,
        tasks: source.getSnapshot().tasks.filter((t) => t.goal !== goal.id),
      }),
    );
    expect(screen.getByText('Waiting for a plan')).toBeInTheDocument();
    act(() => source.set({ goals: [goal, ...goals], tasks: source.getSnapshot().tasks }));
    expect(screen.getByText('Current goal')).toBeInTheDocument();
    expect(screen.queryByText('Waiting for your answer')).not.toBeInTheDocument();
  });
  it('wraps a long open-question subject inside the goal card and opens the question', async () => {
    const source = createFixtureSource(),
      subject =
        'Should the importer accept both the legacy catalogue export and the new supplier feed before the first release, or ship one format now and add the other later?';
    source.set({
      questions: source
        .getSnapshot()
        .questions.map((q) => (q.id === 'local' ? { ...q, subject } : q)),
    });
    const { user } = mount(<ProjectPage id="harbour" />, source);
    const button = screen.getByRole('button', { name: subject });
    expect(button).toHaveTextContent(subject);
    expect(button).toHaveClass('written-subject', 'goal-question');
    await user.click(button);
    expect(location.hash).toBe('#decision/local/project/harbour');
    history.replaceState({}, '', '/');
  });
  it("keeps a plan question's line breaks on the goal card", () => {
    const source = createFixtureSource(),
      subject =
        'Start this plan? Plan the spring planting.\n\nPlan:\n1. backend — Sow beds\n2. frontend — Show beds\n3. backend — Water beds';
    source.set({
      questions: source
        .getSnapshot()
        .questions.map((q) => (q.id === 'local' ? { ...q, subject } : q)),
    });
    mount(<ProjectPage id="harbour" />, source);
    const button = document.querySelector('.goal-question')!;
    // Each step starts on its own line: the breaks reach the page and the button keeps them.
    expect(button.textContent).toBe(subject);
    expect(button.textContent).toContain('\n2. frontend — Show beds\n3.');
  });
  it('redirects an active plan when the owner changes course', async () => {
    const source = createFixtureSource(),
      goal = source.getSnapshot().goals[0];
    const stateGoal = vi.spyOn(source.getSnapshot(), 'stateGoal'),
      redirect = vi.spyOn(source.getSnapshot(), 'redirect');
    const { user } = mount(<ProjectPage id="harbour" />, source);
    await user.click(screen.getByRole('button', { name: 'Change course' }));
    const draft = screen.getByRole('textbox', { name: 'Goal' });
    expect(draft).toHaveValue(goal.text);
    await user.clear(draft);
    await user.type(draft, 'Adjust the active plan');
    await user.click(screen.getByRole('button', { name: 'Save direction' }));
    expect(redirect).toHaveBeenCalledExactlyOnceWith(goal.id, { text: 'Adjust the active plan' });
    expect(stateGoal).not.toHaveBeenCalled();
  });
  it('drops the active goal through the owner API', async () => {
    const source = createFixtureSource(),
      goal = source.getSnapshot().goals[0];
    const dropGoal = vi.spyOn(source.getSnapshot(), 'dropGoal');
    const { user } = mount(<ProjectPage id="harbour" />, source);
    await user.click(screen.getByRole('button', { name: 'Drop goal' }));
    expect(dropGoal).toHaveBeenCalledExactlyOnceWith(goal.id);
    act(() =>
      source.set({
        goals: [{ ...goal, state: 'dropped' }, ...source.getSnapshot().goals.slice(1)],
      }),
    );
    expect(screen.getByText('Last goal dropped')).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Drop goal' })).not.toBeInTheDocument();
  });
});
describe('several current goals', () => {
  function twoGoals() {
    const source = createFixtureSource(),
      task = source.getSnapshot().tasks.find((t) => t.project === 'harbour')!;
    source.set({
      goals: [
        {
          id: 'goal-a',
          project: 'harbour',
          text: 'Plan the spring planting',
          state: 'in_progress',
        },
        { id: 'goal-b', project: 'harbour', text: 'Add a watering schedule', state: 'stated' },
      ],
      tasks: [
        { ...task, id: 'sow', title: 'Sow beds', goal: 'goal-a', state: { state: 'done' } },
        { ...task, id: 'map', title: 'Map beds', goal: 'goal-a', state: { state: 'running' } },
        { ...task, id: 'water', title: 'Water beds', goal: 'goal-b', state: { state: 'planned' } },
      ],
    });
    return source;
  }
  const card = (text: string) =>
    within(screen.getByRole('heading', { name: text }).closest<HTMLElement>('.goal-section')!);
  it('lists each current goal with its own progress, the running one first, and drops the named goal', async () => {
    const source = twoGoals(),
      dropGoal = vi.spyOn(source.getSnapshot(), 'dropGoal');
    const { user } = mount(<ProjectPage id="harbour" />, source);
    expect(
      Array.from(document.querySelectorAll('.project-page-goal'), (h) => h.textContent),
    ).toEqual(['Plan the spring planting', 'Add a watering schedule']);
    const running = card('Plan the spring planting'),
      queued = card('Add a watering schedule');
    expect(running.getByText('In progress')).toBeInTheDocument();
    expect(running.getByText('1')).toBeInTheDocument();
    expect(running.getByText('/ 2')).toBeInTheDocument();
    expect(queued.getByText('Waiting for a plan')).toBeInTheDocument();
    expect(queued.getByText('0')).toBeInTheDocument();
    expect(queued.getByText('/ 1')).toBeInTheDocument();
    // No unnamed Drop goal or Change course is left in the header.
    expect(screen.queryByRole('button', { name: 'Drop goal' })).not.toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Change course' })).not.toBeInTheDocument();
    await user.click(queued.getByRole('button', { name: 'Drop goal: Add a watering schedule' }));
    expect(dropGoal).toHaveBeenCalledExactlyOnceWith('goal-b');
    await user.click(running.getByRole('button', { name: 'Drop goal: Plan the spring planting' }));
    expect(dropGoal).toHaveBeenLastCalledWith('goal-a');
  });
  it('changes course for the goal whose card it is on', async () => {
    const source = twoGoals(),
      redirect = vi.spyOn(source.getSnapshot(), 'redirect');
    const { user } = mount(<ProjectPage id="harbour" />, source);
    const queued = card('Add a watering schedule');
    await user.click(
      queued.getByRole('button', { name: 'Change course: Add a watering schedule' }),
    );
    const draft = queued.getByRole('textbox', { name: 'Goal' });
    expect(draft).toHaveFocus();
    expect(draft).toHaveValue('Add a watering schedule');
    await user.clear(draft);
    await user.type(draft, 'Add a weekly watering schedule');
    await user.click(queued.getByRole('button', { name: 'Save direction' }));
    expect(redirect).toHaveBeenCalledExactlyOnceWith('goal-b', {
      text: 'Add a weekly watering schedule',
    });
  });
  it('states a Next goal after the newest current goal', async () => {
    const source = twoGoals(),
      stateGoal = vi.spyOn(source.getSnapshot(), 'stateGoal');
    const { user } = mount(<ProjectPage id="harbour" />, source);
    await user.click(screen.getByRole('button', { name: 'Next goal' }));
    const draft = screen.getByRole('textbox', { name: 'Next goal' });
    expect(draft).toHaveFocus();
    expect(screen.getByRole('radio', { name: 'Start after the current goal' })).toBeChecked();
    expect(screen.getByRole('radio', { name: 'Start now' })).not.toBeChecked();
    await user.keyboard('Harvest the beds');
    await user.click(screen.getByRole('button', { name: 'Save next goal' }));
    expect(stateGoal).toHaveBeenCalledExactlyOnceWith('harbour', {
      text: 'Harvest the beds',
      after: 'goal-b',
    });
  });
  it('states a goal that starts now with no after when the owner picks Start now', async () => {
    const source = twoGoals(),
      stateGoal = vi.spyOn(source.getSnapshot(), 'stateGoal');
    const { user } = mount(<ProjectPage id="harbour" />, source);
    await user.click(screen.getByRole('button', { name: 'Next goal' }));
    await user.keyboard('Harvest the beds');
    await user.click(screen.getByRole('radio', { name: 'Start now' }));
    expect(screen.getByRole('radio', { name: 'Start now' })).toBeChecked();
    await user.click(screen.getByRole('button', { name: 'Save new goal' }));
    expect(stateGoal).toHaveBeenCalledExactlyOnceWith('harbour', { text: 'Harvest the beds' });
  });
  it('shows a queued goal as waiting for the goal before it, named by its text', () => {
    const source = twoGoals();
    source.set({
      goals: source
        .getSnapshot()
        .goals.map((g) =>
          g.id === 'goal-b'
            ? { ...g, waits_for: 'goal-a' }
            : { ...g, text: 'Plan the spring planting. Beds first, then paths.' },
        ),
    });
    mount(<ProjectPage id="harbour" />, source);
    const queued = card('Add a watering schedule');
    expect(queued.getByText('Waiting for Plan the spring planting.')).toBeInTheDocument();
    expect(queued.queryByText('Waiting for a plan')).not.toBeInTheDocument();
  });
  it('states a Next goal after the only current goal, which keeps its header actions', async () => {
    const source = createFixtureSource(),
      goal = source.getSnapshot().goals[0],
      stateGoal = vi.spyOn(source.getSnapshot(), 'stateGoal');
    const { user } = mount(<ProjectPage id="harbour" />, source);
    expect(screen.getByRole('button', { name: 'Change course' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Drop goal' })).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Next goal' }));
    await user.keyboard('Ship the next release');
    await user.click(screen.getByRole('button', { name: 'Save next goal' }));
    expect(stateGoal).toHaveBeenCalledExactlyOnceWith('harbour', {
      text: 'Ship the next release',
      after: goal.id,
    });
  });
});
describe('goal editor focus', () => {
  it('Tab to Set a goal, Enter and typing lands in the Goal field; Escape returns to the button', async () => {
    const source = createFixtureSource();
    source.set({ goals: [] });
    const { user } = mount(<ProjectPage id="harbour" />, source);
    screen.getByRole('button', { name: 'Pause' }).focus();
    await user.tab();
    const set = screen.getByRole('button', { name: 'Set a goal' });
    expect(set).toHaveFocus();
    await user.keyboard('{Enter}');
    await user.keyboard('Create an importer');
    const field = screen.getByRole('textbox', { name: 'Goal' });
    expect(field).toHaveFocus();
    expect(field).toHaveValue('Create an importer');
    await user.keyboard('{Escape}');
    expect(screen.queryByRole('textbox', { name: 'Goal' })).not.toBeInTheDocument();
    expect(set).toHaveFocus();
  });
  it('Change course focuses the Goal field and Cancel returns to Change course', async () => {
    const { user } = mount(<ProjectPage id="harbour" />);
    const change = screen.getByRole('button', { name: 'Change course' });
    await user.click(change);
    expect(screen.getByRole('textbox', { name: 'Goal' })).toHaveFocus();
    await user.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(change).toHaveFocus();
  });
});
describe('validation', () => {
  afterEach(() => {
    history.replaceState({}, '', '/');
  });
  it('displays project validation detail instead of question instructions', async () => {
    const source = createFixtureSource();
    source.replies['/github'] = { connected: true, organization: 'example-org', install_url: null };
    source.set({
      addProject: async () => ({
        ok: false,
        refusal: { code: 'VALUE_CHANGED', detail: 'bad project name notes.v2' },
      }),
    });
    const { user } = mount(<AddProject />, source);
    await user.type(await screen.findByRole('textbox', { name: 'Name' }), 'notes.v2');
    await user.type(screen.getByRole('textbox', { name: 'Repository' }), 'example-org/notes.v2');
    await screen.findByRole('switch', { name: 'tests' });
    await user.click(screen.getByRole('button', { name: 'Add project' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('bad project name notes.v2');
  });
});
describe('Home project visits', () => {
  it('opening a project posts seen', async () => {
    const source = createFixtureSource();
    source.set({ events: [] });
    const seen = vi.spyOn(source.getSnapshot(), 'markProjectSeen');
    mount(<ProjectPage id="harbour" />, source);
    await waitFor(() => expect(seen).toHaveBeenCalledWith('harbour'));
  });
});

describe('Add project GitHub handoff', () => {
  it('opens the walkthrough GitHub step when no App is installed', async () => {
    const source = createFixtureSource();
    const add = vi.spyOn(source.getSnapshot(), 'addProject');
    mount(<AddProject />, source);
    expect(
      await screen.findByRole('heading', { name: 'Connect your GitHub account or organization' }),
    ).toBeVisible();
    expect(screen.queryByRole('textbox', { name: 'Repository' })).not.toBeInTheDocument();
    expect(add).not.toHaveBeenCalled();
  });

  it.each([false, true])(
    'rechecks on submit and returns every draft field after installation (no CI: %s)',
    async (noCi) => {
      vi.useFakeTimers();
      const source = createFixtureSource();
      source.replies['/github'] = {
        connected: true,
        organization: 'example-org',
        install_url: null,
      };
      if (noCi)
        source.replies['/github/checks'] = {
          status: 'discovered',
          default_branch: 'main',
          head: 'abc',
          checks: [],
        };
      const add = vi.spyOn(source.getSnapshot(), 'addProject');
      mount(<AddProject />, source);
      await act(async () => {});
      fireEvent.change(screen.getByLabelText('Name'), { target: { value: 'notes' } });
      fireEvent.change(screen.getByLabelText('Repository'), {
        target: { value: 'example-org/notes' },
      });
      await act(async () => {});
      if (noCi) fireEvent.click(screen.getByRole('switch', { name: 'Continue without CI' }));
      source.replies['/github'] = { connected: false, organization: null, install_url: null };
      await act(async () => {
        fireEvent.click(screen.getByRole('button', { name: 'Add project' }));
      });
      expect(
        screen.getByRole('heading', { name: 'Connect your GitHub account or organization' }),
      ).toBeVisible();
      expect(add).not.toHaveBeenCalled();
      source.replies['/github'] = {
        connected: false,
        organization: 'example-org',
        install_url: 'https://github.com/apps/notes/installations/new',
      };
      await act(async () => {
        await vi.advanceTimersByTimeAsync(3000);
      });
      expect(
        screen.getByRole('button', { name: "Open GitHub to install dvb's App" }),
      ).toBeVisible();
      expect(screen.queryByLabelText('Repository')).not.toBeInTheDocument();
      source.replies['/github'] = {
        connected: true,
        organization: 'example-org',
        install_url: null,
      };
      await act(async () => {
        await vi.advanceTimersByTimeAsync(3000);
      });
      expect(screen.getByLabelText('Name')).toHaveValue('notes');
      expect(screen.getByLabelText('Repository')).toHaveValue('example-org/notes');
      if (noCi) expect(screen.getByRole('switch', { name: 'Continue without CI' })).toBeChecked();
      else expect(screen.getByRole('switch', { name: 'tests' })).toBeChecked();
      expect(add).not.toHaveBeenCalled();
    },
  );
});

it('keeps connection read failures separate from missing App setup and allows retry', async () => {
  const source = createFixtureSource();
  source.statuses['/github'] = 409;
  source.replies['/github'] = { code: 'INVALID_VALUE', detail: 'GitHub connection unavailable' };
  mount(<AddProject />, source);
  expect(await screen.findByRole('alert')).toHaveTextContent('GitHub connection unavailable');
  expect(
    screen.queryByRole('textbox', { name: 'GitHub account or organization' }),
  ).not.toBeInTheDocument();
  expect(screen.queryByRole('textbox', { name: 'Repository' })).not.toBeInTheDocument();
  source.statuses['/github'] = 200;
  source.replies['/github'] = { connected: true, organization: 'example-org', install_url: null };
  fireEvent.click(screen.getByRole('button', { name: 'Retry GitHub connection' }));
  expect(await screen.findByRole('textbox', { name: 'Repository' })).toBeVisible();
});

describe('shared check discovery', () => {
  it.each(['inaccessible', 'failed'] as const)(
    'never offers no CI for %s discovery and refreshes after repository access changes',
    async (status) => {
      const source = createFixtureSource();
      source.replies['/github'] = {
        connected: true,
        organization: 'example-org',
        install_url: null,
      };
      source.replies['/github/checks'] = {
        status,
        failure: {
          code: status === 'inaccessible' ? 'repository_inaccessible' : 'discovery_failed',
          message: 'Check App repository access and retry.',
          retryable: true,
        },
      };
      const add = vi.spyOn(source.getSnapshot(), 'addProject');
      const { user } = mount(<AddProject />, source);
      await user.type(await screen.findByLabelText('Name'), 'notes');
      await user.type(screen.getByLabelText('Repository'), 'example-org/notes');
      expect(await screen.findByText('Check App repository access and retry.')).toBeVisible();
      expect(screen.queryByRole('switch', { name: 'Continue without CI' })).not.toBeInTheDocument();
      expect(screen.getByRole('button', { name: 'Add project' })).toBeDisabled();
      expect(
        screen.getByRole('link', { name: 'Open GitHub to change repository access' }),
      ).toHaveAttribute(
        'href',
        'https://github.com/organizations/example-org/settings/installations/1',
      );
      source.replies['/github/checks'] = {
        status: 'discovered',
        default_branch: 'main',
        head: 'abcdef123',
        checks: [
          { name: 'lint', status: 'failed', conclusion: 'failure', run: 2, url: null },
          { name: 'build', status: 'pending', conclusion: null, run: 3, url: null },
        ],
      };
      await user.click(screen.getByRole('button', { name: 'Refresh repository access' }));
      expect(await screen.findByText('lint — failure')).toBeVisible();
      expect(screen.getByText('build — pending')).toBeVisible();
      expect(
        screen.queryByRole('textbox', { name: 'Required check names' }),
      ).not.toBeInTheDocument();
      await user.click(screen.getByRole('switch', { name: 'lint' }));
      await user.click(screen.getByRole('button', { name: 'Add project' }));
      expect(add).toHaveBeenCalledWith({
        name: 'notes',
        repo: 'example-org/notes',
        ci: { expected: ['build'] },
      });
    },
  );

  it('shows loading, ignores an obsolete repository response and requires explicit no CI after confirmed absence', async () => {
    const source = createFixtureSource();
    source.replies['/github'] = { connected: true, organization: 'example-org', install_url: null };
    let resolveOld!: (
      value: Awaited<ReturnType<ReturnType<typeof source.getSnapshot>['discoverChecks']>>,
    ) => void;
    source.set({
      discoverChecks: vi
        .fn()
        .mockImplementationOnce(
          () =>
            new Promise((resolve) => {
              resolveOld = resolve;
            }),
        )
        .mockResolvedValue({
          ok: true,
          value: { status: 'discovered', default_branch: 'main', head: 'new', checks: [] },
        }),
    });
    const { user } = mount(<AddProject />, source);
    await user.type(await screen.findByLabelText('Name'), 'notes');
    fireEvent.change(screen.getByLabelText('Repository'), { target: { value: 'example-org/old' } });
    expect(await screen.findByText('Discovering checks on the default branch…')).toBeVisible();
    expect(screen.getByRole('button', { name: 'Add project' })).toBeDisabled();
    fireEvent.change(screen.getByLabelText('Repository'), { target: { value: 'example-org/new' } });
    const noCi = await screen.findByRole('switch', { name: 'Continue without CI' });
    expect(noCi).not.toBeChecked();
    expect(screen.getByRole('button', { name: 'Add project' })).toBeDisabled();
    await act(async () =>
      resolveOld({
        ok: true,
        value: {
          status: 'discovered',
          default_branch: 'old',
          head: 'old',
          checks: [
            { name: 'obsolete', status: 'success', conclusion: 'success', run: 1, url: null },
          ],
        },
      }),
    );
    expect(screen.queryByRole('switch', { name: 'obsolete' })).not.toBeInTheDocument();
    await user.click(noCi);
    expect(screen.getByRole('button', { name: 'Add project' })).toBeEnabled();
    source.statuses['/github/checks'] = 503;
    source.replies['/github/checks'] = { code: 'INVALID_VALUE', detail: 'Discovery unavailable' };
    // The test override is replaced with the real API method for the failed refresh.
    source.set({
      discoverChecks: createFixtureSource({ fetch: source.fetch }).getSnapshot().discoverChecks,
    });
    await user.click(screen.getByRole('button', { name: 'Refresh checks' }));
    expect(await screen.findByText('Discovery unavailable')).toBeVisible();
    expect(screen.queryByRole('switch', { name: 'Continue without CI' })).not.toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Add project' })).toBeDisabled();
  });

  it('returns to shared setup if discovery reports a missing connection', async () => {
    const source = createFixtureSource();
    source.replies['/github'] = { connected: true, organization: 'example-org', install_url: null };
    const { user } = mount(<AddProject />, source);
    await user.type(await screen.findByLabelText('Name'), 'notes');
    source.replies['/github/checks'] = {
      status: 'failed',
      failure: { code: 'not_connected', message: 'Connect GitHub.', retryable: true },
    };
    source.replies['/github'] = { connected: false, organization: null, install_url: null };
    await user.type(screen.getByLabelText('Repository'), 'example-org/notes');
    await user.click(await screen.findByRole('button', { name: 'Check GitHub connection' }));
    expect(
      await screen.findByRole('heading', { name: 'Connect your GitHub account or organization' }),
    ).toBeVisible();
  });
});
describe('task order and builder outcomes', () => {
  function withTasks(overrides: Record<string, Partial<Task>>) {
    const source = createFixtureSource();
    source.set({
      tasks: source
        .getSnapshot()
        .tasks.map((t) => (overrides[t.id] ? { ...t, ...overrides[t.id] } : t)),
    });
    return source;
  }
  it('shows a waiting task on the board and the goal card as waiting for its prerequisite', () => {
    const { container } = mount(
      <ProjectPage id="harbour" />,
      withTasks({ focus: { state: { state: 'planned' }, waiting_for: ['resume'] } }),
    );
    const goal = container.querySelector('.goal-section') as HTMLElement;
    expect(within(goal).getByText(/Waiting for Keep removed records removed/)).toHaveTextContent(
      'Validate imported records: Waiting for Keep removed records removed',
    );
    const planned = screen.getByRole('region', { name: /^Planned/ });
    expect(
      within(planned).getByText('Waiting for Keep removed records removed'),
    ).toBeInTheDocument();
    expect(screen.queryByText(/Failed/)).not.toBeInTheDocument();
  });
  it('shows partial and blocked outcomes with the builder reason on the goal card', () => {
    const { container } = mount(
      <ProjectPage id="harbour" />,
      withTasks({
        focus: {
          state: { state: 'failed', reason: 'no changes: the run changed nothing' },
          outcome: { status: 'blocked', prerequisite: 'resume' },
        },
        checks: {
          state: { state: 'stopped', was: { state: 'running' } },
          outcome: {
            status: 'partial',
            reason: 'out_of_role',
            detail: 'the API needs a backend task',
          },
        },
        create: {
          state: { state: 'stopped', was: { state: 'in_review' } },
          outcome: { status: 'partial', reason: 'completion_unmet', detail: 'verify passed twice' },
        },
      }),
    );
    const goal = within(container.querySelector('.goal-section') as HTMLElement);
    expect(goal.getByText(/Blocked on: Keep removed records removed/)).toBeInTheDocument();
    expect(
      goal.getByText(/Stopped: out of my role: the API needs a backend task/),
    ).toBeInTheDocument();
    expect(goal.getByText(/Stopped: done-when unmet: verify passed twice/)).toBeInTheDocument();
    expect(screen.queryByText('no changes: the run changed nothing')).not.toBeInTheDocument();
  });
  it('adds no task line to the goal card for complete or unreported outcomes', () => {
    const { container } = mount(
      <ProjectPage id="harbour" />,
      withTasks({ focus: { outcome: { status: 'complete' } } }),
    );
    expect(container.querySelector('.goal-task-note')).toBeNull();
  });
  it('drops a waived done-when outcome once the task goes on to review or merging', () => {
    const waived = { status: 'partial', reason: 'completion_unmet', detail: 'verify passed twice' };
    const { container } = mount(
      <ProjectPage id="harbour" />,
      withTasks({
        focus: { state: { state: 'in_review' }, outcome: waived },
        checks: { state: { state: 'merging' }, outcome: waived },
      } as Record<string, Partial<Task>>),
    );
    expect(screen.queryByText(/done-when unmet/)).not.toBeInTheDocument();
    expect(container.querySelector('.goal-task-note')).toBeNull();
  });
});
describe('goal issues', () => {
  const listing = [
    {
      number: 12,
      title: 'Import fails on empty rows',
      url: 'https://github.com/example-org/harbour/issues/12',
      labels: ['bug', 'dvb'],
      trusted: true,
    },
    {
      number: 14,
      title: 'Please add crypto payments',
      url: 'https://github.com/example-org/harbour/issues/14',
      labels: [],
      trusted: false,
    },
  ];
  function openGoalForm(issues: unknown) {
    const source = createFixtureSource();
    source.set({ goals: [] });
    source.replies['/projects/harbour/issues'] = issues;
    const state = vi.spyOn(source.getSnapshot(), 'stateGoal');
    const mounted = mount(<ProjectPage id="harbour" />, source);
    return { ...mounted, source, state };
  }
  it('attaches a picked trusted issue to the new goal, with or without goal text', async () => {
    const { user, state } = openGoalForm(listing);
    await user.click(screen.getByRole('button', { name: 'Set a goal' }));
    const pick = await screen.findByRole('switch', { name: /#12 Import fails on empty rows/ });
    expect(screen.getByText('bug, dvb')).toBeInTheDocument();
    const save = screen.getByRole('button', { name: 'Save direction' });
    expect(save).toBeDisabled();
    await user.click(pick);
    expect(pick).toBeChecked();
    await user.click(save);
    expect(state).toHaveBeenCalledWith('harbour', { text: '', issues: [12] });
  });
  it('lists an untrusted issue by title and link only, cannot pick it, and shows a refusal by the field', async () => {
    const { user, state, source } = openGoalForm(listing);
    source.statuses['POST /projects/harbour/goals'] = 409;
    source.replies['POST /projects/harbour/goals'] = {
      code: 'ISSUE_UNTRUSTED',
      issue: 12,
      url: listing[0].url,
      detail: 'Issue #12 is no longer trusted.',
    };
    await user.click(screen.getByRole('button', { name: 'Set a goal' }));
    const issues = within(await screen.findByRole('group', { name: 'Attach issues (optional)' }));
    const untrusted = await issues.findByRole('link', { name: 'Please add crypto payments' });
    expect(untrusted).toHaveAttribute('href', listing[1].url);
    expect(issues.getByText(/Can't attach: it was not opened by a member\./)).toBeInTheDocument();
    expect(issues.queryByText(/#14/)).not.toBeInTheDocument();
    expect(issues.getAllByRole('switch')).toHaveLength(1);
    await user.type(screen.getByRole('textbox', { name: 'Goal' }), 'Fix the importer');
    await user.click(issues.getByRole('switch', { name: /#12/ }));
    await user.click(screen.getByRole('button', { name: 'Save direction' }));
    expect(state).toHaveBeenCalledWith('harbour', { text: 'Fix the importer', issues: [12] });
    const form = screen.getByRole('textbox', { name: 'Goal' }).closest('form')!;
    expect(await within(form).findByText('Issue #12 is no longer trusted.')).toBeInTheDocument();
    expect(screen.getByRole('textbox', { name: 'Goal' })).toHaveValue('Fix the importer');
  });
  it('states a goal as before when there are no open issues or they cannot be read', async () => {
    const { user, state, source, unmount } = openGoalForm([]);
    await user.click(screen.getByRole('button', { name: 'Set a goal' }));
    expect(await screen.findByText('No open issues.')).toBeInTheDocument();
    await user.type(screen.getByRole('textbox', { name: 'Goal' }), 'Create an importer');
    await user.click(screen.getByRole('button', { name: 'Save direction' }));
    expect(state).toHaveBeenCalledWith('harbour', { text: 'Create an importer' });
    unmount();
    source.statuses['/projects/harbour/issues'] = 500;
    source.replies['/projects/harbour/issues'] = { code: 'INTERNAL_ERROR', detail: 'GitHub down' };
    const { user: again } = mount(<ProjectPage id="harbour" />, source);
    await again.click(screen.getByRole('button', { name: 'Set a goal' }));
    expect(await screen.findByText(/Open issues could not be loaded/)).toBeInTheDocument();
    await again.type(screen.getByRole('textbox', { name: 'Goal' }), 'Create an importer');
    await again.click(screen.getByRole('button', { name: 'Save direction' }));
    expect(state).toHaveBeenCalledTimes(2);
  });
});
describe('plan on hold', () => {
  // "Not yet" closed the plan question and parked the goal.
  const parkedSource = () => {
    const source = createFixtureSource(),
      s = source.getSnapshot();
    source.set({
      tasks: s.tasks.filter((t) => t.project !== 'harbour'),
      goals: s.goals.map((g) =>
        g.id === 'harbour-goal' ? { ...g, state: 'stated', plan_parked: true } : g,
      ),
      questions: s.questions.map((q) => (q.id === 'local' ? { ...q, open: false } : q)),
    });
    return source;
  };
  it('shows the parked plan and asks its question again on Ask me again', async () => {
    const source = parkedSource(),
      reopened = { ...source.getSnapshot().questions[0], id: 'local-again', open: true };
    source.replies['POST /goals/harbour-goal/resume'] = reopened;
    const resume = vi.spyOn(source.getSnapshot(), 'resumeGoal');
    const { user } = mount(<ProjectPage id="harbour" />, source);
    expect(screen.getByText('Plan on hold')).toBeInTheDocument();
    expect(screen.getByText('The plan waits until you ask for it again.')).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: reopened.subject })).toBeNull();
    expect(needs(source.getSnapshot(), source.getSnapshot().projects[0])).toBe(0);
    await user.click(screen.getByRole('button', { name: 'Ask me again' }));
    expect(resume).toHaveBeenCalledExactlyOnceWith('harbour-goal');
    // The engine's events reopen the same plan question.
    const s = source.getSnapshot();
    act(() =>
      source.set({
        goals: s.goals.map((g) => (g.id === 'harbour-goal' ? { ...g, plan_parked: false } : g)),
        questions: [...s.questions, reopened],
      }),
    );
    expect(screen.queryByText('Plan on hold')).toBeNull();
    expect(screen.queryByRole('button', { name: 'Ask me again' })).toBeNull();
    expect(screen.getByRole('button', { name: reopened.subject })).toBeInTheDocument();
    expect(needs(source.getSnapshot(), source.getSnapshot().projects[0])).toBe(1);
  });
  it('shows a refused resume as one sentence', async () => {
    const source = parkedSource();
    source.replies['POST /goals/harbour-goal/resume'] = {
      code: 'PLAN_NOT_PARKED',
      detail: 'the goal has no plan on hold to resume',
    };
    source.statuses['POST /goals/harbour-goal/resume'] = 409;
    const { user } = mount(<ProjectPage id="harbour" />, source);
    await user.click(screen.getByRole('button', { name: 'Ask me again' }));
    expect(await screen.findByRole('alert')).toHaveTextContent(
      'This plan is no longer on hold, so there is nothing to ask again.',
    );
  });
});
describe('remove project', () => {
  afterEach(() => {
    history.replaceState({}, '', '/');
  });
  const removal = (source: ReturnType<typeof createFixtureSource>) =>
    source.fetch.mock.calls.filter(
      ([url, options]) =>
        String(url).endsWith('/projects/harbour/remove') && options?.method === 'POST',
    );
  it.each(['Cancel', 'Close', 'Escape'])(
    'asks once, naming the project, and %s restores focus without removing anything',
    async (dismissal) => {
      const source = createFixtureSource();
      const { user } = mount(<ProjectPage id="harbour" tab="settings" />, source);
      const remove = screen.getByRole('button', { name: 'Remove' });
      await user.click(remove);
      const dialog = screen.getByRole('dialog', { name: 'Remove Harbour?' });
      expect(dialog).toHaveAccessibleDescription(/^Harbour \(example-org\/harbour\) leaves dvb/);
      expect(dialog).toHaveAccessibleDescription(
        /Stopped tasks' unpublished work on this computer is deleted\.$/,
      );
      if (dismissal === 'Escape') await user.keyboard('{Escape}');
      else await user.click(within(dialog).getByRole('button', { name: dismissal }));
      expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
      await waitFor(() => expect(remove).toHaveFocus());
      expect(removal(source)).toHaveLength(0);
    },
  );
  it("shows the engine's refusal for open tasks and keeps the project", async () => {
    const source = createFixtureSource();
    const detail =
      'Harbour has running or open work: resume "Keep removed records removed" (running): stop it with dvb stop resume';
    source.replies['POST /projects/harbour/remove'] = {
      code: 'PROJECT_BUSY',
      tasks: ['resume'],
      detail,
    };
    source.statuses['POST /projects/harbour/remove'] = 409;
    const { user } = mount(<ProjectPage id="harbour" tab="settings" />, source);
    await user.click(screen.getByRole('button', { name: 'Remove' }));
    const dialog = screen.getByRole('dialog', { name: 'Remove Harbour?' });
    await user.click(within(dialog).getByRole('button', { name: 'Remove Harbour' }));
    expect(await within(dialog).findByRole('alert')).toHaveTextContent(detail);
    expect(removal(source)).toHaveLength(1);
    expect(source.getSnapshot().projects.map((p) => p.id)).toContain('harbour');
    expect(location.hash).toBe('#project/harbour/settings');
  });
  it('removes the project through the API, drops it from the lists and goes Home', async () => {
    const source = createFixtureSource();
    const [harbour] = source.getSnapshot().projects;
    source.replies['POST /projects/harbour/remove'] = { project: harbour, not_freed: [] };
    const { user } = mount(<ProjectPage id="harbour" tab="settings" />, source);
    await user.click(screen.getByRole('button', { name: 'Remove' }));
    await user.click(screen.getByRole('button', { name: 'Remove Harbour' }));
    await waitFor(() => expect(location.hash).toBe('#home'));
    expect(removal(source)).toHaveLength(1);
    expect(source.getSnapshot().projects.map((p) => p.id)).not.toContain('harbour');
  });
});
