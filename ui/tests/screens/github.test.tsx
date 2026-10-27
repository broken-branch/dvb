import { json } from '../support/api';
import { mount, createFixtureSource } from '../support/render';
import { describe, it, expect, vi } from 'vitest';
import { screen, waitFor, act, fireEvent } from '@testing-library/react';
import { GithubRepositories, GithubSetup } from '@/screens/github';
import type { components } from '@/api/schema';

const createApp = "Open GitHub to create dvb's App",
  installApp = "Open GitHub to install dvb's App";
describe('GitHub wizard', () => {
  it('shows the desk’s plain-text 409 setup error and closes the creation window', async () => {
    vi.useFakeTimers();
    const message =
      'GitHub setup could not start because dvb or GitHub did not answer; retry in a moment.';
    const fetcher = vi.fn<typeof fetch>().mockImplementation(async (url) => {
      if (url === '/api/github')
        return Response.json({
          connected: false,
          organization: null,
          install_url: null,
          repository_access_url: null,
          create_app: { status: 'not_started' },
          install_app: { status: 'not_started' },
        } satisfies components['schemas']['GithubConnection']);
      if (url === '/github/start')
        return new Response(message, {
          status: 409,
          headers: { 'Content-Type': 'text/plain; charset=utf-8' },
        });
      throw new Error('Unexpected setup request');
    });
    const close = vi.fn();
    vi.spyOn(window, 'open').mockReturnValue({ close } as unknown as Window);
    const submit = vi.spyOn(HTMLFormElement.prototype, 'submit');
    mount(<GithubSetup />, createFixtureSource({ fetch: fetcher }));
    await act(async () => {});
    fireEvent.change(screen.getByRole('textbox', { name: 'GitHub account or organization' }), {
      target: { value: 'example-org' },
    });
    fireEvent.change(screen.getByRole('textbox', { name: 'Your name' }), {
      target: { value: 'Example Owner' },
    });
    fireEvent.change(screen.getByRole('textbox', { name: 'Your email' }), {
      target: { value: 'owner@example.invalid' },
    });
    await act(async () => {
      fireEvent.click(screen.getByRole('button', { name: createApp }));
    });
    expect(screen.getByText(message)).toBeVisible();
    expect(close).toHaveBeenCalledOnce();
    expect(submit).not.toHaveBeenCalled();
    expect(screen.getByRole('button', { name: createApp })).toBeEnabled();
  });
  it('uses the authenticated desk to prepare the manifest and posts only the manifest to GitHub', async () => {
    const fetcher = vi.fn<typeof fetch>().mockImplementation(async (url, options) => {
      if (url === '/api/github')
        return new Response(
          JSON.stringify({ connected: false, organization: null, install_url: null }),
          { status: 200 },
        );
      if (url === '/github/start') {
        expect(JSON.parse(options?.body as string)).toEqual({
          organization: 'example-org',
          owner_name: 'Example Owner',
          owner_email: 'owner@example.invalid',
        });
        return new Response(
          JSON.stringify({
            create_url:
              'https://github.com/organizations/example-org/settings/apps/new?state=opaque',
            manifest: '{"public":false}',
          }),
          { status: 200 },
        );
      }
      throw new Error('Unexpected setup request');
    });
    vi.spyOn(window, 'open').mockReturnValue({ close: vi.fn() } as unknown as Window);
    let posted = false;
    vi.spyOn(HTMLFormElement.prototype, 'submit').mockImplementation(function (
      this: HTMLFormElement,
    ) {
      posted = true;
      expect(this.action).toBe(
        'https://github.com/organizations/example-org/settings/apps/new?state=opaque',
      );
      expect(this.method).toBe('post');
      const data = new FormData(this);
      expect([...data.entries()]).toEqual([['manifest', '{"public":false}']]);
    });
    const { user } = mount(<GithubSetup />, createFixtureSource({ fetch: fetcher }));
    await user.clear(screen.getByRole('textbox', { name: 'GitHub account or organization' }));
    await user.type(
      screen.getByRole('textbox', { name: 'GitHub account or organization' }),
      'example-org',
    );
    await user.type(screen.getByRole('textbox', { name: 'Your name' }), 'Example Owner');
    await user.type(screen.getByRole('textbox', { name: 'Your email' }), 'owner@example.invalid');
    await user.click(screen.getByRole('button', { name: createApp }));
    await waitFor(() => expect(posted).toBe(true));
    expect(fetcher).toHaveBeenCalledWith(
      '/github/start',
      expect.objectContaining({ credentials: 'same-origin', method: 'POST' }),
    );
  });
  it('resumes a created App at the install step after the callback returns', async () => {
    const fetcher = vi.fn<typeof fetch>().mockResolvedValue(
      new Response(
        JSON.stringify({
          connected: false,
          organization: 'example-org',
          install_url: 'https://example.invalid/apps/fixture/installations/new?state=opaque',
        }),
        { status: 200 },
      ),
    );
    mount(<GithubSetup />, createFixtureSource({ fetch: fetcher }));
    expect(await screen.findByRole('button', { name: installApp })).toBeEnabled();
    expect(screen.getByText('App created')).toBeVisible();
    expect(screen.queryByRole('button', { name: createApp })).not.toBeInTheDocument();
    expect(screen.queryByRole('textbox', { name: 'Your name' })).not.toBeInTheDocument();
  });
});
describe('personal accounts', () => {
  const start = (create_url: string) => {
    const fetcher = vi.fn<typeof fetch>().mockImplementation(async (url) => {
      if (url === '/api/github')
        return Response.json({ connected: false, organization: null, install_url: null });
      if (url === '/github/start')
        return Response.json({ create_url, manifest: '{"public":false}' });
      throw new Error('Unexpected setup request');
    });
    const close = vi.fn();
    vi.spyOn(window, 'open').mockReturnValue({ close } as unknown as Window);
    const submit = vi.spyOn(HTMLFormElement.prototype, 'submit').mockImplementation(() => {});
    return { fetcher, close, submit };
  };
  const fill = async (user: ReturnType<typeof mount>['user']) => {
    await user.type(
      screen.getByRole('textbox', { name: 'GitHub account or organization' }),
      'example-user',
    );
    await user.type(screen.getByRole('textbox', { name: 'Your name' }), 'Example Owner');
    await user.type(screen.getByRole('textbox', { name: 'Your email' }), 'owner@example.invalid');
    await user.click(screen.getByRole('button', { name: createApp }));
  };
  it('takes a personal username and posts the manifest to GitHub’s personal App page', async () => {
    const { fetcher, submit } = start('https://github.com/settings/apps/new?state=opaque');
    const { user } = mount(<GithubSetup />, createFixtureSource({ fetch: fetcher }));
    expect(
      screen.getByRole('heading', { name: 'Connect your GitHub account or organization' }),
    ).toBeVisible();
    await fill(user);
    await waitFor(() => expect(submit).toHaveBeenCalledOnce());
    expect((submit.mock.contexts[0] as HTMLFormElement).action).toBe(
      'https://github.com/settings/apps/new?state=opaque',
    );
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
  });
  it('shows the engine’s one-sentence refusal for an unknown account', async () => {
    const detail = 'GitHub has no personal account or organization named example-user.';
    const fetcher = vi.fn<typeof fetch>().mockImplementation(async (url) => {
      if (url === '/api/github')
        return Response.json({ connected: false, organization: null, install_url: null });
      if (url === '/github/start')
        return Response.json({ code: 'INVALID_VALUE', detail }, { status: 409 });
      throw new Error('Unexpected setup request');
    });
    vi.spyOn(window, 'open').mockReturnValue({ close: vi.fn() } as unknown as Window);
    const { user } = mount(<GithubSetup />, createFixtureSource({ fetch: fetcher }));
    await fill(user);
    expect(await screen.findByText(detail)).toBeVisible();
  });
  it.each([
    ['user', 'example-user', '2. Install on your account'],
    ['organization', 'example-org', '2. Install on the organization'],
  ] as const)('names the %s the App installs on', async (account_type, organization, step) => {
    const source = createFixtureSource();
    source.replies['/github'] = {
      connected: false,
      organization,
      account_type,
      install_url: 'https://github.com/apps/fixture/installations/new?state=opaque',
      repository_access_url: null,
      create_app: { status: 'succeeded' },
      install_app: { status: 'not_started' },
    } satisfies components['schemas']['GithubConnection'];
    mount(<GithubSetup />, source);
    expect(await screen.findByRole('heading', { name: step })).toBeVisible();
  });
});

