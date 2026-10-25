import { routes } from '@/lib/routes';
import { useCallback, useEffect, useRef, useState } from 'react';
import { Check, ArrowRight, Monitor, Server } from 'lucide-react';
import { Card } from '@/components/dvb/card';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Textarea } from '@/components/ui/textarea';
import { RadioGroup, RadioGroupItem } from '@/components/ui/radio-group';
import { InfoTip, Heading, Status, ChoiceSelect } from '@/components/dvb/common';
import { DataError, useAction } from '@/components/dvb/data-feedback';
import { GoalIssues } from '@/components/dvb/goal-issues';
import { LoginStep } from '@/components/dvb/login-step';
import { ProjectChecks, useProjectChecks } from '@/components/dvb/project-checks';
import { GithubRepositories, GithubSetup } from '@/screens/github';
import { Walkthrough } from '@/screens/walkthrough';
import { go, useStore } from '@/lib/store';
import { issuesFoundText } from '@/lib/data/display';
import type { IssueCounts, NewProject, Project } from '@/lib/data/types';
import type { components } from '@/api/schema';

export const FIRST_GOAL = 'Get to the first working PR';
const STEPS = ['Machine', 'Sign in', 'GitHub', 'How dvb works', 'First project'] as const;

/**
 * The project name for a repository: its last part without a trailing `.git`, with every character
 * a project name cannot have (anything but letters, digits, `-` and `_`) turned into `-`.
 */
export function projectName(repo: string) {
  const repository = repo.trim().replace(/\/+$/, '');
  return (repository.split('/').pop() || repository)
    .replace(/\.git$/, '')
    .replace(/[^A-Za-z0-9_-]/g, '-');
}

/** The project to add: the name comes from the repository, checks come from the list. */
export function projectRequest(
  kind: 'existing' | 'new',
  repo: string,
  ci: NewProject['ci'],
): NewProject {
  const repository = repo.trim().replace(/\/+$/, '');
  return {
    name: projectName(repository),
    repo: repository,
    ...(kind === 'new' ? { create: true } : {}),
    ci,
  };
}

/**
 * The GitHub repository a typed name stands for: a bare name becomes owner/name when exactly
 * one reachable repository has that name; otherwise the owner is asked to name the owner.
 */
function resolveRepository(
  repo: string,
  create: boolean,
  reachable: components['schemas']['GithubRepositories'] | undefined,
): { repo: string; refusal?: undefined } | { repo?: undefined; refusal: string } {
  const name = repo.trim().replace(/\/+$/, '');
  if (!name || name.includes('/')) return { repo: name };
  const suggestion = `Enter it with its owner, for example ${reachable?.organization || 'owner'}/${name}.`;
  const matches = create
    ? []
    : (reachable?.repositories ?? []).filter(
        (r) => r.repo.split('/').pop()?.toLowerCase() === name.toLowerCase(),
      );
  if (matches.length === 1) return { repo: matches[0].repo };
  if (matches.length > 1)
    return {
      refusal: `Several reachable repositories are named ${name}: ${matches.map((r) => r.repo).join(', ')}. Enter the one you mean.`,
    };
  if (create || !reachable) return { refusal: suggestion };
  return { refusal: `No reachable repository is named ${name}. ${suggestion}` };
}

/** Whether Continue is the plain label or a skip: steps the engine already knows are done need no action. */
export const continueLabel = (done: boolean) => (done ? 'Continue' : 'Skip for now');

