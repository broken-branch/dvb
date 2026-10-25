import { routes } from '@/lib/routes';
import { useState, useMemo, memo } from 'react';
import { ArrowUpRight, ChevronDown } from 'lucide-react';
import { Card } from './card';
import { Status } from './common';
import { projectIdentity } from './project-identity';
import { IdentityMark } from './identity-mark';
import { Button } from '@/components/ui/button';
import { useStore } from '@/lib/store';
import { boardColumns, type BoardTask } from '@/lib/board';
import { mirrorObservation } from '@/lib/data/display';
import { useEventPages } from '@/lib/data/pages';
import { mergeEvents } from '@/lib/data/selectors';
import { completionOrder } from '@/lib/data/board';
export function TaskBoard({ project }: { project?: string }) {
  const { data: s, evidence: sharedEvidence, boardCards } = useStore(),
    tasks = useMemo(
      () => boardCards.filter((task) => !project || task.project === project),
      [boardCards, project],
    ),
    [expanded, setExpanded] = useState(false);
  const completed = useEventPages({ project, kinds: 'task.done,task.cancelled', limit: 200 });
  const evidence = useMemo(
    () => mergeEvents(sharedEvidence, completed.events),
    [sharedEvidence, completed.events],
  );
  const unmirrored = s.tasks.filter(
    (t) =>
      (!project || t.project === project) &&
      mirrorObservation(evidence, t.goal, t.id)?.mirrored === false,
  ).length;
  const noCi = s.projects
    .filter((p) => (!project || p.id === project) && tasks.some((t) => t.project === p.id))
    .every((p) => p.ci === 'no_ci');
  const roles = Object.values(s.roles).flat();
  const order = useMemo(() => completionOrder(evidence), [evidence]);
  return (
    <>
      <div className="board-caption">
        {tasks.length} {tasks.length === 1 ? 'task' : 'tasks'}
        {unmirrored > 0 && <span>{unmirrored} not mirrored on GitHub</span>}
        {tasks.length > 0 && <span>{noCi ? 'No CI' : 'CI'} · Review required</span>}
      </div>
      {!tasks.length ? (
        <Card className="empty-state">
          <h3>No tasks yet.</h3>
        </Card>
      ) : (
        <div
          className="kanban-board"
          tabIndex={0}
          role="region"
          aria-label={project ? 'Project task board' : 'All projects task board'}
        >
          {boardColumns.map(([status, label, tone]) => {
            const items = tasks.filter((t) => t.status === status);
            if (status === 'done')
              items.sort(
                (a, b) =>
                  (order.get(b.id) ?? 0) - (order.get(a.id) ?? 0) || b.id.localeCompare(a.id),
              );
            if (!items.length) return null;
            return (
              <section
                className={`kanban-column ${status === 'done' ? 'compact' : ''}`}
                key={status}
                aria-label={`${label}, ${items.length} ${items.length === 1 ? 'task' : 'tasks'}`}
              >
                <div className="kanban-heading">
                  <Status tone={tone}>{label}</Status>
                  <span>{items.length}</span>
                </div>
                {status === 'done' && items.length > 10 && (
                  <Button
                    variant="ghost"
                    className="done-toggle"
                    aria-expanded={expanded}
                    onClick={() => setExpanded(!expanded)}
                  >
                    {expanded ? 'Show fewer' : `Show all ${items.length}`}{' '}
                    <ChevronDown className={expanded ? 'expanded' : ''} size={14} />
                  </Button>
                )}
                <div className="kanban-stack">
                  {(status === 'done' && !expanded ? items.slice(0, 10) : items).map((task) => {
                    const role = roles.find((r) => r.id === task.role),
                      hires = s.hires.filter((h) => h.role === role?.id && h.status === 'active'),
                      hire =
                        (task.routine && hires.find((h) => h.routine)) ||
                        hires.find((h) => !h.routine);
                    return (
                      <BoardCard
                        key={task.id}
                        task={task}
                        showProject={!project}
                        projectName={
                          s.projects.find((p) => p.id === task.project)?.name || task.project
                        }
                        projectIndex={s.projects.findIndex((p) => p.id === task.project)}
                        roleName={role?.name || 'Unknown role'}
                        model={task.roundModel || hire?.model || 'Not hired'}
                        showCi={s.projects.find((p) => p.id === task.project)?.ci !== 'no_ci'}
                      />
                    );
                  })}
                </div>
              </section>
            );
          })}
        </div>
      )}
    </>
  );
}
/** Stable cards and display primitives keep unrelated feed events outside this boundary. */
const BoardCard = memo(function BoardCard({
  task: t,
  showProject,
  projectName,
  projectIndex,
  roleName,
  model,
  showCi,
}: {
  task: BoardTask;
  showProject: boolean;
  projectName: string;
  projectIndex: number;
  roleName: string;
  model: string;
  showCi: boolean;
}) {
  return (
    <Card density="compact" className={`kanban-card ${t.status}`}>
      <div className="kanban-meta">
        {showProject && (
          <span className="project-label">
            <span className="project-suit" aria-hidden="true">
              <IdentityMark value={projectIdentity(projectIndex)} />
            </span>
            <span>{projectName}</span>
          </span>
        )}
        {t.pr && <span>#{t.pr}</span>}
        {t.routine && <span>Routine</span>}
      </div>
      <a className="kanban-title task-written-text" href={`#${routes.task(t.id)}`}>
        {t.title}
        <ArrowUpRight size={13} />
      </a>
      {t.reason && <span className="kanban-reason blocked">{t.reason}</span>}
      <div className="kanban-agent">
        <span>
          {roleName}
          <small>{model}</small>
        </span>
      </div>
      {showCi && <div className="kanban-ci">CI {t.ci}</div>}
    </Card>
  );
});
