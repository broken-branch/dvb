import { describe, expect, it } from 'vitest';
import { eventText, stateLabels, word, type EventNames } from '@/lib/data/display';
import type { Event, EventKind, Question, Role, Task } from '@/lib/data/types';
import fixtures from '../../../../crates/dvb-core/tests/fixtures/event-kinds.json';
import source from '../../../../crates/dvb-core/src/wording.json';
import generated from '@/api/wording.json';

const event = (kind: EventKind): Event => ({ id: 1, at: 1, actor: 'engine', ...kind });
describe('generated owner wording', () => {
  it('renders typed resource waits and releases while retaining historical notes', () => {
    for (const [resource, expected] of [
      ['memory', 'Waiting for memory: available bytes'],
      ['disk', 'Waiting for disk: available bytes'],
      ['host_load', 'Waiting for host load: available bytes'],
      ['available', 'Resources available; starting run.'],
    ] as const) {
      expect(
        eventText(
          event({
            kind: 'run.output',
            payload: {
              run: 'r',
              resource,
              item: { note: 'available bytes' },
            },
          }),
        ),
      ).toBe(expected);
    }
    expect(
      eventText(
        event({
          kind: 'run.output',
          payload: {
            run: 'old',
            item: { note: 'waiting for disk space: old notice' },
          },
        }),
      ),
    ).toBe('waiting for disk space: old notice');
  });
  it('covers every historical event and uses the core catalogue', () => {
    expect(generated).toEqual(source);
    for (const kind of fixtures as EventKind[]) {
      expect(Object.hasOwn(generated, kind.kind)).toBe(true);
      const text = eventText(event(kind));
      expect(text?.includes('{') ?? false).toBe(false);
      expect(text === null).toBe(generated[kind.kind] === null);
    }
    expect(stateLabels.done).toBe('Done / merged');
    expect(stateLabels.awaiting_checks).toBe('Checks running');
    expect(word('goal_state.failed')).toBe('Some tasks failed or were stopped');
  });

  it('retains restored, singular, staged and payload-dependent wording', () => {
    expect(
      eventText(
        event({
          kind: 'plan.proposed',
          payload: {
            goal: 'g',
            tasks: [{ task: 't', title: 'Task', role: 'backend', review: 'code', summary: '' }],
          },
        }),
      ),
    ).toBe('Plan proposed: 1 task');
    expect(
      eventText(
        event({
          kind: 'run.model',
          payload: { run: 'r', requested: 'wanted', served: { mismatch: 'other' } },
        }),
      ),
    ).toBe('Model mismatch: asked for wanted, served other');
    expect(
      eventText(
        event({
          kind: 'question.answered',
          payload: { question: 'q', answer: { reply: '{model} stays literal' } },
        }),
      ),
    ).toBe('Answered: {model} stays literal');
    expect(
      eventText(
        event({
          kind: 'branch.updated',
          payload: {
            task: 't',
            branch: 'branch',
            base: 'base',
            from: 'old',
            head: 'head',
            same_change: true,
          },
        }),
      ),
    ).toBe('Brought up to date; same change, review kept');
    expect(word('role.changed.restored', { name: 'Backend' })).toBe('Role Backend restored');
    expect(word('goal_current.stated')).toBe('Waiting for a plan');
  });
});

describe('names in event wording', () => {
  const role = '01M56JK2XD7AC16EQBXTA2R3JS',
    task = '01M56JKWF3TR7E3H1CEYNMDK8J';
  const names: EventNames = {
    roles: { p: [{ id: role, name: 'Orchestrator' } as Role] },
    tasks: [{ id: task, title: 'Add the catalogue' } as Task],
    questions: [
      {
        id: 'q',
        options: [
          { id: 1, text: 'Approve', effect: '' },
          { id: 2, text: 'Not yet', effect: '' },
        ],
      } as Question,
    ],
  };
  const inProject = (kind: EventKind): Event => ({ ...event(kind), project: 'p' });
  const started = inProject({
    kind: 'run.started',
    payload: {
      run: 'r',
      task,
      role,
      hire: 'h',
      model: 'm',
      vendor: 'fake',
      effort: 'medium',
      reason: '',
    },
  });
  const hired = inProject({
    kind: 'hire.added',
    payload: { id: 'h', role, model: 'fake-1', effort: 'low', status: 'active' },
  });
  const answered = inProject({
    kind: 'question.answered',
    payload: { question: 'q', answer: { choice: 2 } },
  });
  it('shows role names, task titles and option text', () => {
    expect(eventText(started, names)).toBe('Orchestrator started Add the catalogue');
    expect(eventText(hired, names)).toBe('Orchestrator: fake-1 hired');
    expect(
      eventText(
        inProject({ kind: 'task.started', payload: { task, run: 'r', resume: false } }),
        names,
      ),
    ).toBe('Started Add the catalogue');
    expect(eventText(inProject({ kind: 'task.done', payload: { task } }), names)).toBe(
      'Task done: Add the catalogue',
    );
    expect(eventText(answered, names)).toBe('Answered: Not yet');
    expect(
      eventText({ ...started, payload: { ...started.payload, task: null } } as Event, names),
    ).toBe('Orchestrator started work');
  });
  it('falls back to the id or number when a name is missing', () => {
    const none: EventNames = { roles: {}, tasks: [], questions: [] };
    expect(eventText(started, none)).toBe(`${role} started ${task}`);
    expect(eventText(started)).toBe(`${role} started ${task}`);
    expect(eventText({ ...hired, project: 'other' }, names)).toBe(`${role}: fake-1 hired`);
    expect(eventText(answered, none)).toBe('Answered: 2');
    expect(
      eventText(
        inProject({ kind: 'question.answered', payload: { question: 'q', answer: { choice: 9 } } }),
        names,
      ),
    ).toBe('Answered: 9');
  });
});
