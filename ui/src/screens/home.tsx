import { routes } from '@/lib/routes';
import { Fragment, useState } from 'react';
import { useWorkClock } from '@/lib/data/use-work-clock';
import { ArrowRight, ArrowUpRight, Plus } from 'lucide-react';
import { Card } from '@/components/dvb/card';
import { Heading, SectionTitle, Status, Meter } from '@/components/dvb/common';
import { ProjectLabel } from '@/components/dvb/project-identity';
import { Button } from '@/components/ui/button';
import { useRead } from '@/components/dvb/data-feedback';
import { go, useStore } from '@/lib/store';
import { stateLabels } from '@/lib/data/display';
import type { Project } from '@/lib/data/types';
import {
  activeTask,
  duration,
  failedGoal,
  goalTitle,
  lastWork,
  overviewColumns,
  problems,
  progress,
  projectStatus,
  quietProject,
  relative,
  sortProjects,
  taskList,
  waitingText,
  workDate,
  type SortColumn,
} from '@/lib/data/work';
export function ProjectCard({ project: p }: { project: Project }) {
  const { data: s } = useStore(),
    now = useWorkClock(),
    t = activeTask(s, p),
    g = progress(s, p),
    finished = p.last_finished,
    failed = failedGoal(s, p),
    failedTasks = problems(s, p).filter((f) => f.goal === failed?.id);
  const role = s.roles[p.id]?.find((r) => r.id === (t?.current_run?.role || t?.role));
  const waiting = s.tasks.flatMap((w) => {
    const text = w.goal === g.goal?.id ? waitingText(s.tasks, w) : undefined;
    return text ? [{ id: w.id, title: w.title, text }] : [];
  });
  return (
    <Card className="project-card now-card">
      <a href={`#${routes.project(p.id)}`} className="project-card-link">
        <div className="card-top">
          <h3>
            <ProjectLabel id={p.id} />
          </h3>
          <Status tone={projectStatus(s, p, now, true).tone}>
            {projectStatus(s, p, now, true).label}
          </Status>
          <ArrowUpRight size={18} />
        </div>
        {t ? (
          <>
            <p className="now-goal" title={g.goal?.text}>
              {goalTitle(g.goal?.text || 'No goal yet')}
            </p>
            <hr />
            <p className="now-task" title={t.title}>
              {t.title}
            </p>
            <p className="work-facts actions">
              <span>{stateLabels[t.state.state]}</span>
              <span>{role?.name || 'Role unassigned'}</span>
              <span>{t.current_run?.model || 'Model unassigned'}</span>
              {t.current_run && <span>Running for {duration(t.current_run.started_at, now)}</span>}
            </p>
            <div className="project-bottom">
              <span>
                {g.done} of {g.total} {g.total === 1 ? 'task' : 'tasks'}
              </span>
              <Meter value={g.percent} label={`${p.name} progress`} />
            </div>
          </>
        ) : failed ? (
          <>
            <p className="now-goal" title={failed.text}>
              {goalTitle(failed.text)}
            </p>
            <p className="now-task">
              {failedTasks.length === 1 ? 'Failed or stopped task' : 'Failed or stopped tasks'}:{' '}
              {taskList(failedTasks.map((f) => f.title))}
            </p>
          </>
        ) : (
          <>
            <p className="now-goal" title={finished?.text}>
              {finished
                ? finished.pr
                  ? finished.text
                  : goalTitle(finished.text)
                : 'No finished work yet'}
              {finished?.pr && ` merged PR ${finished.pr}`}
            </p>
            {finished && <p>Finished {workDate(finished.at)}</p>}
          </>
        )}
        {waiting.map((w) => (
          <p key={w.id} className="now-waiting work-facts" title={w.title}>
            {w.title}: {w.text}
          </p>
        ))}
        <small>Last work {relative(p.last_work_at, now)}</small>
      </a>
    </Card>
  );
}
/**
 * Points at setup until GitHub is connected: a link, not a redirect, so Home stays usable and Back from setup
 * does not bounce. Hidden while the status loads; an unreadable status still shows it, since setup is where
 * GitHub is checked and connected.
 */