describe('polling', () => {
  it('starts with no organization, stops polling after connection and aborts on close', async () => {
    vi.useFakeTimers();
    const fetcher = vi
      .fn()
      .mockResolvedValueOnce({
        ok: true,
        status: 200,
        headers: new Headers({ 'dvb-version': '0.1.0' }),
        json: async () => ({ connected: false, organization: null, install_url: null }),
      })
      .mockResolvedValue({
        ok: true,
        status: 200,
        headers: new Headers({ 'dvb-version': '0.1.0' }),
        json: async () => ({ connected: true, organization: 'chosen', install_url: null }),
      });
    const pollingFetch = vi.fn<typeof fetch>((url, options) =>
      url === '/api/github/repositories'
        ? Promise.resolve(
            Response.json({
              organization: 'chosen',
              repository_access_url: 'https://github.com/settings/installations/1',
              repositories: [],
            }),
          )
        : fetcher(url, options),
    );
    const view = mount(<GithubSetup />, createFixtureSource({ fetch: pollingFetch }));
    expect(screen.getByRole('textbox', { name: 'GitHub account or organization' })).toHaveValue('');
    await act(async () => {});
    await act(async () => {
      await vi.advanceTimersByTimeAsync(3000);
    });
    expect(fetcher.mock.calls.filter(([url]) => url === '/api/github')).toHaveLength(2);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(12000);
    });
    expect(fetcher.mock.calls.filter(([url]) => url === '/api/github')).toHaveLength(2);
    view.unmount();
    expect(fetcher.mock.calls[0][1].signal.aborted).toBe(true);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(6000);
    });
    expect(fetcher.mock.calls.filter(([url]) => url === '/api/github')).toHaveLength(2);
  });
  it('a connected screen makes no further polling requests', async () => {
    vi.useFakeTimers();
    const fetcher = vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      headers: new Headers({ 'dvb-version': '0.1.0' }),
      json: async () => ({ connected: true, organization: 'chosen', install_url: null }),
    });
    const pollingFetch = vi.fn<typeof fetch>((url, options) =>
      url === '/api/github/repositories'
        ? Promise.resolve(
            Response.json({
              organization: 'chosen',
              repository_access_url: 'https://github.com/settings/installations/1',
              repositories: [],
            }),
          )
        : fetcher(url, options),
    );
    const view = mount(<GithubSetup />, createFixtureSource({ fetch: pollingFetch }));
    await act(async () => {});
    expect(fetcher.mock.calls.filter(([url]) => url === '/api/github')).toHaveLength(1);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(15000);
    });
    expect(fetcher.mock.calls.filter(([url]) => url === '/api/github')).toHaveLength(1);
    view.unmount();
    expect(fetcher.mock.calls[0][1].signal.aborted).toBe(true);
  });
});

