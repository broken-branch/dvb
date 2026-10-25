import { useCallback, useEffect, useRef, useState } from 'react';
import type { components } from '@/api/schema';
import { useStore } from '@/lib/store';
import { refusalText } from '@/lib/data/display';
import { parseChecks } from '@/lib/data/selectors';
import { DataError } from './data-feedback';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Switch } from '@/components/ui/switch';

const CHECK_POLICY =
  'Selected checks must pass on the latest pull request head before dvb can merge. Code review, then design review, and every CI check must also pass. No additional design verdict is needed after merge.';

export function useProjectChecks(repo: string, create = false) {
  const {
    data: { discoverChecks },
  } = useStore();
  const [result, setResult] = useState<{
    repo: string;
    value: components['schemas']['GithubCheckDiscovery'];
  }>();
  const [selected, setSelected] = useState<string[]>([]);
  const [noCi, setNoCi] = useState(false);
  const [manual, setManual] = useState('');
  const [named, setNamed] = useState(false);
  const [busy, setBusy] = useState(false),
    [error, setError] = useState(''),
    [attempt, setAttempt] = useState(0);
  const repository = repo.trim().replace(/\/+$/, '');
  const lastRepository = useRef('');
  useEffect(() => {
    setNoCi(false);
    lastRepository.current = '';
  }, [repository, create]);
  useEffect(() => {
    setResult(undefined);
    setError('');
    setBusy(false);
    if (create || !repository) return;
    const controller = new AbortController();
    setBusy(true);
    void discoverChecks(repository, controller.signal)
      .then((response) => {
        if (controller.signal.aborted) return;
        if (!response.ok) setError(refusalText(response.refusal));
        else {
          setResult({ repo: repository, value: response.value });
          if (response.value.status === 'discovered') {
            const names = response.value.checks.map((c) => c.name);
            const sameRepository = lastRepository.current === repository;
            setSelected((previous) =>
              sameRepository ? previous.filter((name) => names.includes(name)) : names,
            );
            lastRepository.current = repository;
            if (names.length) setNoCi(false);
          }
        }
      })
      .catch((e) => {
        if (!controller.signal.aborted)
          setError(e instanceof Error ? e.message : 'Check discovery failed. Try again.');
      })
      .finally(() => {
        if (!controller.signal.aborted) setBusy(false);
      });
    return () => controller.abort();
  }, [repository, create, attempt, discoverChecks]);
  const value = result?.repo === repository ? result.value : undefined;
  const empty = value?.status === 'discovered' && value.checks.length === 0;
  const ready = create
    ? !named || parseChecks(manual).length > 0
    : !busy && value?.status === 'discovered' && (empty ? noCi : selected.length > 0);
  return {
    value,
    selected,
    setSelected,
    noCi,
    setNoCi,
    manual,
    setManual,
    named,
    setNamed,
    busy,
    error,
    empty,
    ready,
    ci: (create ? !named : empty && noCi)
      ? ('no_ci' as const)
      : { expected: create ? parseChecks(manual) : selected },
    refresh: useCallback(() => setAttempt((a) => a + 1), []),
  };
}

export function ProjectChecks({
  checks,
  create = false,
}: {
  checks: ReturnType<typeof useProjectChecks>;
  create?: boolean;
}) {
  const value = checks.value;
  return (
    <>
      <h3>Merge checks</h3>
      <p>{CHECK_POLICY}</p>
      {create ? (
        <>
          <p>
            A new repository has no CI yet. Set its checks later with{' '}
            <code>dvb project checks</code>, or name the checks it will run now.
          </p>
          <label className="switch-row">
            No CI yet
            <Switch
              checked={!checks.named}
              onCheckedChange={(enabled) => checks.setNamed(!enabled)}
            />
          </label>
          {checks.named && (
            <label className="field">
              Required check names
              <Input
                value={checks.manual}
                onChange={(e) => checks.setManual(e.target.value)}
                placeholder="Comma-separated check names"
              />
            </label>
          )}
        </>
      ) : (
        <>
          {checks.busy && <p role="status">Discovering checks on the default branch…</p>}
          <DataError message={checks.error} />
          {value && value.status !== 'discovered' && (
            <div role="alert">
              <p>
                {value.status === 'inaccessible'
                  ? 'Repository inaccessible to the App.'
                  : 'Check discovery failed.'}
              </p>
              <p>{value.failure.message}</p>
            </div>
          )}
          {value?.status === 'discovered' && (
            <>
              <p>
                Latest results on {value.default_branch} ({value.head.slice(0, 7)})
              </p>
              {value.checks.map((check) => (
                <label className="switch-row" key={check.name}>
                  <span>
                    {check.name} — {check.conclusion ?? check.status}
                  </span>
                  <Switch
                    aria-label={check.name}
                    checked={checks.selected.includes(check.name)}
                    onCheckedChange={(enabled) =>
                      checks.setSelected(
                        enabled
                          ? [...checks.selected, check.name]
                          : checks.selected.filter((name) => name !== check.name),
                      )
                    }
                  />
                </label>
              ))}
              {checks.empty && (
                <>
                  <p>
                    No check runs were found on this default branch head. This does not prove the
                    repository never uses CI.
                  </p>
                  <label className="switch-row">
                    Continue without CI
                    <Switch checked={checks.noCi} onCheckedChange={checks.setNoCi} />
                  </label>
                </>
              )}
            </>
          )}
          {!checks.busy && !value && !checks.error && (
            <p>Enter a repository name or local path to discover checks.</p>
          )}
          <Button type="button" variant="outline" disabled={checks.busy} onClick={checks.refresh}>
            Refresh checks
          </Button>
        </>
      )}
    </>
  );
}
