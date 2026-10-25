import { useStore } from '@/lib/store';
import { useTaskEvents } from '@/lib/data/pages';
import { taskRunIds } from '@/lib/data/selectors';
import { useRead } from '@/components/dvb/data-feedback';
export function useTask(id: string) {
  const { data: s, canWrite, settings } = useStore(),
    task = s.tasks.find((t) => t.id === id);

  const { events: detailEvents, error: detailError } = useTaskEvents(id);
  const runIds = taskRunIds(detailEvents, id),
    runId = runIds.at(-1);
  const builderStart = detailEvents.findLast(
      (e) => e.kind === 'run.started' && e.payload.task === id && !e.payload.reviews,
    ),
    builderId =
      (builderStart?.kind === 'run.started' ? builderStart.payload.run : undefined) || runId;
  const events = detailEvents;
  const rerouted = events.findLast((e) => e.kind === 'run.rerouted' && e.payload.run === builderId);
  const run = useRead(
    () =>
      builderId
        ? s.run(builderId)
        : Promise.resolve({ ok: false, refusal: { code: 'NOT_A_RUN', detail: 'No run yet.' } }),
    [id, builderId, rerouted?.id],
  );
  const output = useRead(
    () => (runId ? s.runOutput(runId) : Promise.resolve({ ok: true, value: [] })),
    [
      id,
      runId,
      s.events.findLast((e) => e.kind === 'run.output' && e.payload.run === runId)?.id,
      s.connection.state,
    ],
  );
  const diff = useRead(() => s.diff(id), [id, task?.head]);
  const verdict = useRead(
    () => s.verdict(id),
    [id, task?.head, events.filter((e) => e.kind === 'verdict.recorded').length],
  );
  const panels = settings.task_panels,
    reverse = settings.task_files_right;
  const summary = detailEvents.findLast(
    (e) => e.kind === 'plan.proposed' && e.payload.tasks.some((t) => t.task === id),
  );
  const purpose =
    summary?.kind === 'plan.proposed'
      ? summary.payload.tasks.find((t) => t.task === id)?.summary
      : undefined;
  const role = (s.roles[task?.project || ''] || []).find(
    (r) => r.id === task?.role || r.slug === task?.role,
  );
  const modelEvent = events.findLast(
    (e) =>
      e.kind === 'run.model' &&
      e.payload.run === runId &&
      typeof e.payload.served === 'object' &&
      'mismatch' in e.payload.served,
  );
  const mismatch =
    modelEvent?.kind === 'run.model' &&
    typeof modelEvent.payload.served === 'object' &&
    'mismatch' in modelEvent.payload.served
      ? { requested: modelEvent.payload.requested, served: modelEvent.payload.served.mismatch }
      : undefined;
  const check = events.findLast(
    (e) => e.kind === 'checks.observed' && e.payload.head === task?.head,
  );
  const pr = events.findLast((e) => e.kind === 'pr.opened' && e.payload.pr === task?.pr);

  return {
    s,
    canWrite,
    task,
    events,
    detailError,
    runIds,
    run,
    output,
    diff,
    verdict,
    panels,
    reverse,
    purpose,
    role,
    mismatch,
    check,
    pr,
    rerouted,
  };
}