it('checks the GitHub response version and blocks manifest creation on a mismatch', async () => {
  const fetcher = vi
    .fn<typeof fetch>()
    .mockResolvedValue(json({ connected: false, organization: null, install_url: null }, '2'));
  const source = createFixtureSource({ fetch: fetcher });
  mount(<GithubSetup />, source);
  await waitFor(() => expect(source.getSnapshot().connection.state).toBe('version-mismatch'));
  const result = await source.getSnapshot().startGithub({
    organization: 'example-org',
    owner_name: 'Example Owner',
    owner_email: 'owner@example.invalid',
  });
  expect(result).toMatchObject({ ok: false, refusal: { code: 'VERSION_MISMATCH' } });
  expect(fetcher.mock.calls.filter(([url]) => url === '/api/github')).toHaveLength(1);
  expect(screen.getByRole('button', { name: createApp })).toBeDisabled();
});

it('keeps the original page when the browser blocks the creation popup', async () => {
  const source = createFixtureSource();
  vi.spyOn(window, 'open').mockReturnValue(null);
  const start = vi.spyOn(source.getSnapshot(), 'startGithub');
  const { user } = mount(<GithubSetup />, source);
  await user.type(screen.getByLabelText('GitHub account or organization'), 'example-org');
  await user.type(screen.getByLabelText('Your name'), 'Owner');
  await user.type(screen.getByLabelText('Your email'), 'owner@example.invalid');
  await user.click(screen.getByRole('button', { name: createApp }));
  expect(await screen.findByRole('alert')).toHaveTextContent('Allow popups');
  expect(start).not.toHaveBeenCalled();
  expect(screen.getByLabelText('GitHub account or organization')).toHaveValue('example-org');
});