function SetupLink() {
  const { data: s } = useStore();
  const { value, error } = useRead(() => s.github(), [s.github]);
  if (value?.connected || (!value && !error)) return null;
  return (
    <Button asChild className="setup-link">
      <a href={`#${routes.setup()}`}>
        Set up dvb
        <ArrowRight size={15} />
      </a>
    </Button>
  );
}
export function Home() {
  const {
      data: s,
      needsYou: { questions, faults },
    } = useStore(),
    now = useWorkClock();
  const projects = s.projects;
  const working = projects.filter((p) => activeTask(s, p));
  const idle = projects.toSorted((a, b) => lastWork(b) - lastWork(a)).slice(0, 3);
  if (!projects.length)
    return (
      <>
        <Heading title="Now" />
        <SetupLink />
        <Card>
          <h2>Ready for a first goal</h2>
          <Button onClick={() => go(routes.project('new'))}>
            <Plus size={15} />
            Add project
          </Button>
        </Card>
      </>
    );
  return (
    <>
      <Heading title="Now" />
      <SetupLink />
      {!!(questions.length + faults.length) && (
        <section className="attention-section" id="needs-you" tabIndex={-1}>
          <SectionTitle title="Needs you" />
          {questions.map((q) => (
            <Card asChild key={q.id} density="compact">
              <a href={`#${routes.decision(q.id)}`} data-needs className="needs-row">
                <ProjectLabel id={q.project} />
                <span className="written-subject one-line" title={q.subject}>
                  {q.subject}
                </span>
                <small>Waiting {duration(q.opened_at, now)}</small>
                <span>
                  Answer <ArrowUpRight size={15} />
                </span>
              </a>
            </Card>
          ))}
          {faults.map(({ task: t, question: q }) => (
            <Card asChild key={t.id} density="compact">
              <a
                href={`#${q ? routes.decision(q.id, { page: 'task', id: t.id }) : routes.task(t.id)}`}
                data-needs
                className="needs-row"
              >
                <ProjectLabel id={t.project} />
                <span className="written-subject one-line" title={t.title}>
                  {t.title}
                </span>
                <small>{stateLabels[t.state.state]}</small>
                <span>
                  {q ? 'Answer' : 'Open'} <ArrowUpRight size={15} />
                </span>
              </a>
            </Card>
          ))}
        </section>
      )}
      <section>
        <SectionTitle
          title={working.length ? 'Working now' : 'Last worked on'}
          hint={
            working.length
              ? undefined
              : 'The three projects with the most recent work. Visits and settings do not count as work.'
          }
        />
        <div className="project-grid">
          {(working.length ? working : idle).map((p) => (
            <ProjectCard key={p.id} project={p} />
          ))}
        </div>
      </section>
    </>
  );
}
export function Projects() {
  const { data: s } = useStore(),
    now = useWorkClock();
  const [column, setColumn] = useState<SortColumn>('Last work'),
    [descending, setDescending] = useState(true),
    [quiet, setQuiet] = useState(false);
  const ordered = sortProjects(s, s.projects, column, descending, now),
    visible = ordered.filter((p) => !quietProject(p, now)),
    hidden = ordered.filter((p) => quietProject(p, now));
  const sort = (c: SortColumn) => {
    if (column === c) setDescending((v) => !v);
    else {
      setColumn(c);
      setDescending(false);
    }
  };
  const facts = (p: Project) => {
    const g = progress(s, p);
    return [
      <Fragment key="project">
        <ProjectLabel id={p.id} />
        <small className="repo-name">{p.repo}</small>
      </Fragment>,
      <Status key="state" tone={projectStatus(s, p, now).tone}>
        {projectStatus(s, p, now).label}
      </Status>,
      <span key="goal" className="overview-goal one-line" title={g.goal?.text}>
        {goalTitle(g.goal?.text || 'No goal yet')}
      </span>,
      <Fragment key="progress">
        <span>
          {g.done} of {g.total} {g.total === 1 ? 'task' : 'tasks'}
        </span>
        <Meter value={g.percent} label={`${p.name} progress`} />
      </Fragment>,
      <Fragment key="last-work">
        <time>{workDate(p.last_work_at)}</time>
        <small>{relative(p.last_work_at, now)}</small>
      </Fragment>,
      p.done_goals ?? 0,
      <span key="problems" className={problems(s, p).length ? 'warning-text' : ''}>
        {problems(s, p).length}
      </span>,
    ];
  };
  const rows = (list: Project[]) =>
    list.map((p) => (
      <tr key={p.id} onClick={() => go(routes.project(p.id))}>
        {facts(p).map((fact, i) => (
          <td key={i}>{i === 0 ? <a href={`#${routes.project(p.id)}`}>{fact}</a> : fact}</td>
        ))}
      </tr>
    ));
  const mobileRows = (projects: Project[]) =>
    projects.map((p) => (
      <Card key={p.id} className="overview-facts">
        <a href={`#${routes.project(p.id)}`}>
          <dl>
            {facts(p).map((fact, i) => (
              <div key={i}>
                <dt>{overviewColumns[i]}</dt>
                <dd>{fact}</dd>
              </div>
            ))}
          </dl>
        </a>
      </Card>
    ));
  return (
    <>
      <Heading
        title="Overview"
        description="Every connected project, its progress and latest work."
      >
        <Button onClick={() => go(routes.project('new'))}>
          <Plus size={15} />
          Add project
        </Button>
      </Heading>
      <div className="overview-sort">
        <label>
          Sort by{' '}
          <select value={column} onChange={(e) => setColumn(e.target.value as SortColumn)}>
            {overviewColumns.map((c) => (
              <option key={c}>{c}</option>
            ))}
          </select>
        </label>
        <Button variant="outline" onClick={() => setDescending((v) => !v)}>
          {descending ? 'Descending' : 'Ascending'}
        </Button>
      </div>
      <div className="overview-table-scroll">
        <table className="overview-table">
          <thead>
            <tr>
              {overviewColumns.map((c) => (
                <th
                  key={c}
                  scope="col"
                  aria-sort={column === c ? (descending ? 'descending' : 'ascending') : 'none'}
                >
                  <button onClick={() => sort(c)}>
                    {c}
                    {column === c ? (descending ? ' ↓' : ' ↑') : ''}
                  </button>
                </th>
              ))}
            </tr>
          </thead>
          <tbody>{rows(visible)}</tbody>
          {hidden.length > 0 && (
            <>
              <tbody>
                <tr>
                  <td colSpan={7}>
                    <button
                      className="quiet-toggle"
                      aria-expanded={quiet}
                      onClick={() => setQuiet((v) => !v)}
                    >
                      Quiet ({hidden.length}) {quiet ? '−' : '+'}
                    </button>
                  </td>
                </tr>
              </tbody>
              {quiet && <tbody>{rows(hidden)}</tbody>}
            </>
          )}
        </table>
      </div>
      <div className="overview-stacked">
        {mobileRows(visible)}
        {hidden.length > 0 && (
          <>
            <button
              className="quiet-toggle"
              aria-expanded={quiet}
              onClick={() => setQuiet((v) => !v)}
            >
              Quiet ({hidden.length}) {quiet ? '−' : '+'}
            </button>
            {quiet && mobileRows(hidden)}
          </>
        )}
      </div>
    </>
  );
}
