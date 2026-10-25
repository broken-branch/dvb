import type { Task, TaskState, Event } from './types';
import type { BoardTask, BoardStatus } from '../board';
import { stateLabels } from './display';
import { taskNote } from './work';
const column: Record<TaskState['state'], BoardStatus> = {
  planned: 'planned',
  preparing: 'working',
  running: 'working',
  collecting: 'working',
  publishing: 'working',
  in_review: 'review',
  awaiting_checks: 'checks',
  merging: 'ready',
  retry_pending: 'blocked',
  stopped: 'stopped',
  unknown: 'blocked',
  done: 'done',
  cancelled: 'done',
  failed: 'blocked',
};

/** Current-head observations can override the lifecycle column, never terminal cards. */
export function boardCard(task: Task, evidence: Event[], tasks: Task[] = []): BoardTask {
  const check = evidence.findLast(
    (e) =>
      e.kind === 'checks.observed' && e.payload.task === task.id && e.payload.head === task.head,
  );
  const verdict = evidence.findLast(
    (e) =>
      e.kind === 'verdict.recorded' && e.payload.task === task.id && e.payload.head === task.head,
  );
  const finding =
    verdict?.kind === 'verdict.recorded' ? verdict.payload.findings.find((f) => f.open) : undefined;
  let status = column[task.state.state];
  const reasons: Partial<Record<TaskState['state'], string | undefined>> = {
    cancelled: task.reason ? `${stateLabels.cancelled}: ${task.reason}` : stateLabels.cancelled,
    failed: task.state.state === 'failed' ? task.state.reason : undefined,
    unknown: stateLabels.unknown,
    retry_pending: stateLabels.retry_pending,
    in_review: task.reason || undefined,
    preparing: task.reason || undefined,
  };
  let reason = taskNote(tasks, task) || reasons[task.state.state];
  if (
    check?.kind === 'checks.observed' &&
    check.payload.status === 'failed' &&
    !['done', 'cancelled'].includes(task.state.state)
  ) {
    status = 'blocked';
    reason = 'Failing CI';
  } else if (finding && task.state.state === 'in_review') {
    status = 'changes';
    reason = finding.title;
  }
  return {
    id: task.id,
    project: task.project,
    title: task.title,
    status,
    role: task.role,
    ci: check?.kind === 'checks.observed' ? check.payload.status : 'Not reported',
    reason,
    pr: task.pr || undefined,
    routine: !!task.routine,
    roundModel: task.round_model || undefined,
  };
}

export function completionOrder(events: Event[]) {
  const order = new Map<string, number>();
  for (const e of events) {
    if (
      (e.kind === 'task.done' ||
        e.kind === 'task.cancelled' ||
        (e.kind === 'task.state' && ['done', 'cancelled'].includes(e.payload.to.state))) &&
      'task' in e.payload
    )
      order.set(e.payload.task, Math.max(e.id, order.get(e.payload.task) ?? 0));
  }
  return order;
}

const sameCard = (a: BoardTask, b: BoardTask) =>
  a.id === b.id &&
  a.project === b.project &&
  a.title === b.title &&
  a.status === b.status &&
  a.role === b.role &&
  a.ci === b.ci &&
  a.reason === b.reason &&
  a.pr === b.pr &&
  a.routine === b.routine &&
  a.roundModel === b.roundModel;
/** Retain card identities until a displayed fact changes, including across fresh snapshots. */
export function createBoardCards() {
  let previous = new Map<string, BoardTask>();
  return (tasks: Task[], evidence: Event[]) => {
    const next = tasks.map((task) => {
      const card = boardCard(task, evidence, tasks),
        old = previous.get(task.id);
      return old && sameCard(old, card) ? old : card;
    });
    previous = new Map(next.map((card) => [card.id, card]));
    return next;
  };
}