it('explains installation failure and retries through the authenticated desk without recreating the App', async () => {
  const source = createFixtureSource();
  source.replies['/github'] = {
    connected: false,
    organization: 'example-org',
    install_url: null,
    create_app: { status: 'succeeded' },
    install_app: {
      status: 'failed',
      failure: {
        code: 'callback_expired',
        message: 'Installation expired. Retry Install App.',
        retryable: true,
      },
    },
  };
  source.replies['POST /github/install'] = {
    ...(source.replies['/github'] as object),
    install_url: 'https://github.com/apps/example/installations/new?state=fresh',
    install_app: { status: 'awaiting_callback' },
  };
  const external = { close: vi.fn(), location: { href: '' } };
  vi.spyOn(window, 'open').mockReturnValue(external as unknown as Window);
  const start = vi.spyOn(source.getSnapshot(), 'startGithub');
  const { user } = mount(<GithubSetup />, source);
  expect(await screen.findByRole('alert')).toHaveTextContent('Installation expired');
  await user.click(screen.getByRole('button', { name: installApp }));
  expect(external.location.href).toContain('state=fresh');
  expect(source.fetch).toHaveBeenCalledWith(
    '/github/install',
    expect.objectContaining({ method: 'POST', credentials: 'same-origin' }),
  );
  expect(start).not.toHaveBeenCalled();
});

const created = {
  connected: false,
  organization: 'example-org',
  install_url: 'https://github.com/apps/example/installations/new?state=old',
  create_app: { status: 'succeeded' },
  install_app: { status: 'not_started' },
};

it.each([
  ['Create App', 'popup'],
  ['Install App', 'popup'],
  ['Create App', 'request'],
  ['Install App', 'request'],
] as const)(
  'retains %s %s failure guidance across successful status polls until retry',
  async (action, failure) => {
    vi.useFakeTimers();
    const source = createFixtureSource();
    const installing = action === 'Install App';
    const connection = {
      connected: false,
      organization: 'example-org',
      install_url: installing ? 'https://github.com/apps/example/installations/new' : null,
      repository_access_url: null,
      create_app: { status: installing ? 'succeeded' : 'not_started' },
      install_app: { status: installing ? 'awaiting_callback' : 'not_started' },
    };
    source.replies['/github'] = connection;
    const endpoint = installing ? 'POST /github/install' : 'POST /github/start';
    source.statuses[endpoint] = 503;
    source.replies[endpoint] = { code: 'INVALID_VALUE', detail: 'Setup request unavailable' };
    const external = { close: vi.fn(), location: { href: '' } };
    const open = vi
      .spyOn(window, 'open')
      .mockReturnValue(failure === 'popup' ? null : (external as unknown as Window));
    vi.spyOn(HTMLFormElement.prototype, 'submit').mockImplementation(() => {});
    mount(<GithubSetup />, source);
    await act(async () => {});
    if (!installing) {
      for (const [label, value] of [
        ['GitHub account or organization', 'example-org'],
        ['Your name', 'Owner'],
        ['Your email', 'owner@example.invalid'],
      ])
        fireEvent.change(screen.getByLabelText(label), { target: { value } });
    }
    // A pending installation offers to open GitHub again.
    const button = installing ? 'Open GitHub again' : createApp;
    await act(async () => {
      fireEvent.click(screen.getByRole('button', { name: button }));
    });
    const message =
      failure === 'popup'
        ? `Allow popups for dvb, then retry ${action}.`
        : 'Setup request unavailable';
    expect(screen.getByRole('alert')).toHaveTextContent(message);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(6000);
    });
    expect(source.fetch.mock.calls.filter(([url]) => url === '/api/github')).toHaveLength(3);
    expect(screen.getByRole('alert')).toHaveTextContent(message);
    expect(screen.queryByRole('button', { name: 'Retry GitHub status' })).not.toBeInTheDocument();
    if (!installing)
      expect(screen.getByLabelText('GitHub account or organization')).toHaveValue('example-org');
    open.mockReturnValue(external as unknown as Window);
    source.statuses[endpoint] = 200;
    source.replies[endpoint] = installing
      ? connection
      : {
          create_url: 'https://github.com/organizations/example-org/settings/apps/new',
          manifest: '{"public":false}',
        };
    await act(async () => {
      fireEvent.click(screen.getByRole('button', { name: button }));
    });
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
  },
);

