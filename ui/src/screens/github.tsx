import { useCallback, useEffect, useRef, useState } from 'react';
import { Card, ListRow } from '@/components/dvb/card';
import { Heading, InfoTip, Status } from '@/components/dvb/common';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { DataError, useRead } from '@/components/dvb/data-feedback';
import { useStore } from '@/lib/store';
import { refusalText, word } from '@/lib/data/display';
import type { components } from '@/api/schema';

type AccountType = components['schemas']['GithubAccountType'];

/** GitHub setup actions, backed by the desk's manifest bridge. */
export function GithubSetup({
  onChange,
}: { onChange?: (connected: boolean, accountType?: AccountType | null) => void } = {}) {
  const {
    data: { github, startGithub, retryGithubInstall, githubPrefill },
    canWrite,
  } = useStore();
  const [connection, setConnection] = useState<components['schemas']['GithubConnection']>({
    connected: false,
    organization: null,
    install_url: null,
    repository_access_url: null,
    create_app: { status: 'not_started' },
    install_app: { status: 'not_started' },
  });
  const [organization, setOrganization] = useState(''),
    [ownerName, setOwnerName] = useState(''),
    [ownerEmail, setOwnerEmail] = useState(''),
    [statusError, setStatusError] = useState(''),
    [createError, setCreateError] = useState(''),
    [installError, setInstallError] = useState(''),
    [busy, setBusy] = useState(false),
    [loaded, setLoaded] = useState(false),
    [attempt, setAttempt] = useState(0);
  const appCreated = !!connection.install_url || connection.create_app?.status === 'succeeded';
  // Suggestions for Create App's fields, read once the status shows there is no App yet. Fields
  // the owner has typed in are never replaced.
  const typed = useRef(new Set<string>()),
    wantsPrefill = loaded && !connection.connected && !appCreated;
  useEffect(() => {
    if (!wantsPrefill) return;
    let alive = true;
    githubPrefill()
      .then((result) => {
        if (!alive || !result.ok) return;
        const fill = (
          field: string,
          value: string | null | undefined,
          set: typeof setOrganization,
        ) => {
          if (value && !typed.current.has(field)) set(value);
        };
        fill('organization', result.value.organization, setOrganization);
        fill('name', result.value.owner_name, setOwnerName);
        fill('email', result.value.owner_email, setOwnerEmail);
      })
      // Without suggestions the fields stay empty for the owner to fill.
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [wantsPrefill, githubPrefill]);
  const type = (field: string, set: typeof setOrganization, value: string) => {
    typed.current.add(field);
    set(value);
  };
  useEffect(() => {
    onChange?.(connection.connected, connection.account_type);
  }, [connection.connected, connection.account_type, onChange]);
  useEffect(() => {
    if (connection.connected) return;
    const controller = new AbortController();
    const read = async () => {
      try {
        const response = await github(controller.signal);
        if (!response.ok) throw new Error(refusalText(response.refusal));
        const next = response.value;
        if (controller.signal.aborted) return;
        setConnection(next);
        setLoaded(true);
        setStatusError('');
        if (next.connected || next.create_app?.status === 'succeeded') setCreateError('');
        if (next.connected || next.install_app?.status === 'succeeded') setInstallError('');
      } catch (e) {
        if (!controller.signal.aborted)
          setStatusError(e instanceof Error ? e.message : 'GitHub setup is unavailable.');
      }
    };
    void read();
    const timer = setInterval(() => void read(), 3000);
    return () => {
      controller.abort();
      clearInterval(timer);
    };
  }, [connection.connected, github, attempt]);
  const create = async () => {
    setBusy(true);
    setCreateError('');
    // Open during the click so browsers allow the GitHub window after the desk reply.
    const target = `dvb-github-${crypto.randomUUID()}`,
      windowRef = window.open('about:blank', target);
    try {
      if (!windowRef)
        throw new Error('Allow popups for dvb, then retry Create App. Your draft is kept here.');
      const response = await startGithub({
        organization,
        owner_name: ownerName,
        owner_email: ownerEmail,
      });
      if (!response.ok) throw new Error(refusalText(response.refusal));
      const prepared = response.value;
      const form = document.createElement('form');
      form.method = 'POST';
      form.action = prepared.create_url;
      form.target = target;
      const input = document.createElement('input');
      input.type = 'hidden';
      input.name = 'manifest';
      input.value = prepared.manifest;
      form.append(input);
      document.body.append(form);
      form.submit();
      form.remove();
      // Read the status now: the engine reports the step as waiting once the setup is stored.
      setAttempt((n) => n + 1);
    } catch (e) {
      windowRef?.close();
      setCreateError(e instanceof Error ? e.message : 'GitHub setup could not start.');
    } finally {
      setBusy(false);
    }
  };
  const install = async () => {
    setInstallError('');
    // While GitHub's window is open the callback expects the pending link's state, so opening
    // GitHub again shows that link in the same window instead of asking for a new one.
    const pending =
      connection.install_app?.status === 'awaiting_callback' ? connection.install_url : null;
    const external = window.open(pending || 'about:blank', 'dvb-github-install');
    if (pending) {
      if (external) external.focus();
      else setInstallError('Allow popups for dvb, then retry Install App.');
      return;
    }
    setBusy(true);
    try {
      if (!external) throw new Error('Allow popups for dvb, then retry Install App.');
      const response = await retryGithubInstall();
      if (!response.ok) throw new Error(refusalText(response.refusal));
      setConnection(response.value);
      if (!response.value.install_url)
        throw new Error('No installation link is available. Retry in GitHub setup.');
      external.location.href = response.value.install_url;
    } catch (e) {
      external?.close();
      setInstallError(e instanceof Error ? e.message : 'Installation could not start.');
    } finally {
      setBusy(false);
    }
  };
  return (
    <>
      <Heading
        level={2}
        title="Connect your GitHub account or organization"
        description="A private App creates PRs and keeps the board up to date."
      />
      <Card
        onKeyDown={(e) => {
          if (
            e.key !== 'Enter' ||
            !(e.target instanceof HTMLInputElement) ||
            e.nativeEvent.isComposing
          )
            return;
          e.preventDefault();
          if (
            loaded &&
            !busy &&
            !appCreated &&
            canWrite &&
            organization.trim() &&
            ownerName.trim() &&
            ownerEmail.trim()
          )
            void create();
        }}
      >
        {connection.connected ? (
          <>
            <Status tone="good">Connected to {connection.organization}</Status>
            <GithubRepositories />
          </>
        ) : (
          <>
            {!appCreated && (
              <>
                <label className="field">
                  GitHub account or organization
                  <Input
                    value={organization}
                    onChange={(e) => type('organization', setOrganization, e.target.value)}
                  />
                </label>
                <InfoTip text="Your name and email stay on the work machine so dvb can block them from published work." />
                <label className="field">
                  Your name
                  <Input
                    autoComplete="name"
                    value={ownerName}
                    onChange={(e) => type('name', setOwnerName, e.target.value)}
                  />
                </label>
                <label className="field">
                  Your email
                  <Input
                    type="email"
                    autoComplete="email"
                    value={ownerEmail}
                    onChange={(e) => type('email', setOwnerEmail, e.target.value)}
                  />
                </label>
              </>
            )}
            <ListRow>
              <div className="login-row">
                <div>
                  <h3>1. Create the private App</h3>
                  {!appCreated && (
                    <GithubAction
                      next={word('setup.github.create-next')}
                      open={word('setup.github.create-open')}
                      waiting={connection.create_app?.status === 'awaiting_callback'}
                      disabled={
                        !loaded ||
                        busy ||
                        !canWrite ||
                        !organization.trim() ||
                        !ownerName.trim() ||
                        !ownerEmail.trim()
                      }
                      onOpen={() => void create()}
                    />
                  )}
                </div>
                {appCreated && <Status tone="good">{word('setup.github.created')}</Status>}
              </div>
            </ListRow>
            <ListRow>
              <div className="login-row">
                <div>
                  <h3>
                    2. Install on{' '}
                    {connection.account_type === 'organization'
                      ? 'the organization'
                      : connection.account_type === 'user'
                        ? 'your account'
                        : 'the account'}
                  </h3>
                  {appCreated && (
                    <GithubAction
                      next={word('setup.github.install-next')}
                      open={word('setup.github.install-open')}
                      waiting={connection.install_app?.status === 'awaiting_callback'}
                      disabled={busy || !canWrite}
                      onOpen={() => void install()}
                      link={connection.install_url || ''}
                    />
                  )}
                </div>
              </div>
            </ListRow>
          </>
        )}
        {!loaded && !statusError && <p role="status">Checking GitHub setup…</p>}
        {[connection.create_app, connection.install_app].map((step, i) =>
          step?.status === 'failed' ? <DataError key={i} message={step.failure.message} /> : null,
        )}
        <DataError message={createError} />
        <DataError message={installError} />
        <DataError message={statusError} />
        {statusError && (
          <Button type="button" variant="outline" onClick={() => setAttempt((n) => n + 1)}>
            Retry GitHub status
          </Button>
        )}
      </Card>
    </>
  );
}

/**
 * A setup step whose next move is on GitHub: one line saying so and the button that opens it. Once
 * opened it shows that dvb waits for GitHub, keeping the button to open GitHub again.
 */
function GithubAction({
  next,
  open,
  waiting,
  disabled,
  onOpen,
  link,
}: {
  next: string;
  open: string;
  waiting: boolean;
  disabled: boolean;
  onOpen: () => void;
  link?: string;
}) {
  return (
    <div>
      {waiting ? (
        <div role="status">
          <Status tone="working">{word('setup.github.waiting')}</Status>
          <p>{word('setup.github.waiting-detail')}</p>
        </div>
      ) : (
        <p>{next}</p>
      )}
      <div className="actions">
        <Button
          type="button"
          variant={waiting ? 'outline' : 'default'}
          disabled={disabled}
          onClick={onOpen}
        >
          {waiting ? word('setup.github.open-again') : open}
        </Button>
        {link && <CopyLink link={link} />}
      </div>
    </div>
  );
}

/** Copies a GitHub link for a browser on another machine. */
function CopyLink({ link }: { link: string }) {
  const [copied, setCopied] = useState(false),
    [failed, setFailed] = useState(false);
  const copy = () => {
    setFailed(false);
    // Without a secure context the browser has no clipboard; the link is shown instead.
    Promise.resolve()
      .then(() => navigator.clipboard.writeText(link))
      .then(
        () => setCopied(true),
        () => setFailed(true),
      );
  };
  return (
    <>
      <Button type="button" variant="outline" onClick={copy}>
        {copied ? word('setup.github.copied') : word('setup.github.copy')}
      </Button>
      {failed && <DataError message={word('setup.github.copy-failed', { url: link })} />}
    </>
  );
}

export function GithubRepositories({
  onRefresh,
  onLoad,
  onPick,
}: {
  onRefresh?: () => void;
  onLoad?: (value: components['schemas']['GithubRepositories'] | undefined) => void;
  onPick?: (repo: string) => void;
} = {}) {
  const { data: s } = useStore();
  const [attempt, setAttempt] = useState(0);
  const { value, error } = useRead(() => s.githubRepositories(), [s.githubRepositories, attempt]);
  useEffect(() => onLoad?.(value), [value, onLoad]);
  const refresh = useCallback(() => {
    setAttempt((n) => n + 1);
    onRefresh?.();
  }, [onRefresh]);
  useEffect(() => {
    window.addEventListener('focus', refresh);
    return () => window.removeEventListener('focus', refresh);
  }, [refresh]);
  return (
    <section aria-label="GitHub repository access">
      {value ? (
        <>
          <p>GitHub account: {value.organization}</p>
          <div className="actions">
            <Button asChild variant="outline">
              <a href={value.repository_access_url} target="_blank" rel="noreferrer">
                {word('setup.github.access-open')}
              </a>
            </Button>
            <CopyLink link={value.repository_access_url} />
          </div>
          <p>After changing repository access, return here and refresh.</p>
          <h3>Reachable repositories</h3>
          {value.repositories.length ? (
            <ul>
              {value.repositories.map((repo) => (
                <li key={repo.repo}>
                  {repo.repo} — {repo.private ? 'Private' : 'Public'}
                  {onPick && (
                    <>
                      {' '}
                      <Button
                        type="button"
                        variant="outline"
                        size="sm"
                        aria-label={`Use ${repo.repo}`}
                        onClick={() => onPick(repo.repo)}
                      >
                        Use
                      </Button>
                    </>
                  )}
                </li>
              ))}
            </ul>
          ) : (
            <p>The App cannot reach any repositories. Update repository access, then refresh.</p>
          )}
        </>
      ) : error ? (
        <DataError message={error} />
      ) : (
        <p role="status">Loading reachable repositories…</p>
      )}
      <Button type="button" variant="outline" onClick={refresh}>
        Refresh repository access
      </Button>
    </section>
  );
}
