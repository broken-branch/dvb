import { mount } from '../support/render';
import { it, expect, vi, afterEach, describe } from 'vitest';
import { act, fireEvent, screen, within } from '@testing-library/react';
import { createFixtureSource } from '../support/render';
import { LoginStep } from '@/components/dvb/login-step';
describe('code', () => {
  it('finishes Claude sign-in and polls with an empty code after sending the pasted code once', async () => {
    const { LoginStep } = await import('@/components/dvb/login-step');
    const source = createFixtureSource();
    const startLogin = vi.fn().mockResolvedValue({
      ok: true,
      value: {
        status: 'pending',
        run: 'login-r',
        url: 'https://example.invalid/auth',
        prompt: 'Paste the code.',
      },
    });
    const finishLogin = vi
      .fn()
      .mockResolvedValueOnce({
        ok: true,
        value: {
          status: 'pending',
          run: 'login-r',
          url: '',
          prompt: 'Still waiting for the login to finish.',
        },
      })
      .mockResolvedValueOnce({
        ok: true,
        value: { status: 'logged_in', run: 'login-r', url: '', prompt: 'Logged in.' },
      });
    source.set({ startLogin, finishLogin });
    const { user } = mount(<LoginStep />, source);
    await user.click(screen.getAllByRole('button', { name: 'Sign in' })[0]);
    expect(startLogin).toHaveBeenCalledWith('claude');
    await user.type(
      await screen.findByRole('textbox', { name: 'Code from the sign-in page' }),
      'pasted-code',
    );
    vi.useFakeTimers();
    await act(async () => {
      fireEvent.click(screen.getByRole('button', { name: 'Finish sign-in' }));
    });
    expect(finishLogin).toHaveBeenNthCalledWith(1, 'claude', {
      run: 'login-r',
      code: 'pasted-code',
    });
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1000);
    });
    expect(screen.getByText('Signed in')).toBeInTheDocument();
    expect(finishLogin).toHaveBeenNthCalledWith(2, 'claude', { run: 'login-r', code: '' });
  });
});
describe('recovery', () => {
  afterEach(() => {
    history.replaceState({}, '', '/');
  });
  it('refreshes login status when authorization ended before Finish sign-in', async () => {
    const source = createFixtureSource();
    const logins = vi
      .fn()
      .mockResolvedValueOnce({ ok: true, value: { claude: false, codex: false } })
      .mockResolvedValue({ ok: true, value: { claude: false, codex: true } });
    const startLogin = vi.fn().mockResolvedValue({
      ok: true,
      value: { status: 'pending', run: 'login-r', url: '', prompt: 'Authorize Codex.' },
    });
    const finishLogin = vi.fn().mockResolvedValue({
      ok: true,
      value: {
        status: 'logged_in',
        run: 'login-r',
        url: '',
        prompt: 'This login run has already ended (Logged in.); start a new one.',
      },
    });
    source.set({ logins, startLogin, finishLogin });
    const { user } = mount(<LoginStep />, source);
    await user.click(screen.getAllByRole('button', { name: 'Sign in' })[1]);
    await user.click(screen.getByRole('button', { name: 'Finish sign-in' }));
    expect(await screen.findByText('Signed in')).toBeInTheDocument();
    expect(screen.queryAllByRole('alert').map((a) => a.textContent)).toEqual([]);
    expect(logins).toHaveBeenCalledTimes(2);
    expect(finishLogin).toHaveBeenCalledOnce();
  });
  it.each([
    'Login failed: denied',
    'This login run has already ended (Login failed: denied); start a new one.',
    'refused',
  ])('starts a fresh login after %s', async (prompt) => {
    const source = createFixtureSource(),
      logins = vi.fn().mockResolvedValue({ ok: true, value: { claude: false, codex: false } });
    const startLogin = vi
      .fn()
      .mockResolvedValueOnce({
        ok: true,
        value: { status: 'pending', run: 'old', url: '', prompt: 'Paste the code.' },
      })
      .mockResolvedValueOnce({
        ok: true,
        value: { status: 'pending', run: 'new', url: '', prompt: 'Paste the new code.' },
      });
    const finishLogin = vi
      .fn()
      .mockResolvedValue(
        prompt === 'refused'
          ? { ok: false, refusal: { code: 'NOT_A_RUN', detail: 'Unknown login run.' } }
          : { ok: true, value: { status: 'failed', run: 'old', url: '', prompt } },
      );
    source.set({ logins, startLogin, finishLogin });
    const { user } = mount(<LoginStep />, source);
    await user.click(screen.getAllByRole('button', { name: 'Sign in' })[0]);
    await user.type(
      screen.getByRole('textbox', { name: 'Code from the sign-in page' }),
      'old-code',
    );
    await user.click(screen.getByRole('button', { name: 'Finish sign-in' }));
    expect(await screen.findByRole('alert')).toHaveTextContent(
      prompt === 'refused' ? 'Unknown login run.' : prompt,
    );
    expect(screen.queryByRole('textbox')).not.toBeInTheDocument();
    await user.click(screen.getAllByRole('button', { name: 'Sign in' })[0]);
    expect(screen.getByRole('textbox')).toHaveValue('');
    expect(startLogin).toHaveBeenCalledTimes(2);
    expect(finishLogin).toHaveBeenCalledExactlyOnceWith('claude', { run: 'old', code: 'old-code' });
    expect(logins).toHaveBeenCalledTimes(2);
  });
});
describe('status', () => {
  it('login success uses status with independently worded prompts', async () => {
    const { LoginStep } = await import('@/components/dvb/login-step');
    const source = createFixtureSource();
    source.set({
      logins: async () => ({ ok: true, value: { claude: false, codex: false } }),
      startLogin: async () => ({
        ok: true,
        value: {
          status: 'pending',
          run: 'login',
          url: '',
          prompt: 'Authorize in the vendor page.',
        },
      }),
      finishLogin: async () => ({
        ok: true,
        value: { status: 'logged_in', run: 'login', url: '', prompt: 'Authorization accepted.' },
      }),
    });
    const { user } = mount(<LoginStep />, source);
    const row = screen.getByRole('heading', { name: 'Codex' }).closest('.login-row') as HTMLElement;
    await user.click(within(row).getByRole('button', { name: 'Sign in', exact: true }));
    await user.click(within(row).getByRole('button', { name: 'Finish sign-in' }));
    expect(within(row).getByText('Signed in')).toBeInTheDocument();
    expect(within(row).queryByRole('alert')).not.toBeInTheDocument();
  });
});
describe('needed vendors', () => {
  const rows = () =>
    ['Claude Code', 'Codex'].map(
      (name) => screen.getByRole('heading', { name }).closest('.login-row') as HTMLElement,
    );
  it('is signed in with only the needed Claude and shows Codex as optional', async () => {
    const source = createFixtureSource();
    source.replies['/logins'] = { claude: true, codex: false, needed: ['claude'] };
    const onChange = vi.fn();
    mount(<LoginStep onChange={onChange} />, source);
    await vi.waitFor(() => expect(onChange).toHaveBeenLastCalledWith(true));
    const [claude, codex] = rows();
    expect(within(claude).getByText('Signed in')).toBeInTheDocument();
    expect(within(claude).queryByText(/Optional/)).toBeNull();
    expect(within(codex).getByText("Optional. New projects' hires do not use it.")).toBeVisible();
    expect(within(codex).getByRole('button', { name: 'Sign in' })).toBeEnabled();
  });
  it('is not signed in while a needed vendor is missing', async () => {
    const source = createFixtureSource();
    source.replies['/logins'] = { claude: true, codex: false, needed: ['codex'] };
    const onChange = vi.fn();
    mount(<LoginStep onChange={onChange} />, source);
    await screen.findByText('Signed in');
    expect(onChange).toHaveBeenLastCalledWith(false);
    expect(within(rows()[0]).getByText(/Optional/)).toBeInTheDocument();
    expect(within(rows()[1]).queryByText(/Optional/)).toBeNull();
  });
  it('needs both vendors from an older engine that sends no list', async () => {
    const source = createFixtureSource();
    source.replies['/logins'] = { claude: true, codex: false };
    const onChange = vi.fn();
    mount(<LoginStep onChange={onChange} />, source);
    await screen.findByText('Signed in');
    expect(onChange).toHaveBeenLastCalledWith(false);
    expect(screen.queryByText(/Optional/)).toBeNull();
  });
  it('is signed in when the list is empty and shows neither vendor as required', async () => {
    const source = createFixtureSource();
    source.replies['/logins'] = { claude: false, codex: false, needed: [] };
    const onChange = vi.fn();
    mount(<LoginStep onChange={onChange} />, source);
    await vi.waitFor(() => expect(onChange).toHaveBeenLastCalledWith(true));
    for (const row of rows()) expect(within(row).getByText(/Optional/)).toBeInTheDocument();
  });
});