describe('prefill', () => {
  it('fills the fields from the desk, keeps them editable and never replaces a typed value', async () => {
    const source = createFixtureSource();
    let answer: (value: components['schemas']['GithubPrefill']) => void = () => {};
    const githubPrefill = vi.fn(
      () =>
        new Promise<{ ok: true; value: components['schemas']['GithubPrefill'] }>((resolve) => {
          answer = (value) => resolve({ ok: true, value });
        }),
    );
    source.set({ githubPrefill });
    const { user } = mount(<GithubSetup />, source);
    await vi.waitFor(() => expect(githubPrefill).toHaveBeenCalledOnce());
    const name = screen.getByRole('textbox', { name: 'Your name' });
    await user.type(name, 'Typed Owner');
    await act(async () =>
      answer({
        organization: 'example-org',
        owner_name: 'Example Owner',
        owner_email: 'owner@example.invalid',
      }),
    );
    const organization = screen.getByRole('textbox', { name: 'GitHub account or organization' });
    expect(organization).toHaveValue('example-org');
    expect(screen.getByRole('textbox', { name: 'Your email' })).toHaveValue(
      'owner@example.invalid',
    );
    expect(name).toHaveValue('Typed Owner');
    await user.clear(organization);
    await user.type(organization, 'other-org');
    expect(organization).toHaveValue('other-org');
  });
  it('leaves a field with no known value empty', async () => {
    const source = createFixtureSource();
    source.replies['/github/prefill'] = {
      organization: null,
      owner_name: 'Example Owner',
      owner_email: null,
    };
    mount(<GithubSetup />, source);
    expect(await screen.findByDisplayValue('Example Owner')).toBeInTheDocument();
    expect(screen.getByRole('textbox', { name: 'GitHub account or organization' })).toHaveValue('');
    expect(screen.getByRole('textbox', { name: 'Your email' })).toHaveValue('');
  });
});