export function Setup() {
  const { data: s, canWrite } = useStore(),
    action = useAction();
  // An engine already connected means the machine is set up, local or forwarded, so setup opens on Sign in.
  const engineConnected = s.connection.state === 'connected';
  const [step, setStep] = useState(engineConnected ? 1 : 0),
    [host, setHost] = useState('local'),
    [address, setAddress] = useState('');
  const [signedIn, setSignedIn] = useState(false),
    [connected, setConnected] = useState(false),
    [accountType, setAccountType] = useState<components['schemas']['GithubAccountType'] | null>();
  const githubChanged = useCallback(
    (now: boolean, type?: components['schemas']['GithubAccountType'] | null) => {
      setConnected(now);
      setAccountType(type);
    },
    [],
  );
  // GitHub does not let the App create a repository in a personal account.
  const canCreate = accountType !== 'user';
  const [chosenKind, setKind] = useState<'known' | 'existing' | 'new'>(),
    [chosenProject, setProject] = useState(''),
    [repo, setRepo] = useState(''),
    [repoLeft, setRepoLeft] = useState(false),
    [typedGoal, setGoal] = useState<string>(),
    [picked, setPicked] = useState<number[]>([]),
    [reachable, setReachable] = useState<components['schemas']['GithubRepositories']>(),
    // A project added here, with what dvb found in its issues, until the projects list has it.
    [added, setAdded] = useState<Project>(),
    // The engine refused the name taken from the repository; cleared when the repository changes.
    [nameRefused, setNameRefused] = useState(false);
  const projects =
    added && !s.projects.some((p) => p.id === added.id) ? [...s.projects, added] : s.projects;
  const known = projects.find((p) => p.id === chosenProject) || projects[0],
    kind =
      (chosenKind === 'new' && !canCreate ? undefined : chosenKind) ||
      (known ? 'known' : 'existing');
  const { counts, loading: countsLoading } = useIssueCounts(
    step === 4 && kind === 'known' ? known : undefined,
    added,
  );
  // With trusted issues to work from, or before the counts say, the goal is not pre-filled.
  const goal = typedGoal ?? (countsLoading || counts?.trusted ? '' : FIRST_GOAL);
  // Only a project dvb already has can list its issues before the goal is stated.
  const issues = kind === 'known' ? picked : [];
  const resolved = resolveRepository(repo, kind === 'new', reachable);
  const repository = resolved.repo ?? '';
  // A refusal waits until the owner leaves the field or presses Enter, so it does not interrupt their typing.
  const refusal = repoLeft ? resolved.refusal : undefined;
  const checks = useProjectChecks(step === 4 && kind !== 'known' ? repository : '', kind === 'new');
  useEffect(() => {
    if (connected) checks.refresh();
  }, [connected, checks.refresh]);
  const duplicate =
    kind !== 'known' &&
    !!repository.trim() &&
    s.projects.some((p) => p.name === projectName(repository));
  const changeRepo = (value: string) => {
    setRepo(value);
    setNameRefused(false);
  };
  // On a cold load the connection turns connected after setup opens: move past Machine once, unless
  // the owner has already left it; Back to Machine afterwards stays there.
  const passedMachine = useRef(engineConnected);
  useEffect(() => {
    if (!engineConnected || passedMachine.current) return;
    passedMachine.current = true;
    setStep((current) => (current === 0 ? 1 : current));
  }, [engineConnected]);
  const next = () => setStep(step + 1);
  // Each step opens at the top, so its heading is not left under the top bar from the last step's scroll.
  useEffect(() => {
    window.scrollTo({ top: 0 });
  }, [step]);
  /** Adds the project when it is new, states the first goal when asked, and opens the project. */
  const finish = (withGoal: boolean) =>
    void action.act(
      async () => {
        let id = known?.id ?? '';
        if (kind !== 'known') {
          const connection = await s.github();
          if (!connection.ok) return connection;
          if (!connection.value.connected) {
            setConnected(false);
            throw new Error('Connect GitHub to continue. Your project draft is kept here.');
          }
          const project = await s.addProject(projectRequest(kind, repository, checks.ci));
          if (!project.ok && project.refusal.code === 'INVALID_PROJECT_NAME') {
            // Explained next to the Repository field, which the name comes from.
            setNameRefused(true);
            return { ok: true as const, value: '' };
          }
          if (!project.ok) return project;
          id = project.value.id;
          if (!withGoal && project.value.issues_found?.open) {
            // Stay on the step to show what dvb found, and offer to work from trusted issues.
            setAdded(project.value);
            setProject(id);
            setKind('known');
            return { ok: true as const, value: '' };
          }
        }
        if (!withGoal) return { ok: true as const, value: id };
        const stated = await s.stateGoal(
          id,
          issues.length ? { text: goal.trim(), issues } : { text: goal.trim() },
        );
        return stated.ok ? { ok: true as const, value: id } : stated;
      },
      (id) => id && go(routes.project(id)),
    );
  /** Asks the orchestrator to propose the first goal from the open issues, and opens the project for its question. */
  const workFromIssues = (id: string) =>
    void action.act(
      () => s.proposeGoal(id),
      () => go(routes.project(id)),
    );
  const bodies = [
    <>
      <h2>
        Where should the work happen?{' '}
        <InfoTip text="This window is where you follow the work and answer questions." />
      </h2>
      <RadioGroup value={host} onValueChange={setHost} className="host-options">
        {[
          ['local', 'This machine', 'Everything runs here.'],
          ['remote', 'Another Linux machine', 'Work runs there. You oversee it here.'],
        ].map(([v, t, d]) => (
          <Card asChild density="compact" key={v}>
            <label className={`choice-row ${host === v ? 'selected' : ''}`}>
              <RadioGroupItem value={v} />
              <span>
                {v === 'local' ? <Monitor size={24} /> : <Server size={24} />}
                <strong>{t}</strong>
                <InfoTip text={d} />
              </span>
            </label>
          </Card>
        ))}
      </RadioGroup>
      {host === 'remote' && (
        <>
          <label className="field">
            Machine address
            <Input
              value={address}
              onChange={(e) => setAddress(e.target.value)}
              placeholder="user@machine"
            />
          </label>
          <p>Download install.sh from the current GitHub release on both machines.</p>
          <p>On the other machine:</p>
          <pre>{'sudo sh install.sh --engine'}</pre>
          <p>Then here:</p>
          <pre>{`sudo sh install.sh --cockpit --engine-host ${address.trim() || 'user@machine'}`}</pre>
          <Status tone={canWrite ? 'good' : 'neutral'}>
            {canWrite
              ? 'Connected'
              : s.connection.state === 'version-mismatch'
                ? `Different version (${s.connection.engineVersion})`
                : 'Waiting for the machine'}
          </Status>
        </>
      )}
    </>,
    <LoginStep onChange={setSignedIn} />,
    <GithubSetup onChange={githubChanged} />,
    <Walkthrough onDone={next} />,
    <>
      <h2>
        Your first project{' '}
        <InfoTip text="You set a goal. The orchestrator proposes a plan. You approve it, and the team builds and reviews." />
      </h2>
      <RadioGroup value={kind} onValueChange={(v) => setKind(v as 'known' | 'existing' | 'new')}>
        {known && (
          <Card asChild density="compact">
            <label className="choice-row">
              <RadioGroupItem value="known" />
              <span>
                <strong>A project dvb already has</strong>
              </span>
            </label>
          </Card>
        )}
        <Card asChild density="compact">
          <label className="choice-row">
            <RadioGroupItem value="existing" />
            <span>
              <strong>Use an existing repository</strong>
            </span>
          </label>
        </Card>
        {canCreate && (
          <Card asChild density="compact">
            <label className="choice-row">
              <RadioGroupItem value="new" />
              <span>
                <strong>Create a private repository</strong>
              </span>
            </label>
          </Card>
        )}
      </RadioGroup>
      {!canCreate && (
        <p>
          dvb cannot create a repository in a personal GitHub account; create it on GitHub, give the
          dvb App access to it, then add it here.
        </p>
      )}
      {kind === 'known' && known ? (
        <div className="field">
          Project
          <ChoiceSelect
            label="Project"
            value={known.id}
            onChange={(project) => {
              setProject(project);
              setPicked([]);
            }}
            options={projects.map((p) => ({ value: p.id, label: `${p.name} (${p.repo})` }))}
          />
        </div>
      ) : (
        <>
          <label className="field">
            Repository
            <Input
              required
              value={repo}
              onChange={(e) => {
                changeRepo(e.target.value);
                setRepoLeft(false);
              }}
              onBlur={() => setRepoLeft(true)}
              onKeyDown={(e) => e.key === 'Enter' && setRepoLeft(true)}
              aria-invalid={!!refusal || nameRefused}
              aria-describedby={
                refusal ? 'repository-refusal' : nameRefused ? 'project-name-refusal' : undefined
              }
              placeholder="owner/repository"
            />
          </label>
          {refusal ? (
            <p role="alert" id="repository-refusal">
              {refusal}
            </p>
          ) : (
            !resolved.refusal &&
            !repo.includes('/') &&
            repository && <p role="status">Repository: {repository}</p>
          )}
          {nameRefused && (
            <p role="alert" id="project-name-refusal">
              dvb cannot name the project after this repository. A project name can use only
              letters, digits, - and _.
            </p>
          )}
          {connected ? (
            <>
              <GithubRepositories
                onRefresh={checks.refresh}
                onLoad={setReachable}
                onPick={kind === 'existing' ? changeRepo : undefined}
              />
              <ProjectChecks checks={checks} create={kind === 'new'} />
              <Button
                type="button"
                variant="outline"
                disabled={action.busy}
                onClick={() =>
                  void action.act(
                    () => s.github(),
                    (connection) => githubChanged(connection.connected, connection.account_type),
                  )
                }
              >
                Check GitHub connection
              </Button>
            </>
          ) : (
            <>
              <p>Connect and install the App to continue. Your project draft is kept here.</p>
              <GithubSetup onChange={githubChanged} />
            </>
          )}
        </>
      )}
      {countsLoading && <p role="status">Reading the open issues…</p>}
      {!!counts?.open && <p role="status">{issuesFoundText(counts)}</p>}
      <label className="field">
        First goal
        <Textarea
          className="written-input"
          required={!issues.length}
          rows={3}
          value={goal}
          onChange={(e) => setGoal(e.target.value)}
        />
      </label>
      {kind === 'known' && known && (
        <GoalIssues project={known.id} picked={picked} onChange={setPicked} />
      )}
      {duplicate && (
        <p role="alert">dvb already has this project. Pick it from the list of projects dvb has.</p>
      )}
      <DataError message={action.error} />
    </>,
  ];
  const addable =
    (kind === 'known' ? !!known : !!repository && !duplicate && connected && checks.ready) &&
    canWrite &&
    !action.busy;
  const ready = step !== 4 || (addable && !countsLoading && (!!goal.trim() || !!issues.length));
  return (
    <div className="setup-page">
      <Heading title="Welcome to dvb." description="A small team, working toward your goals.">
        <Button variant="ghost" onClick={() => go(routes.home())}>
          Skip setup
        </Button>
      </Heading>
      <ol className="setup-steps">
        {STEPS.map((t, i) => {
          const skipped = i < step && ((i === 1 && !signedIn) || (i === 2 && !connected));
          return (
            <li
              className={i === step ? 'active' : skipped ? 'skipped' : i < step ? 'complete' : ''}
              key={t}
            >
              <span>{i < step && !skipped ? <Check size={14} /> : i + 1}</span>
              {t}
              {skipped && <small>Skipped</small>}
            </li>
          );
        })}
      </ol>
      <form
        onSubmit={(e) => {
          e.preventDefault();
          if (!ready) return;
          if (step === 4) finish(true);
          else if (step === 0) next();
        }}
      >
        <Card asChild key={step}>
          <section className="setup-body">{bodies[step]}</section>
        </Card>
        <div className="setup-actions">
          <Button
            type="button"
            variant="ghost"
            onClick={() => (step ? setStep(step - 1) : go(routes.home()))}
          >
            {step ? 'Back' : 'Cancel'}
          </Button>
          {step === 4 && (
            <Button
              type="button"
              variant="outline"
              disabled={!addable}
              onClick={() =>
                kind === 'known' && known ? go(routes.project(known.id)) : finish(false)
              }
            >
              {kind === 'known' ? 'Open project' : 'Add project'}
            </Button>
          )}
          {step === 4 && kind === 'known' && known && !!counts?.trusted && (
            <Button type="button" disabled={!addable} onClick={() => workFromIssues(known.id)}>
              Work from the open issues
            </Button>
          )}
          {step !== 3 && (
            <Button
              type={step === 4 || step === 0 ? 'submit' : 'button'}
              // Until GitHub is connected, opening GitHub is the step's main action, not skipping it.
              variant={step === 2 && !connected ? 'outline' : 'default'}
              onClick={step === 1 || step === 2 ? next : undefined}
              disabled={!ready}
            >
              {step === 4
                ? kind === 'known'
                  ? 'Set goal & see plan'
                  : 'Create project & see plan'
                : step === 1 || step === 2
                  ? continueLabel(step === 1 ? signedIn : connected)
                  : 'Continue'}
              <ArrowRight size={15} />
            </Button>
          )}
        </div>
      </form>
    </div>
  );
}

/**
 * What dvb found in a project's open issues: as read when it was added here, otherwise read now,
 * loading until the read for this project ends. A failed read shows nothing, and the step stays as
 * without issues.
 */
function useIssueCounts(project: Project | undefined, added: Project | undefined) {
  const { data: s } = useStore();
  const fresh = project && project.id === added?.id ? added.issues_found : undefined;
  const id = project && !fresh ? project.id : undefined;
  const [read, setRead] = useState<{ id: string; counts?: IssueCounts }>();
  useEffect(() => {
    if (!id) return;
    let alive = true;
    const done = (counts?: IssueCounts) => alive && setRead({ id, counts });
    s.issueCounts(id).then(
      (result) => done(result.ok ? result.value : undefined),
      () => done(),
    );
    return () => {
      alive = false;
    };
  }, [id, s.issueCounts]);
  if (!id) return { counts: fresh ?? undefined, loading: false };
  return read?.id === id ? { counts: read.counts, loading: false } : { loading: true };
}
