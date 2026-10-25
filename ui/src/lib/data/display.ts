import type { DataStore, Event, IssueCounts, Refusal, TaskState } from './types';
import catalogue from '@/api/wording.json';

const words: Record<string, string | { cli: string; app: string } | null> = catalogue;
export function word(key: string, values: Record<string, unknown> = {}): string {
  const entry = words[key];
  const template = typeof entry === 'string' ? entry : entry?.app || '';
  return template.replace(/\{([^}]+)\}/g, (_, name: string) => String(values[name] ?? ''));
}
export const stateLabels = Object.fromEntries(
  Object.keys(words)
    .filter((key) => key.startsWith('state.'))
    .map((key) => [key.slice(6), word(key)]),
) as Record<TaskState['state'], string>;
export function refusalText(
  refusal: Refusal | Extract<Event, { kind: 'command.refused' }>['payload'],
) {
  switch (refusal.code) {
    case 'VERSION_MISMATCH':
      return 'Update dvb before making changes. Your displayed work is still available.';
    case 'PLAN_NOT_PARKED':
      return 'This plan is no longer on hold, so there is nothing to ask again.';
    default:
      return refusal.detail;
  }
}

/** What dvb found in a repository's open issues: "Open issues found: 8 (5 trusted, 3 labelled enhancement)". */
export function issuesFoundText(counts: IssueCounts) {
  const labelled = counts.labels.map((l) => word('project.issues_found.label', l)).join('');
  return word('project.issues_found', { ...counts, labelled });
}

export function outputText(
  item: { text: string } | { note: string } | { tool_call: { name: string } },
) {
  return 'text' in item
    ? item.text
    : 'note' in item
      ? item.note
      : word('run.output.tool', item.tool_call);
}
function stageText(stage?: 'code' | 'design' | null) {
  return word(`stage.${stage || 'none'}`);
}
/** The store's names for the ids an event carries; an id not found shows as itself. */
export type EventNames = Pick<DataStore, 'roles' | 'tasks' | 'questions'>;
export function eventText(e: Event, names?: EventNames): string | null {
  const values: Record<string, unknown> = { ...e.payload };
  if ('role' in e.payload && typeof e.payload.role === 'string') {
    const role = e.payload.role;
    values.role = names?.roles[e.project || '']?.find((r) => r.id === role)?.name || role;
  }
  if ('task' in e.payload && typeof e.payload.task === 'string') {
    const task = e.payload.task;
    values.task = names?.tasks.find((t) => t.id === task)?.title || task;
  }
  let key: string = e.kind;
  switch (e.kind) {
    case 'engine.reconciled':
      values.state = stateLabels[e.payload.state.state];
      break;
    case 'role.changed':
      if (e.payload.restored_from) key += '.restored';
      break;
    case 'hire.changed':
      if (e.payload.restored_from) key += '.restored';
      break;
    case 'settings.changed':
      if (e.payload.restored_from) key += '.restored';
      break;
    case 'github.mirror_observed':
      if (!e.payload.mirrored) key += e.payload.detail ? '.detail' : '.failed';
      break;
    case 'project.issues_found':
      return issuesFoundText(e.payload);
    case 'plan.proposed':
      values.count = e.payload.tasks.length;
      values.tasks = word(e.payload.tasks.length === 1 ? 'plan.tasks.one' : 'plan.tasks.many');
      break;
    case 'question.answered': {
      const { question, answer } = e.payload;
      values.answer =
        'reply' in answer
          ? answer.reply
          : names?.questions
              .find((q) => q.id === question)
              ?.options.find((o) => o.id === answer.choice)?.text || answer.choice;
      break;
    }
    case 'command.refused':
      values.refusal = refusalText(e.payload);
      break;
    case 'run.started':
      values.task ||= word('run.started.work');
      if (e.payload.review_stage) {
        key += '.review';
        values.stage = stageText(e.payload.review_stage);
      }
      break;
    case 'run.preflight': {
      const failed = e.payload.checks.find((c) => !c.ok);
      if (failed) {
        key += '.failed';
        values.failed = failed.code;
      }
      break;
    }
    case 'run.model':
      if (typeof e.payload.served === 'object' && 'mismatch' in e.payload.served) {
        key += '.mismatch';
        values.served = e.payload.served.mismatch;
      }
      break;
    case 'run.output':
      values.output = e.payload.resource
        ? e.payload.resource === 'available'
          ? word('resource.available')
          : word('resource.notice', {
              resource: word(`resource.${e.payload.resource}`),
              detail: outputText(e.payload.item),
            })
        : outputText(e.payload.item);
      break;
    case 'run.ended':
      if (e.payload.reason === 'completed') key += '.completed';
      break;
    case 'run.rerouted':
      if (!e.payload.limited) key += '.only';
      break;
    case 'only.started':
      values.models = e.payload.models.join(', ');
      values.ends_at = new Date(e.payload.ends_at).toLocaleString();
      break;
    case 'vendor.limited':
      if (e.payload.resets_at) {
        key += '.reset';
        values.resets_at = new Date(e.payload.resets_at).toLocaleString();
      }
      break;
    case 'branch.updated':
      if (e.payload.same_change) key += '.same';
      break;
    case 'verdict.recorded':
      values.open = e.payload.findings.filter((f) => f.open).length;
      values.stage = stageText(e.payload.review_stage);
      break;
    case 'effect.retry':
      values.effect = word(`effect.${e.payload.effect}`);
      break;
  }
  return words[key] === null ? null : word(key, values);
}

export function mirrorObservation(events: Event[], goal: string, task?: string) {
  const event = events.findLast(
    (e) =>
      e.kind === 'github.mirror_observed' &&
      e.payload.goal === goal &&
      (e.payload.task || undefined) === task,
  );
  return event?.kind === 'github.mirror_observed' ? event.payload : undefined;
}