describe('the next move on GitHub', () => {
  const installLink = 'https://github.com/apps/example/installations/new?state=current';
  const fillAndOpen = async (user: ReturnType<typeof mount>['user']) => {
    await user.type(screen.getByLabelText('GitHub account or organization'), 'example-org');
    await user.type(screen.getByLabelText('Your name'), 'Owner');
    await user.type(screen.getByLabelText('Your email'), 'owner@example.invalid');
    await user.click(screen.getByRole('button', { name: createApp }));
  };
  it('makes opening GitHub the primary action, says so, and offers no copy for the posted manifest', async () => {
    mount(<GithubSetup />);
    const open = await screen.findByRole('button', { name: createApp });
    expect(open).toHaveAttribute('data-variant', 'default');
    expect(
      screen.getByText(
        "Next: open GitHub and approve dvb's App. This screen continues when you're done.",
      ),
    ).toBeVisible();
    expect(screen.queryByRole('button', { name: 'Copy link' })).not.toBeInTheDocument();
    expect(screen.queryByRole('button', { name: installApp })).not.toBeInTheDocument();
  });
  it('waits for GitHub once it is opened and keeps opening it again reachable', async () => {
    const source = createFixtureSource();
    source.replies['POST /github/start'] = {
      create_url: 'https://github.com/organizations/example-org/settings/apps/new',
      manifest: '{"public":false}',
    } satisfies components['schemas']['GithubManifest'];
    vi.spyOn(window, 'open').mockReturnValue({ close: vi.fn() } as unknown as Window);
    // The engine stores the pending setup on start and reports it as waiting for the callback.
    const submit = vi.spyOn(HTMLFormElement.prototype, 'submit').mockImplementation(() => {
      source.replies['/github'] = {
        ...(source.replies['/github'] as object),
        create_app: { status: 'awaiting_callback' },
      };
    });
    const { user } = mount(<GithubSetup />, source);
    await fillAndOpen(user);
    const waiting = await screen.findByRole('status');
    expect(waiting).toHaveTextContent('Waiting for GitHub');
    expect(waiting).toHaveTextContent('Finish in the GitHub window.');
    expect(screen.queryByText(/^Next: open GitHub/)).not.toBeInTheDocument();
    const again = screen.getByRole('button', { name: 'Open GitHub again' });
    expect(again).toHaveAttribute('data-variant', 'outline');
    await user.click(again);
    await waitFor(() => expect(submit).toHaveBeenCalledTimes(2));
  });
  it('shows the installation the engine waits on as waiting for GitHub', async () => {
    const source = createFixtureSource();
    source.replies['/github'] = {
      ...created,
      install_url: installLink,
      install_app: { status: 'awaiting_callback' },
    };
    mount(<GithubSetup />, source);
    expect(await screen.findByText('Waiting for GitHub')).toBeVisible();
    expect(screen.getByRole('button', { name: 'Open GitHub again' })).toBeEnabled();
  });
  it.each([
    ['create_app', createApp],
    ['install_app', installApp],
  ])('stops waiting once GitHub reports %s failed', async (step, open) => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const source = createFixtureSource();
    const base =
      step === 'create_app' ? source.replies['/github'] : { ...created, install_url: installLink };
    const waiting = { ...(base as object), [step]: { status: 'awaiting_callback' } };
    source.replies['/github'] = base;
    source.replies['POST /github/start'] = {
      create_url: 'https://github.com/organizations/example-org/settings/apps/new',
      manifest: '{"public":false}',
    } satisfies components['schemas']['GithubManifest'];
    source.replies['POST /github/install'] = waiting;
    vi.spyOn(window, 'open').mockReturnValue({
      close: vi.fn(),
      location: { href: '' },
    } as unknown as Window);
    vi.spyOn(HTMLFormElement.prototype, 'submit').mockImplementation(() => {
      source.replies['/github'] = waiting;
    });
    const { user } = mount(<GithubSetup />, source);
    if (step === 'create_app') await fillAndOpen(user);
    else await user.click(await screen.findByRole('button', { name: open }));
    expect(await screen.findByText('Waiting for GitHub')).toBeVisible();
    source.replies['/github'] = {
      ...(base as object),
      [step]: {
        status: 'failed',
        failure: { code: 'callback_expired', message: 'GitHub setup expired.', retryable: true },
      },
    };
    await act(async () => {
      await vi.advanceTimersByTimeAsync(3000);
    });
    expect(await screen.findByText('GitHub setup expired.')).toBeVisible();
    expect(screen.queryByText('Waiting for GitHub')).not.toBeInTheDocument();
    expect(screen.getByRole('button', { name: open })).toHaveAttribute('data-variant', 'default');
    vi.useRealTimers();
  });
  it('makes installing the primary action and copies its GitHub link', async () => {
    const source = createFixtureSource();
    source.replies['/github'] = { ...created, install_url: installLink };
    const { user } = mount(<GithubSetup />, source);
    expect(await screen.findByRole('button', { name: installApp })).toHaveAttribute(
      'data-variant',
      'default',
    );
    expect(screen.getByText(/^Next: open GitHub, install dvb's App/)).toBeVisible();
    await user.click(screen.getByRole('button', { name: 'Copy link' }));
    expect(await navigator.clipboard.readText()).toBe(installLink);
    expect(screen.getByRole('button', { name: 'Link copied' })).toBeVisible();
  });
  it('shows the link when the browser cannot copy it', async () => {
    const source = createFixtureSource();
    source.replies['/github'] = { ...created, install_url: installLink };
    const { user } = mount(<GithubSetup />, source);
    vi.spyOn(navigator.clipboard, 'writeText').mockRejectedValue(new Error('denied'));
    await user.click(await screen.findByRole('button', { name: 'Copy link' }));
    expect(await screen.findByRole('alert')).toHaveTextContent(installLink);
  });
  it('opens repository access on GitHub from a button and copies its link', async () => {
    const source = createFixtureSource();
    const access = 'https://github.com/settings/installations/1';
    source.replies['/github/repositories'] = {
      organization: 'example-org',
      repository_access_url: access,
      repositories: [
        { repo: 'example-org/public', private: false, default_branch: 'main' },
        { repo: 'example-org/private', private: true, default_branch: 'main' },
      ],
    } satisfies components['schemas']['GithubRepositories'];
    const { user } = mount(<GithubRepositories />, source);
    const open = await screen.findByRole('link', {
      name: 'Open GitHub to change repository access',
    });
    expect(open).toHaveAttribute('href', access);
    expect(open).toHaveAttribute('data-slot', 'button');
    expect(screen.getByText('example-org/public — Public', { exact: true })).toBeVisible();
    expect(screen.getByText('example-org/private — Private', { exact: true })).toBeVisible();
    await user.click(screen.getByRole('button', { name: 'Copy link' }));
    expect(await navigator.clipboard.readText()).toBe(access);
  });
});
