import { project, goal, task, event } from '../support/rows';
import { WorkspaceInfo } from '@/components/dvb/workspace-info';
import { TaskBoard } from '@/components/dvb/task-board';
import { beforeEach } from 'vitest';
import { needs, railGroups, railStatus, sortProjects } from '@/lib/data/work';
import type { Goal, DataStore, Question, Task } from '@/lib/data/types';
import { mount } from '../support/render';
import { it, expect, vi, describe } from 'vitest';
import { act, screen, fireEvent, within } from '@testing-library/react';
import { Home, Projects, ProjectCard } from '@/screens/home';
import { createFixtureSource } from '../support/render';
import { ProjectPage } from '@/screens/project';
import { stateLabels } from '@/lib/data/display';
import { activeTaskStates } from '@/lib/data/selectors';
beforeEach(() => vi.spyOn(Date, 'now').mockReturnValue(1790811300000));
describe('actions', () => {
  it('Home lists real open questions and stopped tasks, then removes answered questions', async () => {
    const { source } = mount(<Home />);
    const needs = within(document.getElementById('needs-you')!);
    expect(needs.getByRole('link', { name: /Import one catalogue/ })).toHaveAttribute(
      'href',
      '#decision/local',
    );
    expect(needs.getByRole('link', { name: /Which map format/ })).toHaveAttribute(
      'href',
      '#decision/atlas-format',
    );
    await act(async () => {
      const answered = await source.getSnapshot().answer('local', { choice: 1 });
      if (answered.ok)
        source.set({ questions: [answered.value, source.getSnapshot().questions[1]] });
    });
    expect(needs.queryByRole('link', { name: /Import one catalogue/ })).not.toBeInTheDocument();
  });
});
describe('overview', () => {
  it('overview row has only its link in the Tab order and still handles row clicks', async () => {
    const { user } = mount(<Projects />);
    const row = document.querySelector('.overview-table tbody tr')!;
    expect(row).not.toHaveAttribute('tabindex');
    const link = within(row as HTMLElement).getByRole('link');
    link.focus();
    expect(link).toHaveFocus();
    await user.tab();
    expect(row).not.toHaveFocus();
    fireEvent.click(row.querySelectorAll('td')[1]);
    expect(location.hash).toBe(link.getAttribute('href'));
  });
});
describe('question closure', () => {
  it('closed engine projection clears Needs you without an owner answer', async () => {
    const source = createFixtureSource();
    const q = source.getSnapshot().questions[0];
    mount(<Home />, source);
    expect(screen.getByText(q.subject)).toBeInTheDocument();
    const closed = {
      id: 9000,
      at: 1,
      actor: 'engine' as const,
      project: q.project,
      kind: 'question.closed' as const,
      payload: { question: q.id, task: 'finished' },
    };
    act(() =>
      source.set({
        questions: [{ ...q, open: false }, source.getSnapshot().questions[1]],
        events: [...source.getSnapshot().events, closed],
      }),
    );
    expect(screen.queryByText(q.subject)).not.toBeInTheDocument();
    expect(source.getSnapshot().questions.find((question) => question.id === q.id)).toMatchObject({
      open: false,
    });
    expect(
      source.getSnapshot().questions.find((question) => question.id === q.id)?.answer,
    ).toBeUndefined();
    expect(source.getSnapshot().events).toContainEqual(closed);
  });
});
describe('questions combinations', () => {
  const homeQuestionCases = Array.from(
    { length: 16 },
    (_, bits) =>
      [
        bits & 3,
        bits & 4 ? ('reconnecting' as const) : ('connected' as const),
        !!(bits & 8),
      ] as const,
  );
  it.each(homeQuestionCases)(
    'no Home toggle or filter combination hides an open question from Needs you [%i %s paused=%s]',
    (mask, connection, paused) => {
      vi.spyOn(Date, 'now').mockReturnValue(1_800_000_000_000);
      const source = createFixtureSource(),
        base = source.getSnapshot(),
        q = base.questions[0];
      const questions = [
        q,
        { ...q, id: 'fn1', project: 'fieldnotes', subject: 'Fieldnotes question one?' },
        { ...q, id: 'fn2', project: 'fieldnotes', subject: 'Fieldnotes question two?' },
      ];
      const toggles = ['notify_faults', 'notify_completed'] as const;
      const values = {
        ...base.settings.values,
        ...Object.fromEntries(toggles.map((k, i) => [k, !!((mask >> i) & 1)])),
      };
      source.set({
        questions,
        settings: { values },
        connection: { ...base.connection, state: connection },
        projects: base.projects.map((p) => ({ ...p, paused })),
      });
      mount(<Home />, source);
      const needs = document.getElementById('needs-you')!,
        links = within(needs).getAllByRole('link');
      for (const x of questions) {
        const link = links.find((link) => link.getAttribute('href') === `#decision/${x.id}`);
        expect(link, `${mask} ${connection} ${paused}: ${x.id}`).toBeInTheDocument();
        expect(link).toHaveTextContent(x.subject);
        expect(link).toHaveAccessibleName(new RegExp(x.subject.replace('?', '\\?')));
        expect(link).toHaveAttribute('href', `#decision/${x.id}`);
      }
    },
  );
});
describe('questions', () => {
  it('Needs you lists exactly the open questions across projects and drops each one as it is answered', async () => {
    const source = createFixtureSource(),
      s = source.getSnapshot();
    const project = (id: string) => ({ ...s.projects[0], id, name: id, repo: `example-org/${id}` });
    const question = (id: string, proj: string, open: boolean) => ({
      id,
      project: proj,
      subject: `Subject ${id}`,
      format: 'quick_compare' as const,
      recommended: 1,
      open,
      options: [
        { id: 1, text: `Yes ${id}`, effect: 'Go.', approves_plan: false },
        { id: 2, text: `No ${id}`, effect: 'Stop.', approves_plan: false },
      ],
    });
    const qs = [
      question('qa', 'alpha', true),
      question('qb', 'beta', true),
      question('qc', 'gamma', true),
      question('qd', 'gamma', true),
      question('qx', 'alpha', false),
    ];
    source.set({
      projects: ['alpha', 'beta', 'gamma'].map(project),
      goals: [],
      tasks: [],
      events: [],
      questions: qs,
      status: { running: 0, needs_you: 4 },
    });
    mount(<Home />, source);
    const listed = () =>
      Array.from(document.querySelectorAll('.attention-section a[data-needs]')).map((a) =>
        a.getAttribute('href'),
      );
    expect(listed()).toEqual(['#decision/qa', '#decision/qb', '#decision/qc', '#decision/qd']);
    expect(screen.queryByText('Subject qx')).not.toBeInTheDocument();
    expect(document.querySelector('.attention-section .section-count')).not.toBeInTheDocument();
    const remaining = ['qa', 'qb', 'qc', 'qd'];
    for (const id of ['qb', 'qa', 'qd', 'qc']) {
      await act(async () => {
        source.set({ questions: qs.filter((q) => q.id !== id && remaining.includes(q.id)) });
      });
      remaining.splice(remaining.indexOf(id), 1);
      expect(listed()).toEqual(remaining.map((r) => `#decision/${r}`));
      expect(document.querySelectorAll('[data-needs]')).toHaveLength(remaining.length);
    }
    expect(document.querySelector('.attention-section')).not.toBeInTheDocument();
  });
});
describe('snapshots', () => {
  function source(patch: Partial<DataStore> = {}) {
    const s = createFixtureSource();
    s.set({
      goals: [],
      tasks: [],
      questions: [],
      hires: [],
      roles: {},
      events: [],
      evidence: [],
      activity: {},
      status: { running: 0, needs_you: 0 },
      projects: [project],
      settings: { values: {} },
      connection: { state: 'connected' },
      ...patch,
    });
    return s;
  }
  /** The engine's status listing the task, folding its question when it names one. */
  const waiting = (t: Task, question?: string): DataStore['status'] => ({
    running: 0,
    needs_you: 1,
    tasks: [{ id: t.id, project: t.project, title: t.title, state: t.state, question }],
  });
  describe('authoritative project state', () => {
    it.each(['done', 'failed', 'dropped'] as const)(
      'shows the last %s goal in both views without task counts or goal actions',
      (state) => {
        const s = source({
          goals: [goal('last', state)],
          tasks: [task('old', 'last', { state: 'done' })],
        });
        const { rerender } = mount(<ProjectCard project={project} />, s);
        expect(screen.getByText('No finished work yet')).toBeInTheDocument();
        expect(screen.queryByText(/tasks done/)).not.toBeInTheDocument();
        rerender(<ProjectPage id="p" />);
        expect(screen.getByText(`Last goal ${state}`)).toBeInTheDocument();
        expect(screen.getByRole('heading', { level: 2, name: 'Goal last' })).toBeInTheDocument();
        expect(screen.queryByText('tasks completed')).not.toBeInTheDocument();
        expect(screen.queryByRole('button', { name: 'Change course' })).not.toBeInTheDocument();
        expect(screen.queryByRole('button', { name: 'Drop goal' })).not.toBeInTheDocument();
        expect(screen.getByRole('button', { name: 'Set a new goal' })).toBeInTheDocument();
      },
    );
    it('selects a current goal even when a later terminal goal exists, and counts only its tasks', () => {
      const s = source({
        goals: [goal('current', 'in_progress'), goal('old', 'done')],
        activity: { p: { latest: 1, fresh: 0, goals: ['old', 'current'] } },
        tasks: [
          task('one', 'current', { state: 'done' }),
          task('two', 'current', { state: 'in_review', head: 'abc' }),
          task('three', 'old', { state: 'done' }),
        ],
      });
      const { rerender } = mount(<ProjectCard project={project} />, s);
      expect(screen.getByText('Goal current')).toBeInTheDocument();
      expect(screen.getByText('1 of 2 tasks')).toBeInTheDocument();
      expect(screen.getByText('In review', { selector: '.status-label' })).toBeInTheDocument();
      rerender(<ProjectPage id="p" />);
      expect(screen.getByText('Goal current')).toBeInTheDocument();
      expect(document.querySelector('.progress-fraction')).toHaveTextContent('1 / 2');
    });
    describe('a goal that ended with a failed task', () => {
      const ended = (patch: { goal?: Goal['state']; broken?: Task['state'] } = {}) =>
        source({
          goals: [goal('g', patch.goal ?? 'failed')],
          tasks: [
            task('merged', 'g', { state: 'done' }),
            task('broken', 'g', patch.broken ?? { state: 'failed', reason: 'merging failed' }),
          ],
        });
      it('stays in Needs you, says the goal failed and links the task', () => {
        const s = ended(),
          now = Date.now();
        s.set({ status: waiting(s.getSnapshot().tasks[1]) });
        const { rerender } = mount(<Home />, s);
        const attention = within(document.querySelector('.attention-section') as HTMLElement);
        expect(attention.getByRole('link', { name: /Task broken/ })).toHaveAttribute(
          'href',
          '#task/broken',
        );
        const card = within(document.querySelector('.now-card') as HTMLElement);
        expect(card.getByText('Goal failed', { selector: '.status-label' })).toBeInTheDocument();
        expect(card.getByText('Goal g')).toBeInTheDocument();
        expect(card.getByText('Failed or stopped task: Task broken')).toBeInTheDocument();
        expect(card.queryByText(/Ready|Finished/)).not.toBeInTheDocument();
        const data = s.getSnapshot();
        expect(needs(data, project)).toBe(1);
        expect(railStatus(data, project, now)).toBe('Goal failed');
        expect(railGroups(data, now).map((g) => g.label)).toEqual(['Needs you']);
        rerender(<ProjectPage id="p" />);
        const goalCard = within(document.querySelector('.goal-section') as HTMLElement);
        expect(goalCard.getByText('Last goal failed')).toBeInTheDocument();
        expect(goalCard.getByRole('heading', { level: 2, name: 'Goal g' })).toBeInTheDocument();
        expect(goalCard.getByRole('link', { name: 'Task broken' })).toHaveAttribute(
          'href',
          '#task/broken',
        );
        expect(goalCard.queryByRole('link', { name: 'Task merged' })).not.toBeInTheDocument();
        expect(screen.getByRole('button', { name: 'Drop goal' })).toBeInTheDocument();
      });
      it.each([
        ['retried', ended({ goal: 'in_progress', broken: { state: 'planned' } })],
        ['dropped', ended({ goal: 'dropped' })],
      ])('leaves Needs you once the task is %s', (_, s) => {
        mount(<Home />, s);
        expect(document.querySelector('.attention-section')).toBeNull();
        expect(screen.queryByText('Goal failed')).not.toBeInTheDocument();
        expect(needs(s.getSnapshot(), project)).toBe(0);
        expect(railStatus(s.getSnapshot(), project, Date.now())).not.toBe('Goal failed');
      });
      it.each(['stated', 'done'] as const)(
        'leaves Needs you once a later goal replaces it (%s)',
        (state) => {
          const s = source({
            goals: [goal('g', 'failed'), goal('next', state)],
            tasks: [task('broken', 'g', { state: 'failed', reason: 'merging failed' })],
            activity: { p: { latest: 0, fresh: 0, goals: ['g', 'next'] } },
          });
          const { rerender } = mount(<Home />, s);
          expect(document.querySelector('.attention-section')).toBeNull();
          expect(screen.queryByText('Goal failed')).not.toBeInTheDocument();
          expect(needs(s.getSnapshot(), project)).toBe(0);
          expect(railStatus(s.getSnapshot(), project, Date.now())).not.toBe('Goal failed');
          rerender(<ProjectPage id="p" />);
          expect(screen.queryByText('Last goal failed')).not.toBeInTheDocument();
          expect(screen.queryByRole('heading', { name: 'Goal g' })).not.toBeInTheDocument();
        },
      );
    });
    describe('a stopped task', () => {
      const stopped = task('held', 'g', { state: 'stopped', was: { state: 'planned' } });
      const question = (id: string): Question => ({
        id,
        project: 'p',
        task: 'held',
        subject: `Question ${id}`,
        format: 'simple',
        open: true,
        recommended: 2,
        options: [
          { id: 1, text: 'Retry it', effect: 'Runs the task again.', retries_task: 'held' },
          { id: 2, text: 'Leave it stopped', effect: 'Nothing runs.', no_effect: true },
        ],
      });
      const listed = () =>
        Array.from(document.querySelectorAll('.attention-section a[data-needs]')).map((a) =>
          a.getAttribute('href'),
        );
      it('counts and lists once with its leave-stopped question, which the row opens', () => {
        const s = source({
          goals: [goal('g', 'in_progress')],
          tasks: [stopped],
          questions: [question('leave')],
          status: waiting(stopped, 'leave'),
        });
        mount(<Home />, s);
        expect(listed()).toEqual(['#decision/leave/task/held']);
        const row = within(document.querySelector('.attention-section') as HTMLElement);
        expect(row.getByRole('link', { name: /Task held.*Stopped.*Answer/ })).toBeInTheDocument();
        expect(row.queryByText('Question leave')).not.toBeInTheDocument();
        expect(needs(s.getSnapshot(), project)).toBe(1);
      });
      it('counts and lists once with no question', () => {
        const s = source({
          goals: [goal('g', 'in_progress')],
          tasks: [stopped],
          status: waiting(stopped),
        });
        mount(<Home />, s);
        expect(listed()).toEqual(['#task/held']);
        expect(needs(s.getSnapshot(), project)).toBe(1);
      });
    });
    it.each([...activeTaskStates])(
      'uses the same active set for the card and At work: %s',
      (state) => {
        const s = source({
          goals: [goal('g', 'approved')],
          tasks: [task('active', 'g', { state } as Task['state'])],
          status: { running: 7, needs_you: 2 },
        });
        mount(<Home />, s);
        expect(
          screen.getByText(stateLabels[state], { selector: '.project-card .status-label' }),
        ).toBeInTheDocument();
        expect(
          within(document.querySelector('.now-card') as HTMLElement).getByText('Task active'),
        ).toBeInTheDocument();
        expect(screen.queryByText('7 tasks active')).not.toBeInTheDocument();
      },
    );
    it('ignores output and incidental events in new badges', () => {
      const events = [
        event(1, 'project.seen', {}),
        event(2, 'run.output', { run: 'r', item: { text: 'noise' } }),
        event(3, 'question.opened', {
          question: 'q',
          subject: 'Question',
          options: [],
          recommended: 1,
          format: 'simple',
          lapses_on: { value_version: 1 },
        }),
        event(4, 'task.done', { task: 't' }),
        event(5, 'goal.done', { goal: 'g' }),
        event(6, 'run.ended', { run: 'r', reason: 'completed' }),
      ];
      mount(<Home />, source({ events }));
      expect(screen.queryByText('3 new')).not.toBeInTheDocument();
    });
  });
});
it('shows Last worked on only when idle with the newest three projects, independent of feed', () => {
  const source = createFixtureSource();
  mount(<Home />, source);
  expect(screen.getByText('Working now')).toBeInTheDocument();
  expect(screen.queryByText('Last worked on')).not.toBeInTheDocument();
  act(() => source.set({ tasks: [], events: [] }));
  expect(screen.getByText('Last worked on')).toBeInTheDocument();
  expect(
    [...document.querySelectorAll('.now-card .project-label')].map((e) => e.textContent),
  ).toHaveLength(3);
  expect([...document.querySelectorAll('.now-card a')].map((e) => e.getAttribute('href'))).toEqual([
    '#project/harbour',
    '#project/fieldnotes',
    '#project/atlas',
  ]);
  expect(screen.queryByText('Working now')).not.toBeInTheDocument();
});
it('sorts Overview newest first, allows alternate and direction sorting, and expands quiet projects', async () => {
  const source = createFixtureSource(),
    { user } = mount(<Projects />, source);
  const order = () =>
    [...document.querySelectorAll('.overview-table tbody tr td:first-child a')].map((e) =>
      e.getAttribute('href'),
    );
  expect(order()).toEqual([
    '#project/harbour',
    '#project/fieldnotes',
    '#project/atlas',
    '#project/ledger',
  ]);
  expect(screen.getByRole('columnheader', { name: /Last work/ })).toHaveAttribute(
    'aria-sort',
    'descending',
  );
  await user.click(screen.getByRole('button', { name: 'Project', exact: true }));
  expect(order()).toEqual([
    '#project/atlas',
    '#project/fieldnotes',
    '#project/harbour',
    '#project/ledger',
  ]);
  expect(screen.getByRole('columnheader', { name: /Project/ })).toHaveAttribute(
    'aria-sort',
    'ascending',
  );
  await user.click(screen.getByRole('button', { name: 'Project ↑' }));
  expect(order()[0]).toBe('#project/ledger');
  await user.click(screen.getAllByRole('button', { name: /Quiet \(4\)/ })[0]);
  expect(order()).toHaveLength(8);
  const s = source.getSnapshot();
  expect(sortProjects(s, s.projects, 'Goals finished', true, Date.now())[0].name).toBe('Archive');
});

