import { routes } from '@/lib/routes';
import { TextDisclosure } from '@/components/dvb/text-disclosure';
import { mirrorObservation, stateLabels, word } from '@/lib/data/display';
import { useEffect, useRef, useState } from 'react';
import { toast } from 'sonner';
import { ListPlus, Pause, Play, Plus } from 'lucide-react';
import { Card } from '@/components/dvb/card';
import { Heading, Status, PageTabs, Meter } from '@/components/dvb/common';
import { ProjectChecks, useProjectChecks } from '@/components/dvb/project-checks';
import { GithubRepositories, GithubSetup } from '@/screens/github';
import { ProjectLabel } from '@/components/dvb/project-identity';
import { GoalIssues } from '@/components/dvb/goal-issues';
import { TaskBoard } from '@/components/dvb/task-board';
import { DecisionList } from '@/components/dvb/decision-list';
import {
  DataError,
  toggleWork,
  OlderEvents,
  PagedFeed,
  useAction,
  useRead,
} from '@/components/dvb/data-feedback';
import { Button } from '@/components/ui/button';
import { Textarea } from '@/components/ui/textarea';
import { Switch } from '@/components/ui/switch';
import { RadioGroup, RadioGroupItem } from '@/components/ui/radio-group';
import { Input } from '@/components/ui/input';
import { Dialog, DialogContent, DialogHeader, DialogTitle } from '@/components/ui/dialog';
import { go, useStore } from '@/lib/store';
import type { Goal, Project, Question } from '@/lib/data/types';
import { currentGoals, projectGoal, lastGoalText } from '@/lib/data/selectors';
import { useEventPages } from '@/lib/data/pages';
import { failedGoal, goalTitle, needLabels, problems, taskNote, workLabels } from '@/lib/data/work';
/** The goal editor: a first or new goal, a goal after the current ones, or a new course for one goal. */
type GoalEdit = { kind: 'set' | 'next' } | { kind: 'course'; goal: Goal };
export function ProjectPage({ id, tab = 'board' }: { id: string; tab?: string }) {
  const { data: s, canWrite, evidence } = useStore(),
    p = s.projects.find((p) => p.id === id),
    order = s.activity[id]?.goals,
    goals = currentGoals(s.goals, id, order),
    { current: newest, last } = projectGoal(s.goals, id, order),
    activeGoal = goals[0],
    goal = activeGoal || last,
    openQuestions = s.questions.filter((q) => q.project === id && q.open);
  const [edit, setEdit] = useState<GoalEdit>(),
    [draft, setDraft] = useState(''),
    [issues, setIssues] = useState<number[]>([]),
    [startNow, setStartNow] = useState(false),
    [resumed, setResumed] = useState<string>(),
    opener = useRef<HTMLElement | null>(null),
    action = useAction(),
    goalAction = useAction(),
    resumeAction = useAction();

  const connected = canWrite,
    markSeen = s.markProjectSeen;
  useEffect(() => {
    if (p && connected)
      markSeen(id).catch(() => {
        /* Home keeps its count until the next visit. */
      });
  }, [id, !!p, connected]);
  if (!p)
    return (
      <Card className="empty-state">
        <h1>Project unavailable.</h1>
      </Card>
    );
  const disabled = action.busy || !canWrite,
    // A failed goal stays the owner's to retry or drop while its failed tasks still count.
    failed = activeGoal ? undefined : failedGoal(s, p),
    dropping = activeGoal || failed,
    // With several current goals each card carries its own Change course and Drop goal.
    perGoal = goals.length > 1;
  const openEditor = (next: GoalEdit, from: HTMLElement) => {
    opener.current = from;
    setDraft(next.kind === 'course' ? next.goal.text : '');
    setIssues([]);
    setStartNow(false);
    setEdit(next);
  };
  const closeEditor = () => {
    setEdit(undefined);
    if (opener.current?.isConnected) opener.current.focus();
  };
  const dropButton = (g: Goal, named: boolean) => (
    <Button
      variant="outline"
      disabled={disabled}
      aria-label={named ? `Drop goal: ${goalTitle(g.text)}` : undefined}
      title="Stop working on this goal. Work already saved stays."
      onClick={() => void action.act(() => s.dropGoal(g.id))}
    >
      Drop goal
    </Button>
  );
  // A plan question sits on the card of the goal its planned tasks belong to.
  const questionGoal = (q: Question) =>
    s.tasks.find((t) => q.plan?.some((planned) => planned.task === t.id))?.goal;
  const editor = edit && (
    <form
      key={edit.kind === 'course' ? edit.goal.id : edit.kind}
      onKeyDown={(e) => {
        if (e.key !== 'Escape') return;
        e.preventDefault();
        closeEditor();
      }}
      onSubmit={(e) => {
        e.preventDefault();
        const text = draft.trim(),
          body = issues.length ? { text, issues } : { text };
        void goalAction.act(
          () =>
            edit.kind === 'course'
              ? s.redirect(edit.goal.id, { text })
              : s.stateGoal(
                  id,
                  edit.kind === 'next' && newest && !startNow
                    ? { ...body, after: newest.id }
                    : body,
                ),
          closeEditor,
        );
      }}
    >
      <label className="field">
        {edit.kind === 'next' ? 'Next goal' : edit.kind === 'set' && last ? 'New goal' : 'Goal'}
        <Textarea
          autoFocus
          className="written-input"
          required={!issues.length}
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
        />
      </label>
      {edit.kind === 'next' && (
        <RadioGroup
          className="actions"
          aria-label="When it starts"
          value={startNow ? 'now' : 'after'}
          onValueChange={(value) => setStartNow(value === 'now')}
          orientation="horizontal"
        >
          {(
            [
              ['after', 'Start after the current goal'],
              ['now', 'Start now'],
            ] as const
          ).map(([value, label]) => (
            <RadioGroupItem asChild key={value} value={value}>
              <Button
                type="button"
                variant={(value === 'now') === startNow ? 'default' : 'outline'}
              >
                {label}
              </Button>
            </RadioGroupItem>
          ))}
        </RadioGroup>
      )}
      {edit.kind !== 'course' && <GoalIssues project={id} picked={issues} onChange={setIssues} />}
      <DataError message={goalAction.error} />
      <div className="actions">
        <Button disabled={disabled || goalAction.busy || (!draft.trim() && !issues.length)}>
          {edit.kind === 'next' && !startNow
            ? 'Save next goal'
            : edit.kind !== 'course' && last
              ? 'Save new goal'
              : 'Save direction'}
        </Button>
        <Button type="button" variant="ghost" onClick={closeEditor}>
          Cancel
        </Button>
      </div>
    </form>
  );
  const goalCard = (g: Goal | undefined, first: boolean) => {
    const tasks = g ? s.tasks.filter((t) => t.goal === g.id) : [],
      completed = tasks.filter((t) => t.state.state === 'done').length,
      questions = openQuestions.filter((q) => {
        const owner = questionGoal(q);
        return owner && goals.some((c) => c.id === owner) ? owner === g?.id : first;
      }),
      planWaits =
        g?.state === 'stated' &&
        tasks.length > 0 &&
        questions.some((q) => q.options.some((o) => o.approves_plan)),
      prior = g?.waits_for && s.goals.find((w) => w.id === g.waits_for),
      shown = g || last,
      ended = g ? undefined : failed || last;
    return (
      <Card key={g?.id || 'goal'} className="goal-section">
        <div className="goal-copy">
          {g?.waits_for ? (
            <Status>
              {word('task.waiting-for', {
                tasks: prior ? goalTitle(prior.text) : g.waits_for,
              })}
            </Status>
          ) : planWaits ? (
            <Status tone={workLabels.attention.tone}>{needLabels.question}</Status>
          ) : g?.plan_parked ? (
            <Status>{workLabels.parked.label}</Status>
          ) : (
            <Status>{g ? word(`goal_current.${g.state}`) : lastGoalText(ended)}</Status>
          )}
          {shown && mirrorObservation(evidence, shown.id)?.mirrored === false && (
            <Status tone="warn">Not mirrored on GitHub</Status>
          )}
          <TextDisclosure
            as="h2"
            className="project-page-goal"
            text={g?.text || (ended ? goalTitle(ended.text) : 'What do you want to achieve?')}
          />
          {perGoal && g && (
            <div className="actions">
              <Button
                variant="ghost"
                disabled={disabled}
                aria-label={`Change course: ${goalTitle(g.text)}`}
                onClick={(e) => openEditor({ kind: 'course', goal: g }, e.currentTarget)}
              >
                Change course
              </Button>
              {dropButton(g, true)}
            </div>
          )}
          {!g &&
            failed &&
            problems(s, p)
              .filter((t) => t.goal === failed.id)
              .map((t) => (
                <p key={t.id} className="goal-task-note">
                  <a className="task-written-text" href={`#${routes.task(t.id)}`}>
                    {t.title}
                  </a>
                  : {taskNote(s.tasks, t) || stateLabels[t.state.state]}
                </p>
              ))}
          {g?.plan_parked && (
            <>
              <p>The plan waits until you ask for it again.</p>
              {resumed === g.id && <DataError message={resumeAction.error} />}
              <div className="actions">
                <Button
                  variant="outline"
                  disabled={disabled || resumeAction.busy}
                  aria-label={perGoal ? `Ask me again: ${goalTitle(g.text)}` : undefined}
                  title="Ask the plan's question again. The plan stays as it is."
                  onClick={() => {
                    setResumed(g.id);
                    void resumeAction.act(() => s.resumeGoal(g.id));
                  }}
                >
                  Ask me again
                </Button>
              </div>
            </>
          )}
          {(edit?.kind === 'course' ? edit.goal.id === g?.id : first) && editor}
          {questions.map((q) => (
            <Button
              className="written-subject goal-question"
              key={q.id}
              variant="outline"
              onClick={() => go(routes.decision(q.id, { page: 'project', id }))}
            >
              {q.subject}
            </Button>
          ))}
          {tasks.map((t) => {
            const note = taskNote(s.tasks, t);
            return (
              note && (
                <p key={t.id} className="goal-task-note">
                  <a className="task-written-text" href={`#${routes.task(t.id)}`}>
                    {t.title}
                  </a>
                  : {note}
                </p>
              )
            );
          })}
        </div>
        {g && (
          <div className="goal-progress">
            <span className="progress-fraction">
              {completed}
              <span> / {tasks.length}</span>
            </span>
            <p>tasks completed</p>
            <Meter
              value={tasks.length ? (completed / tasks.length) * 100 : 0}
              label="Goal completion"
            />
          </div>
        )}
      </Card>
    );
  };

  return (
    <>
      <Heading title={<ProjectLabel id={id} />} description={p.repo}>
        {p.ci === 'no_ci' && <Status tone="warn">No CI</Status>}
        <Button
          variant="outline"
          disabled={disabled}
          onClick={() => void action.act(() => toggleWork(s, p))}
        >
          {p.paused ? <Play size={14} /> : <Pause size={14} />}
          {p.paused ? 'Resume' : 'Pause'}
        </Button>
        {!perGoal && (
          <Button
            variant="outline"
            disabled={disabled}
            onClick={(e) =>
              openEditor(
                activeGoal ? { kind: 'course', goal: activeGoal } : { kind: 'set' },
                e.currentTarget,
              )
            }
          >
            <Plus size={14} />
            {activeGoal ? 'Change course' : goal ? 'Set a new goal' : 'Set a goal'}
          </Button>
        )}
        {activeGoal && (
          <Button
            variant="outline"
            disabled={disabled}
            title="State a goal that starts after the current one is done."
            onClick={(e) => openEditor({ kind: 'next' }, e.currentTarget)}
          >
            <ListPlus size={14} />
            Next goal
          </Button>
        )}
        {dropping && !perGoal && dropButton(dropping, false)}
      </Heading>
      <DataError message={action.error} />
      {p.paused && (
        <Card className="notice">
          <Status tone="warn">Project paused</Status>
          <p>New work waits. Your work is saved.</p>
        </Card>
      )}
      {goals.length ? goals.map((g, i) => goalCard(g, i === 0)) : goalCard(undefined, true)}
      <PageTabs
        value={tab}
        onChange={(value) => {
          go(routes.project(id, value));
        }}
        items={[
          { id: 'board', label: 'Board', content: <TaskBoard project={id} /> },
          {
            id: 'conversation',
            label: 'Conversation',
            content: <ProjectConversation project={id} />,
          },
          { id: 'decisions', label: 'Team decisions', content: <ProjectDecisions project={id} /> },
          { id: 'activity', label: 'Activity', content: <ProjectActivity project={id} /> },
          {
            id: 'brief',
            label: 'Brief',
            content: <BriefEditor project={id} />,
          },
          {
            id: 'settings',
            label: 'Settings',
            content: (
              <div className="project-settings">
                <Card>
                  <h2>Checks</h2>
                  <p>
                    {p.ci === 'no_ci'
                      ? 'No CI. This project was explicitly configured without checks.'
                      : `Expected: ${p.ci.expected.join(', ')}`}
                  </p>
                  <p>Default branch: {p.default_branch}</p>
                </Card>
                <Autonomy name={p.name} />
                <RemoveProject project={p} />
              </div>
            ),
          },
        ]}
      />
    </>
  );
}
/** Adds or removes this project in the `autonomous_projects` setting. */
function Autonomy({ name }: { name: string }) {
  const { data: s, canWrite, settings } = useStore(),
    action = useAction(),
    names = settings.autonomous_projects,
    on = names.includes(name);
  return (
    <Card>
      <h2>Autonomy</h2>
      <label className="switch-row">
        Autonomous
        <Switch
          checked={on}
          disabled={action.busy || !canWrite}
          aria-describedby="project-autonomy"
          onCheckedChange={(value) =>
            void action.act(() =>
              s.changeSettings({
                values: {
                  autonomous_projects: value ? [...names, name] : names.filter((n) => n !== name),
                },
              }),
            )
          }
        />
      </label>
      <p id="project-autonomy">
        dvb approves its own plans and starts proposed work from trusted issues. Failures,
        merge-anyway and scope changes still come to you.
      </p>
      <DataError message={action.error} />
    </Card>
  );
}
/** Removes the project from this install after one confirm that names it. */
function RemoveProject({ project }: { project: Project }) {
  const { data: s, canWrite } = useStore(),
    action = useAction(),
    opener = useRef<HTMLButtonElement>(null),
    [confirming, setConfirming] = useState(false);
  return (
    <Card>
      <h2>Remove project</h2>
      <p>
        dvb stops tracking this project and frees its files on this computer. Nothing changes on
        GitHub. A project with running or open tasks cannot be removed.
      </p>
      <div className="actions">
        <Button
          ref={opener}
          variant="outline"
          disabled={!canWrite}
          onClick={() => setConfirming(true)}
        >
          Remove
        </Button>
      </div>
      <Dialog open={confirming} onOpenChange={setConfirming}>
        <DialogContent
          aria-describedby="remove-project"
          onCloseAutoFocus={(event) => {
            event.preventDefault();
            opener.current?.focus();
          }}
        >
          <DialogHeader>
            <DialogTitle>Remove {project.name}?</DialogTitle>
          </DialogHeader>
          <p id="remove-project">
            {project.name} ({project.repo}) leaves dvb on this computer. Its earlier activity stays
            in the record and nothing changes on GitHub. Stopped tasks' unpublished work on this
            computer is deleted.
          </p>
          <DataError message={action.error} />
          <div className="actions">
            <Button
              disabled={action.busy || !canWrite}
              onClick={() =>
                void action.act(
                  () => s.removeProject(project.id),
                  (removal) => {
                    toast.success(word('project.removed', removal.project));
                    if (removal.not_freed.length)
                      toast.error(`Not freed: ${removal.not_freed.join('; ')}`);
                    go(routes.home());
                  },
                )
              }
            >
              Remove {project.name}
            </Button>
            <Button variant="ghost" onClick={() => setConfirming(false)}>
              Cancel
            </Button>
          </div>
        </DialogContent>
      </Dialog>
    </Card>
  );
}
function BriefEditor({ project }: { project: string }) {
  const { data: s, canWrite } = useStore(),
    action = useAction(),
    [saved, setSaved] = useState(false);
  const { value, error } = useRead(() => s.brief(project), [project]);
  const [draft, setDraft] = useState<string>();
  const text = draft ?? value?.text ?? '';
  return (
    <Card>
      <h2>Brief</h2>
      <p>
        Notes for the team about this project: what it is for, what to keep in mind, what to avoid.
        It stays private to you and your team and is never posted to GitHub. Leave it empty to clear
        it.
      </p>
      <DataError message={error} />
      <form
        onSubmit={(e) => {
          e.preventDefault();
          setSaved(false);
          void action.act(
            () => s.setBrief(project, { text }),
            (b) => {
              setDraft(b.text);
              setSaved(true);
            },
          );
        }}
      >
        <label className="field">
          Project brief
          <Textarea
            disabled={!value && !error}
            value={text}
            onChange={(e) => {
              setDraft(e.target.value);
              setSaved(false);
            }}
          />
        </label>
        <DataError message={action.error} />
        <div className="actions">
          <Button disabled={action.busy || !value || !canWrite}>Save brief</Button>
          {saved && <span role="status">Brief saved.</span>}
        </div>
      </form>
    </Card>
  );
}
export function AddProject() {
  const { data: s, canWrite } = useStore(),
    action = useAction(),
    [name, setName] = useState(''),
    [repo, setRepo] = useState(''),
    [githubOverride, setGithubReady] = useState<boolean>(),
    [attempt, setAttempt] = useState(0);
  const checks = useProjectChecks(repo);
  const connection = useRead(() => s.github(), [s.github, attempt]);
  const githubReady = githubOverride ?? connection.value?.connected;
  useEffect(() => {
    if (githubReady) checks.refresh();
  }, [githubReady, checks.refresh]);
  return (
    <>
      <Heading title="Add a project" description="Connect an existing repository." />
      {githubReady === undefined ? (
        <Card>
          {connection.error ? (
            <>
              <DataError message={connection.error} />
              <Button onClick={() => setAttempt(attempt + 1)}>Retry GitHub connection</Button>
            </>
          ) : (
            <p role="status">Checking GitHub connection…</p>
          )}
        </Card>
      ) : !githubReady ? (
        <>
          <p>Connect and install the App to continue. Your project draft is kept here.</p>
          <GithubSetup
            onChange={(connected) => {
              if (connected) setGithubReady(true);
            }}
          />
        </>
      ) : (
        <Card>
          <form
            onSubmit={(e) => {
              e.preventDefault();
              if (!checks.ready || !canWrite) return;
              void action.act<Project | null>(
                async () => {
                  const connection = await s.github();
                  if (!connection.ok) return connection;
                  if (!connection.value.connected) {
                    setGithubReady(false);
                    return { ok: true as const, value: null };
                  }
                  return s.addProject({
                    name,
                    repo,
                    ci: checks.ci,
                  });
                },
                (p) => {
                  if (p) go(routes.project(p.id));
                },
              );
            }}
          >
            <label className="field">
              Name
              <Input required value={name} onChange={(e) => setName(e.target.value)} />
            </label>
            <label className="field">
              Repository
              <Input
                required
                value={repo}
                onChange={(e) => setRepo(e.target.value)}
                placeholder="owner/repository"
              />
            </label>
            <GithubRepositories onRefresh={checks.refresh} />
            <ProjectChecks checks={checks} />
            <DataError message={action.error} />
            <div className="actions add-project-actions">
              <Button
                type="button"
                variant="outline"
                onClick={() => {
                  setGithubReady(undefined);
                  setAttempt((n) => n + 1);
                }}
              >
                Check GitHub connection
              </Button>
              <Button disabled={action.busy || !checks.ready || !canWrite}>Add project</Button>
            </div>
          </form>
        </Card>
      )}
    </>
  );
}
const talk = [
  {
    key: 'tasks',
    label: 'Task updates',
    kinds: [
      'task.started',
      'task.stopped',
      'task.resumed',
      'task.cancelled',
      'task.done',
      'task.failed',
      'pr.opened',
      'pr.merged',
      'run.output',
    ],
  },
  { key: 'phases', label: 'Phase completions', kinds: ['goal.approved', 'goal.done'] },
  { key: 'decisions', label: 'Team decisions', kinds: ['decision.recorded'] },
];
function Conversation({ page }: { page: ReturnType<typeof useEventPages> }) {
  const [shown, setShown] = useState<Record<string, boolean>>({
    tasks: true,
    phases: true,
    decisions: true,
  });
  const kinds = talk.filter((t) => shown[t.key]).flatMap((t) => t.kinds);
  return (
    <>
      <div className="conversation-toggles">
        {talk.map((t) => (
          <label key={t.key} className="switch-row">
            {t.label}
            <Switch
              checked={shown[t.key]}
              onCheckedChange={(value) => setShown({ ...shown, [t.key]: value })}
            />
          </label>
        ))}
      </div>
      <PagedFeed
        page={page}
        events={page.events.filter(
          (e) =>
            ['question.opened', 'question.answered', 'goal.stated'].includes(e.kind) ||
            kinds.includes(e.kind),
        )}
      />
    </>
  );
}

function ProjectActivity({ project }: { project: string }) {
  const page = useEventPages({ project });
  return <PagedFeed page={page} events={page.events} />;
}
function ProjectConversation({ project }: { project: string }) {
  const page = useEventPages({
    project,
    kinds:
      'question.opened,question.answered,goal.stated,' + talk.flatMap((t) => t.kinds).join(','),
  });
  return <Conversation page={page} />;
}
function ProjectDecisions({ project }: { project: string }) {
  const { data: s } = useStore(),
    page = useEventPages({ project, kinds: 'decision.recorded' });
  return (
    <>
      <DecisionList events={page.events} roles={s.roles[project] || []} />
      <OlderEvents page={page} />
    </>
  );
}
