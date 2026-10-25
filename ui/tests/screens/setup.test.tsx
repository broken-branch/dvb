import type { components } from '@/api/schema';
import { createFixtureSource } from '../support/render';
import { act, fireEvent } from '@testing-library/react';
import { mount } from '../support/render';
import { describe, expect, it, vi } from 'vitest';
import { screen, within } from '@testing-library/react';
import { Setup, FIRST_GOAL, projectRequest, projectName, continueLabel } from '@/screens/setup';
import { refusalText } from '@/lib/data/display';
describe('first project', () => {
  it('offers the projects dvb has and states the goal without creating one', async () => {
    const { source, user } = mount(<Setup />);
    const state = vi.spyOn(source.getSnapshot(), 'stateGoal'),
      add = vi.spyOn(source.getSnapshot(), 'addProject');
    await user.click(screen.getByRole('button', { name: 'Skip for now' }));
    await user.click(screen.getByRole('button', { name: 'Skip for now' }));
    await user.click(screen.getByRole('button', { name: 'Skip' }));
    expect(screen.getByRole('radio', { name: 'A project dvb already has' })).toBeChecked();
    expect(screen.queryByRole('textbox', { name: 'Repository' })).not.toBeInTheDocument();
    // Nothing is created for a project dvb has: the buttons open it or set its goal.
    expect(screen.queryByRole('button', { name: 'Add project' })).toBeNull();
    expect(screen.queryByRole('button', { name: 'Create project & see plan' })).toBeNull();
    expect(screen.getByRole('button', { name: 'Open project' })).toBeEnabled();
    await user.click(screen.getByRole('button', { name: 'Set goal & see plan' }));
    await vi.waitFor(() => expect(state).toHaveBeenCalledWith('harbour', { text: FIRST_GOAL }));
    expect(add).not.toHaveBeenCalled();
  });
  it('attaches picked issues to the first goal of a project dvb has', async () => {
    const source = createFixtureSource();
    source.replies['/projects/harbour/issues'] = [
      {
        number: 7,
        title: 'Add export',
        url: 'https://github.com/example-org/harbour/issues/7',
        labels: [],
        trusted: true,
      },
    ];
    const { user } = mount(<Setup />, source);
    const state = vi.spyOn(source.getSnapshot(), 'stateGoal');
    await user.click(screen.getByRole('button', { name: 'Skip for now' }));
    await user.click(screen.getByRole('button', { name: 'Skip for now' }));
    await user.click(screen.getByRole('button', { name: 'Skip' }));
    await user.click(await screen.findByRole('switch', { name: /#7 Add export/ }));
    await user.click(screen.getByRole('button', { name: 'Set goal & see plan' }));
    await vi.waitFor(() =>
      expect(state).toHaveBeenCalledWith('harbour', { text: FIRST_GOAL, issues: [7] }),
    );
  });
  it('flags a duplicate name before submitting, and leaves other refusals alone', async () => {
    const { source, user } = mount(<Setup />);
    const add = vi.spyOn(source.getSnapshot(), 'addProject');
    await user.click(screen.getByRole('button', { name: 'Skip for now' }));
    await user.click(screen.getByRole('button', { name: 'Skip for now' }));
    await user.click(screen.getByRole('button', { name: 'Skip' }));
    await user.click(screen.getByRole('radio', { name: 'Use an existing repository' }));
    const repository = screen.getByLabelText('Repository');
    await user.click(repository);
    await user.paste('example-org/exists.foo');
    expect(screen.queryByText(/already has this project/)).not.toBeInTheDocument();
    await user.clear(repository);
    await user.paste('example-org/Harbour');
    expect(screen.getByText(/already has this project/)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Create project & see plan' })).toBeDisabled();
    expect(add).not.toHaveBeenCalled();
    expect(refusalText({ code: 'VALUE_CHANGED', detail: 'name exists.foo is invalid' })).toBe(
      'name exists.foo is invalid',
    );
  });
});
describe('first-run steps', () => {
  it('builds the project request from what the owner typed', () => {
    expect(
      projectRequest('existing', ' example-org/notes/ ', { expected: ['tests', 'build'] }),
    ).toEqual({
      name: 'notes',
      repo: 'example-org/notes',
      ci: { expected: ['tests', 'build'] },
    });
    expect(projectRequest('new', 'example-org/garden', 'no_ci')).toEqual({
      name: 'garden',
      repo: 'example-org/garden',
      create: true,
      ci: 'no_ci',
    });
  });
  it('skips only what is not done', () => {
    expect(continueLabel(true)).toBe('Continue');
    expect(continueLabel(false)).toBe('Skip for now');
  });
  it('shows the install command and connection state for another machine', async () => {
    const { user } = mount(<Setup />);
    await user.click(screen.getByRole('button', { name: 'Back' }));
    expect(screen.queryByText(/install\.sh/)).not.toBeInTheDocument();
    await user.click(screen.getByRole('radio', { name: /Another Linux machine/ }));
    expect(screen.getByText(/current GitHub release/)).toBeInTheDocument();
    expect(screen.getByText('sudo sh install.sh --engine')).toBeInTheDocument();
    expect(
      screen.getByText('sudo sh install.sh --cockpit --engine-host user@machine'),
    ).toBeInTheDocument();
    expect(screen.queryByText(/systemctl/)).not.toBeInTheDocument();
    await user.type(screen.getByLabelText('Machine address'), 'me@box');
    expect(
      screen.getByText('sudo sh install.sh --cockpit --engine-host me@box'),
    ).toBeInTheDocument();
    expect(screen.getByText('Connected')).toBeInTheDocument();
  });
  it('marks sign-in done from the engine and walks to a stated goal', async () => {
    const { source, user } = mount(<Setup />);
    const state = vi.spyOn(source.getSnapshot(), 'stateGoal'),
      add = vi.spyOn(source.getSnapshot(), 'addProject');
    expect(screen.getAllByRole('button', { name: 'Sign in' })).toHaveLength(2);
    await user.click(screen.getByRole('button', { name: 'Skip for now' }));
    expect(screen.getAllByRole('heading', { level: 1 })).toHaveLength(1);
    expect(
      screen.getByRole('heading', {
        level: 2,
        name: /Connect your GitHub account or organization/,
      }),
    ).toBeInTheDocument();
    // Opening GitHub is the step's primary action; skipping it is not.
    expect(
      await screen.findByRole('button', { name: "Open GitHub to create dvb's App" }),
    ).toHaveAttribute('data-variant', 'default');
    expect(screen.getByRole('button', { name: 'Skip for now' })).toHaveAttribute(
      'data-variant',
      'outline',
    );
    await user.click(screen.getByRole('button', { name: 'Skip for now' }));
    expect(screen.getByText('1 of 4. Projects')).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Skip' }));
    expect(screen.getByLabelText('First goal')).toHaveValue(FIRST_GOAL);
    source.replies['/github'] = { connected: true, organization: 'example-org', install_url: null };
    await user.click(screen.getByRole('radio', { name: 'Use an existing repository' }));
    await user.click(screen.getByLabelText('Repository'));
    await user.paste('example-org/notes');
    source.replies['/github'] = { connected: true, organization: 'example-org', install_url: null };
    await vi.waitFor(() => expect(screen.getByRole('switch', { name: 'tests' })).toBeChecked());
    await user.click(screen.getByRole('button', { name: 'Create project & see plan' }));
    await vi.waitFor(() =>
      expect(state).toHaveBeenCalledWith(expect.stringMatching(/^project-/), { text: FIRST_GOAL }),
    );
    expect(add).toHaveBeenCalledWith({
      name: 'notes',
      repo: 'example-org/notes',
      ci: { expected: ['tests'] },
    });
  });
});

it('continues from Sign in when every needed vendor is signed in', async () => {
  const source = createFixtureSource();
  source.replies['/logins'] = { claude: true, codex: false, needed: ['claude'] };
  const { user } = mount(<Setup />, source);
  await user.click(await screen.findByRole('button', { name: 'Continue' }));
  expect(
    screen.getByRole('heading', { level: 2, name: /Connect your GitHub account or organization/ }),
  ).toBeInTheDocument();
  expect(screen.getByText('Sign in').closest('li')).toHaveClass('complete');
});

it('Enter finishes sign-in and keeps the setup step', async () => {
  const source = createFixtureSource();
  const finish = vi.spyOn(source.getSnapshot(), 'finishLogin');
  const { user } = mount(<Setup />, source);
  await user.click(screen.getAllByRole('button', { name: 'Sign in' })[0]);
  source.replies['POST /logins/claude'] = {
    status: 'logged_in',
    run: 'login',
    url: '',
    prompt: 'Signed in',
  } satisfies components['schemas']['LoginStarted'];
  await user.type(
    screen.getByRole('textbox', { name: 'Code from the sign-in page' }),
    'code{Enter}',
  );
  expect(finish).toHaveBeenCalledWith('claude', { run: 'login', code: 'code' });
  expect(
    screen.getByRole('heading', { name: 'Sign in to your subscriptions' }),
  ).toBeInTheDocument();
});

it('Enter in GitHub fields creates the App and keeps the wizard on GitHub', async () => {
  const source = createFixtureSource();
  source.replies['POST /github/start'] = {
    create_url: 'https://github.com/organizations/example-org/settings/apps/new',
    manifest: '{"public":false}',
  } satisfies components['schemas']['GithubManifest'];
  const start = vi.spyOn(source.getSnapshot(), 'startGithub');
  vi.spyOn(window, 'open').mockReturnValue({ close: vi.fn() } as unknown as Window);
  vi.spyOn(HTMLFormElement.prototype, 'submit').mockImplementation(() => {});
  const { user } = mount(<Setup />, source);
  await user.click(screen.getByRole('button', { name: 'Skip for now' }));
  fireEvent.change(screen.getByRole('textbox', { name: 'GitHub account or organization' }), {
    target: { value: 'example-org' },
  });
  fireEvent.change(screen.getByRole('textbox', { name: 'Your name' }), {
    target: { value: 'Example Owner' },
  });
  const email = screen.getByRole('textbox', { name: 'Your email' });
  fireEvent.change(email, { target: { value: 'owner@example.invalid' } });
  email.focus();
  await user.keyboard('{Enter}');
  expect(start).toHaveBeenCalledWith({
    organization: 'example-org',
    owner_name: 'Example Owner',
    owner_email: 'owner@example.invalid',
  });
  expect(
    screen.getByRole('heading', { name: /Connect your GitHub account or organization/ }),
  ).toBeInTheDocument();
});

it('rechecks GitHub at first-project submission and retains the repository, check choice and goal through installation', async () => {
  const source = createFixtureSource();
  const { user } = mount(<Setup />, source);
  const add = vi.spyOn(source.getSnapshot(), 'addProject');
  await user.click(screen.getByRole('button', { name: 'Skip for now' }));
  await user.click(screen.getByRole('button', { name: 'Skip for now' }));
  await user.click(screen.getByRole('button', { name: 'Skip' }));
  source.replies['/github'] = { connected: true, organization: 'example-org', install_url: null };
  await user.click(screen.getByRole('radio', { name: 'Use an existing repository' }));
  fireEvent.change(screen.getByLabelText('Repository'), { target: { value: 'example-org/notes' } });
  fireEvent.change(screen.getByLabelText('First goal'), { target: { value: 'Keep this goal' } });
  expect(await screen.findByRole('switch', { name: 'tests' })).toBeChecked();
  source.replies['/github'] = { connected: false, organization: null, install_url: null };
  vi.useFakeTimers();
  await act(async () => {
    fireEvent.click(screen.getByRole('button', { name: 'Create project & see plan' }));
  });
  expect(
    screen.getByRole('heading', { name: 'Connect your GitHub account or organization' }),
  ).toBeVisible();
  expect(add).not.toHaveBeenCalled();
  source.statuses['/github'] = 503;
  source.replies['/github'] = {
    code: 'INVALID_VALUE',
    detail: 'Connection temporarily unavailable',
  };
  await act(async () => {
    await vi.advanceTimersByTimeAsync(3000);
  });
  source.statuses['/github'] = 200;
  source.replies['/github'] = { connected: true, organization: 'example-org', install_url: null };
  await act(async () => {
    fireEvent.click(screen.getByRole('button', { name: 'Retry GitHub status' }));
  });
  expect(screen.getByLabelText('Repository')).toHaveValue('example-org/notes');
  expect(screen.getByLabelText('First goal')).toHaveValue('Keep this goal');
  expect(screen.getByRole('switch', { name: 'tests' })).toBeChecked();
  expect(screen.getByRole('button', { name: 'Create project & see plan' })).toBeEnabled();
});

describe('step bar', () => {
  const stepIn = (name: string) =>
    within(document.querySelector<HTMLElement>('.setup-steps')!)
      .getAllByRole('listitem')
      .find((li) => li.textContent?.includes(name))!;
  const skippedStep = (name: string) => {
    const item = stepIn(name);
    expect(item).toHaveClass('skipped');
    expect(item).not.toHaveClass('complete');
    expect(item.querySelector('svg')).toBeNull();
    expect(item).toHaveTextContent('Skipped');
  };
  const doneStep = (name: string) => {
    const item = stepIn(name);
    expect(item).toHaveClass('complete');
    expect(item.querySelector('svg')).not.toBeNull();
    expect(item).not.toHaveTextContent('Skipped');
  };

  it('opens on Sign in with Machine done when an engine is connected', async () => {
    const { user } = mount(<Setup />);
    expect(stepIn('Sign in')).toHaveClass('active');
    doneStep('Machine');
    await user.click(screen.getByRole('button', { name: 'Back' }));
    expect(stepIn('Machine')).toHaveClass('active');
    expect(screen.getByRole('radio', { name: /This machine/ })).toBeChecked();
  });

  it('opens on Machine when no engine is connected', () => {
    const source = createFixtureSource();
    source.set({ connection: { state: 'reconnecting' } });
    mount(<Setup />, source);
    expect(stepIn('Machine')).toHaveClass('active');
    expect(screen.getByRole('button', { name: 'Cancel' })).toBeInTheDocument();
    expect(screen.getByText('Where should the work happen?')).toBeInTheDocument();
  });

  it('moves to Sign in with Machine done when the engine connects after setup opens', async () => {
    const source = createFixtureSource();
    source.set({ connection: { state: 'reconnecting' } });
    const { user } = mount(<Setup />, source);
    expect(stepIn('Machine')).toHaveClass('active');
    act(() => source.set({ connection: { state: 'connected' } }));
    expect(stepIn('Sign in')).toHaveClass('active');
    doneStep('Machine');
    await user.click(screen.getByRole('button', { name: 'Back' }));
    act(() => source.set({ connection: { state: 'reconnecting' } }));
    act(() => source.set({ connection: { state: 'connected' } }));
    expect(stepIn('Machine')).toHaveClass('active');
  });

  it('marks skipped Sign in and GitHub as skipped, not done', async () => {
    const { user } = mount(<Setup />);
    await user.click(screen.getByRole('button', { name: 'Skip for now' }));
    await user.click(screen.getByRole('button', { name: 'Skip for now' }));
    doneStep('Machine');
    skippedStep('Sign in');
    skippedStep('GitHub');
    expect(stepIn('Sign in').querySelector('span')).toHaveTextContent('2');
  });

  it('marks a signed-in or connected step passed with Continue as done', async () => {
    const source = createFixtureSource();
    source.replies['/logins'] = { claude: true, codex: true };
    source.replies['/github'] = { connected: true, organization: 'example-org', install_url: null };
    const { user } = mount(<Setup />, source);
    await user.click(await screen.findByRole('button', { name: 'Continue' }));
    await user.click(await screen.findByRole('button', { name: 'Continue' }));
    doneStep('Sign in');
    doneStep('GitHub');
  });

  it('opens each next step at the top of the page', async () => {
    const { user } = mount(<Setup />);
    await user.click(screen.getByRole('button', { name: 'Skip for now' }));
    const scroll = vi.spyOn(window, 'scrollTo');
    await user.click(screen.getByRole('button', { name: 'Skip for now' }));
    expect(scroll).toHaveBeenCalledWith({ top: 0 });
    scroll.mockClear();
    await user.click(screen.getByRole('button', { name: 'Skip' }));
    expect(scroll).toHaveBeenCalledWith({ top: 0 });
  });

  it('turns a skipped step done after going back and finishing it', async () => {
    const source = createFixtureSource();
    const { user } = mount(<Setup />, source);
    await user.click(screen.getByRole('button', { name: 'Skip for now' }));
    await user.click(screen.getByRole('button', { name: 'Skip for now' }));
    skippedStep('GitHub');
    source.replies['/github'] = { connected: true, organization: 'example-org', install_url: null };
    await user.click(screen.getByRole('button', { name: 'Back' }));
    expect(await screen.findByRole('button', { name: 'Continue' })).toHaveAttribute(
      'data-variant',
      'default',
    );
    await user.click(screen.getByRole('button', { name: 'Continue' }));
    doneStep('GitHub');
    skippedStep('Sign in');
  });
});

describe('first project without a goal and repository names', () => {
  async function openExisting(kind = 'Use an existing repository') {
    const source = createFixtureSource();
    source.replies['/github/repositories'] = {
      organization: 'example-org',
      repository_access_url:
        'https://github.com/organizations/example-org/settings/installations/1',
      repositories: [
        { repo: 'example-org/notes', private: true, default_branch: 'main' },
        { repo: 'example-org/garden', private: true, default_branch: 'main' },
        { repo: 'other-org/garden', private: true, default_branch: 'main' },
      ],
    } satisfies components['schemas']['GithubRepositories'];
    const add = vi.spyOn(source.getSnapshot(), 'addProject'),
      state = vi.spyOn(source.getSnapshot(), 'stateGoal');
    const { user } = mount(<Setup />, source);
    await user.click(screen.getByRole('button', { name: 'Skip for now' }));
    await user.click(screen.getByRole('button', { name: 'Skip for now' }));
    await user.click(screen.getByRole('button', { name: 'Skip' }));
    source.replies['/github'] = { connected: true, organization: 'example-org', install_url: null };
    await user.click(screen.getByRole('radio', { name: kind }));
    await screen.findByText(/^example-org\/notes/);
    return { source, user, add, state };
  }
  const repository = () => screen.getByLabelText('Repository');

  it('adds the project without stating a goal, even with the goal empty, and opens it', async () => {
    const { user, add, state } = await openExisting();
    fireEvent.change(repository(), { target: { value: 'example-org/notes' } });
    await user.clear(screen.getByLabelText('First goal'));
    await vi.waitFor(() => expect(screen.getByRole('switch', { name: 'tests' })).toBeChecked());
    expect(screen.getByRole('button', { name: 'Create project & see plan' })).toBeDisabled();
    await user.click(screen.getByRole('button', { name: 'Add project' }));
    await vi.waitFor(() =>
      expect(add).toHaveBeenCalledWith({
        name: 'notes',
        repo: 'example-org/notes',
        ci: { expected: ['tests'] },
      }),
    );
    await vi.waitFor(() => expect(location.hash).toMatch(/^#project\/project-/));
    expect(state).not.toHaveBeenCalled();
  });

  it('opens a project dvb already has without a goal', async () => {
    const { source, user } = mount(<Setup />);
    const state = vi.spyOn(source.getSnapshot(), 'stateGoal');
    await user.click(screen.getByRole('button', { name: 'Skip for now' }));
    await user.click(screen.getByRole('button', { name: 'Skip for now' }));
    await user.click(screen.getByRole('button', { name: 'Skip' }));
    await user.click(screen.getByRole('button', { name: 'Open project' }));
    expect(location.hash).toBe('#project/harbour');
    expect(state).not.toHaveBeenCalled();
  });

  it('fills the field from a picked repository', async () => {
    const { user } = await openExisting();
    await user.click(screen.getByRole('button', { name: 'Use example-org/notes' }));
    expect(repository()).toHaveValue('example-org/notes');
  });

  it('resolves a bare name with one reachable match and states the goal with it', async () => {
    const { user, add, state } = await openExisting();
    fireEvent.change(repository(), { target: { value: 'notes' } });
    expect(screen.getByText('Repository: example-org/notes')).toHaveAttribute('role', 'status');
    await vi.waitFor(() => expect(screen.getByRole('switch', { name: 'tests' })).toBeChecked());
    await user.click(screen.getByRole('button', { name: 'Create project & see plan' }));
    await vi.waitFor(() =>
      expect(state).toHaveBeenCalledWith(expect.stringMatching(/^project-/), { text: FIRST_GOAL }),
    );
    expect(add).toHaveBeenCalledWith(
      expect.objectContaining({ name: 'notes', repo: 'example-org/notes' }),
    );
  });

  it('refuses a bare name with no match or several matches once the field is left', async () => {
    const { add } = await openExisting();
    fireEvent.change(repository(), { target: { value: 'example' } });
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Add project' })).toBeDisabled();
    fireEvent.blur(repository());
    expect(repository()).toHaveAccessibleDescription(/No reachable repository/);
    expect(screen.getByRole('alert')).toHaveTextContent(
      'No reachable repository is named example. Enter it with its owner, for example example-org/example.',
    );
    expect(screen.getByRole('button', { name: 'Add project' })).toBeDisabled();
    expect(screen.getByRole('button', { name: 'Create project & see plan' })).toBeDisabled();
    fireEvent.change(repository(), { target: { value: 'garden' } });
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
    fireEvent.blur(repository());
    expect(screen.getByRole('alert')).toHaveTextContent(
      'Several reachable repositories are named garden: example-org/garden, other-org/garden.',
    );
    expect(screen.getByRole('button', { name: 'Add project' })).toBeDisabled();
    expect(add).not.toHaveBeenCalled();
  });

  it('refuses an unresolved bare name on Enter without leaving the field', async () => {
    const { user, add } = await openExisting();
    await user.type(repository(), 'example');
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
    await user.keyboard('{Enter}');
    expect(repository()).toHaveFocus();
    expect(repository()).toHaveAttribute('aria-invalid', 'true');
    expect(repository()).toHaveAccessibleDescription(/No reachable repository/);
    expect(screen.getByRole('alert')).toHaveTextContent(
      'No reachable repository is named example. Enter it with its owner, for example example-org/example.',
    );
    await user.clear(repository());
    await user.type(repository(), 'garden');
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
    await user.keyboard('{Enter}');
    expect(repository()).toHaveFocus();
    expect(repository()).toHaveAccessibleDescription(/Several reachable repositories/);
    expect(screen.getByRole('alert')).toHaveTextContent(
      'Several reachable repositories are named garden: example-org/garden, other-org/garden.',
    );
    expect(add).not.toHaveBeenCalled();
  });

  it('defaults a new repository to No CI yet and sends no_ci', async () => {
    const { user, add } = await openExisting('Create a private repository');
    fireEvent.change(repository(), { target: { value: 'example-org/fresh' } });
    expect(screen.getByText(/A new repository has no CI yet/)).toHaveTextContent(
      'dvb project checks',
    );
    expect(screen.getByRole('switch', { name: 'No CI yet' })).toBeChecked();
    expect(screen.queryByRole('textbox', { name: 'Required check names' })).not.toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Add project' }));
    await vi.waitFor(() =>
      expect(add).toHaveBeenCalledWith({
        name: 'fresh',
        repo: 'example-org/fresh',
        create: true,
        ci: 'no_ci',
      }),
    );
  });

  it('sends named checks for a new repository only once a name is typed', async () => {
    const { user, add } = await openExisting('Create a private repository');
    fireEvent.change(repository(), { target: { value: 'example-org/fresh' } });
    await user.click(screen.getByRole('switch', { name: 'No CI yet' }));
    const names = screen.getByRole('textbox', { name: 'Required check names' });
    expect(screen.getByRole('button', { name: 'Add project' })).toBeDisabled();
    await user.type(names, 'tests, build');
    await user.click(screen.getByRole('button', { name: 'Add project' }));
    await vi.waitFor(() =>
      expect(add).toHaveBeenCalledWith({
        name: 'fresh',
        repo: 'example-org/fresh',
        create: true,
        ci: { expected: ['tests', 'build'] },
      }),
    );
  });

  it('offers Use buttons only for an existing repository', async () => {
    const { user } = await openExisting('Create a private repository');
    expect(screen.getByText(/^example-org\/notes/)).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: /^Use example-org\// })).not.toBeInTheDocument();
    await user.type(repository(), 'notes{Enter}');
    expect(screen.getByRole('alert')).toHaveTextContent(
      'Enter it with its owner, for example example-org/notes.',
    );
    await user.click(screen.getByRole('radio', { name: 'Use an existing repository' }));
    expect(screen.getByRole('button', { name: 'Use example-org/notes' })).toBeInTheDocument();
  });

  it.each([
    ['user', false],
    ['organization', true],
  ] as const)('on a %s account offers creating a repository: %s', async (account_type, offered) => {
    const source = createFixtureSource();
    source.replies['/github'] = {
      connected: true,
      organization: 'example-owner',
      account_type,
      install_url: null,
    };
    const { user } = mount(<Setup />, source);
    await user.click(screen.getByRole('button', { name: 'Skip for now' }));
    await screen.findByText(/^Connected to example-owner/);
    await user.click(screen.getByRole('button', { name: 'Continue' }));
    await user.click(screen.getByRole('button', { name: 'Skip' }));
    expect(screen.getByRole('radio', { name: 'Use an existing repository' })).toBeVisible();
    const refusal =
      'dvb cannot create a repository in a personal GitHub account; create it on GitHub, give the dvb App access to it, then add it here.';
    if (offered) {
      expect(screen.getByRole('radio', { name: /Create a private repository/ })).toBeVisible();
      expect(screen.queryByText(refusal)).not.toBeInTheDocument();
    } else {
      expect(
        screen.queryByRole('radio', { name: /Create a private repository/ }),
      ).not.toBeInTheDocument();
      expect(screen.getByText(refusal)).toBeVisible();
    }
  });

  it('refuses a bare name for a new repository with the organization suggestion', async () => {
    await openExisting('Create a private repository');
    fireEvent.change(repository(), { target: { value: 'notes' } });
    fireEvent.blur(repository());
    expect(screen.getByRole('alert')).toHaveTextContent(
      'Enter it with its owner, for example example-org/notes.',
    );
    expect(screen.getByRole('button', { name: 'Add project' })).toBeDisabled();
  });
});

describe('work from the open issues', () => {
  const found = {
    open: 8,
    trusted: 5,
    labels: [{ label: 'enhancement', open: 3 }],
  } satisfies components['schemas']['IssueCounts'];
  async function openStep(source = createFixtureSource()) {
    const propose = vi.spyOn(source.getSnapshot(), 'proposeGoal'),
      state = vi.spyOn(source.getSnapshot(), 'stateGoal'),
      add = vi.spyOn(source.getSnapshot(), 'addProject');
    const { user } = mount(<Setup />, source);
    await user.click(screen.getByRole('button', { name: 'Skip for now' }));
    await user.click(screen.getByRole('button', { name: 'Skip for now' }));
    await user.click(screen.getByRole('button', { name: 'Skip' }));
    return { source, user, propose, state, add };
  }

  it('shows what dvb found and starts the proposal for a project dvb has', async () => {
    const source = createFixtureSource();
    source.replies['/projects/harbour/issues/counts'] = found;
    source.replies['POST /projects/harbour/proposal'] = found;
    const { user, propose, state } = await openStep(source);
    expect(
      await screen.findByText('Open issues found: 8 (5 trusted, 3 labelled enhancement)'),
    ).toHaveAttribute('role', 'status');
    expect(screen.getByLabelText('First goal')).toHaveValue('');
    expect(screen.getByRole('button', { name: 'Set goal & see plan' })).toBeDisabled();
    await user.click(screen.getByRole('button', { name: 'Work from the open issues' }));
    await vi.waitFor(() => expect(location.hash).toBe('#project/harbour'));
    expect(propose).toHaveBeenCalledWith('harbour');
    expect(state).not.toHaveBeenCalled();
  });

  it('offers it only when trusted issues exist', async () => {
    const source = createFixtureSource();
    source.replies['/projects/harbour/issues/counts'] = { open: 2, trusted: 0, labels: [] };
    await openStep(source);
    expect(await screen.findByText('Open issues found: 2 (0 trusted)')).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Work from the open issues' })).toBeNull();
    expect(screen.getByLabelText('First goal')).toHaveValue(FIRST_GOAL);
    expect(screen.getByRole('button', { name: 'Set goal & see plan' })).toBeEnabled();
  });

  it('keeps stating a goal next to it', async () => {
    const source = createFixtureSource();
    source.replies['/projects/harbour/issues/counts'] = found;
    const { user, propose, state } = await openStep(source);
    await screen.findByRole('button', { name: 'Work from the open issues' });
    await user.type(screen.getByLabelText('First goal'), 'Add export');
    await user.click(screen.getByRole('button', { name: 'Set goal & see plan' }));
    await vi.waitFor(() => expect(state).toHaveBeenCalledWith('harbour', { text: 'Add export' }));
    expect(propose).not.toHaveBeenCalled();
  });

  it('shows a refused proposal and stays on the step', async () => {
    const source = createFixtureSource();
    source.replies['/projects/harbour/issues/counts'] = found;
    source.statuses['POST /projects/harbour/proposal'] = 409;
    source.replies['POST /projects/harbour/proposal'] = {
      code: 'VALUE_CHANGED',
      detail: 'A goal is already stated in harbour',
    };
    location.hash = '';
    const { user } = await openStep(source);
    await user.click(await screen.findByRole('button', { name: 'Work from the open issues' }));
    expect(await screen.findByRole('alert')).toHaveTextContent(
      'A goal is already stated in harbour',
    );
    expect(location.hash).toBe('');
  });

  it('shows what dvb found after adding a repository and offers its issues', async () => {
    const source = createFixtureSource();
    source.replies['POST /projects'] = {
      ...(source.replies['POST /projects'] as object),
      issues_found: found,
    };
    source.replies['POST /projects/project-example/proposal'] = found;
    location.hash = '';
    const { user, propose, add, state } = await openStep(source);
    source.replies['/github'] = { connected: true, organization: 'example-org', install_url: null };
    await user.click(screen.getByRole('radio', { name: 'Use an existing repository' }));
    fireEvent.change(screen.getByLabelText('Repository'), {
      target: { value: 'example-org/notes' },
    });
    await vi.waitFor(() => expect(screen.getByRole('switch', { name: 'tests' })).toBeChecked());
    await user.click(screen.getByRole('button', { name: 'Add project' }));
    expect(
      await screen.findByText('Open issues found: 8 (5 trusted, 3 labelled enhancement)'),
    ).toBeInTheDocument();
    expect(add).toHaveBeenCalledTimes(1);
    expect(location.hash).toBe('');
    expect(screen.getByRole('radio', { name: 'A project dvb already has' })).toBeChecked();
    await user.click(screen.getByRole('button', { name: 'Work from the open issues' }));
    await vi.waitFor(() => expect(location.hash).toBe('#project/project-example'));
    expect(propose).toHaveBeenCalledWith('project-example');
    expect(add).toHaveBeenCalledTimes(1);
    expect(state).not.toHaveBeenCalled();
  });

  it('shows what dvb found after adding a repository with only untrusted issues', async () => {
    const source = createFixtureSource();
    source.replies['POST /projects'] = {
      ...(source.replies['POST /projects'] as object),
      issues_found: { open: 1, trusted: 0, labels: [] },
    };
    location.hash = '';
    const { user, propose } = await openStep(source);
    source.replies['/github'] = { connected: true, organization: 'example-org', install_url: null };
    await user.click(screen.getByRole('radio', { name: 'Use an existing repository' }));
    fireEvent.change(screen.getByLabelText('Repository'), {
      target: { value: 'example-org/notes' },
    });
    await vi.waitFor(() => expect(screen.getByRole('switch', { name: 'tests' })).toBeChecked());
    await user.click(screen.getByRole('button', { name: 'Add project' }));
    expect(await screen.findByText('Open issues found: 1 (0 trusted)')).toHaveAttribute(
      'role',
      'status',
    );
    expect(location.hash).toBe('');
    expect(screen.getByRole('radio', { name: 'A project dvb already has' })).toBeChecked();
    expect(screen.queryByRole('button', { name: 'Work from the open issues' })).toBeNull();
    expect(screen.getByLabelText('First goal')).toHaveValue(FIRST_GOAL);
    await user.click(screen.getByRole('button', { name: 'Open project' }));
    expect(location.hash).toBe('#project/project-example');
    expect(propose).not.toHaveBeenCalled();
  });

  it('opens a repository added without open issues as before', async () => {
    const source = createFixtureSource();
    source.replies['POST /projects'] = {
      ...(source.replies['POST /projects'] as object),
      issues_found: { open: 0, trusted: 0, labels: [] },
    };
    location.hash = '';
    const { user, propose } = await openStep(source);
    source.replies['/github'] = { connected: true, organization: 'example-org', install_url: null };
    await user.click(screen.getByRole('radio', { name: 'Use an existing repository' }));
    fireEvent.change(screen.getByLabelText('Repository'), {
      target: { value: 'example-org/notes' },
    });
    await vi.waitFor(() => expect(screen.getByRole('switch', { name: 'tests' })).toBeChecked());
    await user.click(screen.getByRole('button', { name: 'Add project' }));
    await vi.waitFor(() => expect(location.hash).toBe('#project/project-example'));
    expect(propose).not.toHaveBeenCalled();
  });

  it('hides the pre-filled goal and disables Create while the counts load', async () => {
    const source = createFixtureSource();
    let answer: (counts: components['schemas']['IssueCounts']) => void = () => {};
    source.set({
      issueCounts: () =>
        new Promise((resolve) => {
          answer = (value) => resolve({ ok: true, value });
        }),
    });
    await openStep(source);
    expect(screen.getByLabelText('First goal')).toHaveValue('');
    expect(screen.getByText('Reading the open issues…')).toHaveAttribute('role', 'status');
    expect(screen.getByRole('button', { name: 'Set goal & see plan' })).toBeDisabled();
    await act(async () => answer({ open: 0, trusted: 0, labels: [] }));
    expect(screen.getByLabelText('First goal')).toHaveValue(FIRST_GOAL);
    expect(screen.queryByText('Reading the open issues…')).toBeNull();
    expect(screen.getByRole('button', { name: 'Set goal & see plan' })).toBeEnabled();
  });

  it('restores the pre-filled goal when the counts cannot be read', async () => {
    const source = createFixtureSource();
    let fail: () => void = () => {};
    source.set({
      issueCounts: () =>
        new Promise((_, reject) => {
          fail = () => reject(new Error('Unable to connect.'));
        }),
    });
    await openStep(source);
    expect(screen.getByRole('button', { name: 'Set goal & see plan' })).toBeDisabled();
    await act(async () => fail());
    expect(screen.getByLabelText('First goal')).toHaveValue(FIRST_GOAL);
    expect(screen.getByRole('button', { name: 'Set goal & see plan' })).toBeEnabled();
  });
});

describe('project names from the repository', () => {
  async function openRepository(repo: string) {
    const source = createFixtureSource();
    const add = vi.spyOn(source.getSnapshot(), 'addProject');
    const { user } = mount(<Setup />, source);
    await user.click(screen.getByRole('button', { name: 'Skip for now' }));
    await user.click(screen.getByRole('button', { name: 'Skip for now' }));
    await user.click(screen.getByRole('button', { name: 'Skip' }));
    source.replies['/github'] = { connected: true, organization: 'example-org', install_url: null };
    await user.click(screen.getByRole('radio', { name: 'Use an existing repository' }));
    fireEvent.change(screen.getByLabelText('Repository'), { target: { value: repo } });
    await vi.waitFor(() => expect(screen.getByRole('switch', { name: 'tests' })).toBeChecked());
    return { source, user, add };
  }

  it('derives a name the engine accepts', () => {
    expect(projectName('/srv/git/fieldnotes.git')).toBe('fieldnotes');
    expect(projectName('owner/owner.github.io')).toBe('owner-github-io');
    expect(projectName('owner/site.v2/')).toBe('site-v2');
    expect(projectName('example-org/my_repo-2')).toBe('my_repo-2');
  });

  it('adds a dotted GitHub repository with dashes in its name', async () => {
    const { user, add } = await openRepository('example-org/example-org.github.io');
    await user.click(screen.getByRole('button', { name: 'Add project' }));
    await vi.waitFor(() =>
      expect(add).toHaveBeenCalledWith(
        expect.objectContaining({
          name: 'example-org-github-io',
          repo: 'example-org/example-org.github.io',
        }),
      ),
    );
  });

  it('flags a duplicate by the derived name', async () => {
    await openRepository('example-org/Harbour');
    expect(screen.getByText(/already has this project/)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Add project' })).toBeDisabled();
  });

  it('explains a refused name next to the field and clears it when the repository changes', async () => {
    const { source, user } = await openRepository('example-org/.git');
    source.statuses['POST /projects'] = 400;
    source.replies['POST /projects'] = {
      code: 'INVALID_PROJECT_NAME',
      detail: 'bad project name ""',
    };
    location.hash = '';
    await user.click(screen.getByRole('button', { name: 'Add project' }));
    const repository = screen.getByLabelText('Repository');
    await vi.waitFor(() =>
      expect(repository).toHaveAccessibleDescription(
        'dvb cannot name the project after this repository. A project name can use only letters, digits, - and _.',
      ),
    );
    expect(repository).toHaveAttribute('aria-invalid', 'true');
    expect(screen.queryByText(/bad project name/)).toBeNull();
    expect(location.hash).toBe('');
    fireEvent.change(repository, { target: { value: 'example-org/fieldnotes' } });
    expect(screen.queryByText(/A project name can use only/)).toBeNull();
    expect(repository).toHaveAttribute('aria-invalid', 'false');
  });

  it('shows other refusals as they are, even with name-like wording', async () => {
    const { source, user } = await openRepository('example-org/.git');
    source.statuses['POST /projects'] = 400;
    source.replies['POST /projects'] = { code: 'INVALID_VALUE', detail: 'bad project name ""' };
    await user.click(screen.getByRole('button', { name: 'Add project' }));
    expect(await screen.findByText(/bad project name/)).toBeInTheDocument();
    const repository = screen.getByLabelText('Repository');
    expect(repository).toHaveAttribute('aria-invalid', 'false');
    expect(screen.queryByText(/A project name can use only/)).toBeNull();
  });
});