it('gives Now a page heading with existing projects', () => {
  mount(<Home />);
  expect(screen.getByRole('heading', { level: 1, name: 'Now' })).toBeInTheDocument();
});

it('uses singular task and attention wording and separates CI from review', () => {
  const source = createFixtureSource();
  const data = source.getSnapshot();
  source.set({
    projects: [{ ...data.projects[0], ci: { expected: ['tests'] } }],
    tasks: [{ ...data.tasks[0], state: { state: 'running' } }],
    status: { running: 1, needs_you: 1 },
  });
  mount(
    <>
      <WorkspaceInfo />
      <TaskBoard project="harbour" />
      <Home />
    </>,
    source,
  );
  expect(screen.getByRole('link', { name: '1 needs you' })).toBeInTheDocument();
  expect(document.querySelector('.board-caption')).toHaveTextContent('1 task');
  expect(document.querySelector('.board-caption')).not.toHaveTextContent('1 tasks');
  expect(screen.getByText('CI · Review required')).toBeInTheDocument();
  expect(screen.getByText('0 of 1 task')).toBeInTheDocument();
});
describe('waiting tasks', () => {
  it('Now shows a waiting task as waiting for its prerequisite, not as failed', () => {
    const source = createFixtureSource();
    source.set({
      tasks: source
        .getSnapshot()
        .tasks.map((t) =>
          t.id === 'focus'
            ? { ...t, state: { state: 'planned' }, waiting_for: ['resume', 'gone'] }
            : t,
        ),
    });
    mount(<Home />, source);
    expect(
      screen.getByText(
        'Validate imported records: Waiting for Keep removed records removed and gone',
      ),
    ).toBeInTheDocument();
    expect(
      within(document.getElementById('needs-you')!).queryByText(/Validate imported/),
    ).toBeNull();
  });
  it('Now shows no waiting line when no task waits', () => {
    const { container } = mount(<Home />);
    expect(container.querySelector('.now-waiting')).toBeNull();
  });
});
describe('setup link', () => {
  const connected = {
    connected: true,
    organization: 'example-org',
    install_url: null,
    repository_access_url: null,
    create_app: { status: 'succeeded' as const },
    install_app: { status: 'succeeded' as const },
  };
  it('points at setup while GitHub is not connected', async () => {
    const { user } = mount(<Home />);
    await user.click(await screen.findByRole('link', { name: 'Set up dvb' }));
    expect(location.hash).toBe('#setup');
  });
  it('points at setup on an empty Home next to the first-goal card', async () => {
    const source = createFixtureSource();
    source.set({ projects: [] });
    mount(<Home />, source);
    expect(screen.getByText('Ready for a first goal')).toBeInTheDocument();
    expect(await screen.findByRole('link', { name: 'Set up dvb' })).toHaveAttribute(
      'href',
      '#setup',
    );
  });
  it('shows no link once GitHub is connected, nor while the status loads', async () => {
    const source = createFixtureSource();
    source.replies['/github'] = connected;
    const github = vi.spyOn(source.getSnapshot(), 'github');
    mount(<Home />, source);
    expect(screen.queryByRole('link', { name: 'Set up dvb' })).not.toBeInTheDocument();
    await vi.waitFor(() => expect(github).toHaveBeenCalled());
    await act(() => github.mock.results[0].value);
    expect(screen.queryByRole('link', { name: 'Set up dvb' })).not.toBeInTheDocument();
  });
  it('shows the link when the GitHub status cannot be read', async () => {
    const source = createFixtureSource();
    source.statuses['/github'] = 503;
    source.replies['/github'] = {
      code: 'INVALID_VALUE',
      detail: 'Connection temporarily unavailable',
    };
    mount(<Home />, source);
    expect(await screen.findByRole('link', { name: 'Set up dvb' })).toHaveAttribute(
      'href',
      '#setup',
    );
  });
});
