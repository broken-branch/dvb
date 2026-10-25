import type { Question } from '@/lib/data/types';
import { mount } from '../support/render';
import { it, expect, vi, describe, afterEach } from 'vitest';
import { fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import App from '@/App';
import { createFixtureSource } from '../support/render';
import { Decision } from '@/screens/decision';
describe('answers', () => {
  it('Decision sends option IDs and displays the recorded answer', async () => {
    const { source, user } = mount(<Decision id="local" />),
      answer = vi.spyOn(source.getSnapshot(), 'answer');
    await user.click(screen.getByRole('button', { name: 'Send answer' }));
    expect(answer).toHaveBeenCalledWith('local', { choice: 1 });
    expect(await screen.findByText('Your direction is with the team.')).toBeInTheDocument();
    expect(source.getSnapshot().questions[0].open).toBe(true); // The receipt renders before the next engine snapshot arrives.
  });
  it('Decision sends a written reply and keeps the draft when the question lapses', async () => {
    const source = createFixtureSource();
    source.set({
      answer: async () => ({ ok: false, refusal: { code: 'QUESTION_LAPSED', detail: 'Changed' } }),
    });
    const { user } = mount(<Decision id="local" />, source);
    await user.click(screen.getByRole('radio', { name: 'Write a reply' }));
    await user.type(screen.getByRole('textbox'), 'Start locally');
    await user.click(screen.getByRole('button', { name: 'Send answer' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('This question has changed');
    expect(screen.getByRole('textbox')).toHaveValue('Start locally');
    expect(screen.queryByText('Your direction is with the team.')).not.toBeInTheDocument();
  });
  it('names no owner for an answer this session did not send', () => {
    const source = createFixtureSource();
    const [q] = source.getSnapshot().questions;
    // dvb answered it; the answered event is outside the recent events.
    source.set({ questions: [{ ...q, open: false, answer: { choice: 1 } }], events: [] });
    mount(<Decision id="local" />, source);
    expect(screen.getByText('This question is answered.')).toBeInTheDocument();
    expect(screen.queryByText('Your direction is with the team.')).not.toBeInTheDocument();
  });
});
describe('connection', () => {
  it('disables Decision writes when versions differ', () => {
    const source = createFixtureSource();
    source.set({ connection: { state: 'version-mismatch', engineVersion: '2', appVersion: '1' } });
    mount(<Decision id="local" />, source);
    expect(screen.getByRole('button', { name: 'Send answer' })).toBeDisabled();
  });
});
describe('formats', () => {
  function withFormat(format: Question['format'], effects?: string[]) {
    const source = createFixtureSource(),
      q = source.getSnapshot().questions.find((q) => q.id === 'local')!;
    source.set({
      questions: [
        {
          ...q,
          format,
          recommended: 2,
          options: q.options.map((o, i) => ({ ...o, effect: effects?.[i] ?? o.effect })),
        },
      ],
    });
    return source;
  }
  describe('question formats', () => {
    it('simple lists the options with the recommendation pre-selected', () => {
      mount(<Decision id="local" />, withFormat('simple'));
      expect(screen.getByRole('radio', { name: /Include a second catalogue/ })).toBeChecked();
      expect(screen.getByText('Recommended')).toBeInTheDocument();
    });
    it('quick compare puts the options side by side with their effect', () => {
      const { container } = mount(<Decision id="local" />, withFormat('quick_compare'));
      expect(container.querySelector('.decision-choices.compare')).not.toBeNull();
      expect(screen.getByText('Import the initial catalogue sooner.')).toBeInTheDocument();
      expect(screen.getByText('Include both import formats.')).toBeInTheDocument();
    });
    it('swot shows each option as four labelled parts', () => {
      const source = withFormat('swot', [
        'Strengths: fast\nWeaknesses: one machine\nOpportunities: reuse\nThreats: rework',
        'Strength: flexible\nThreat: delay',
      ]);
      mount(<Decision id="local" />, source);
      expect(screen.getAllByText('Strengths')).toHaveLength(2);
      expect(screen.getByText('one machine')).toBeInTheDocument();
      expect(screen.getAllByText('Threats')).toHaveLength(2);
      expect(screen.getAllByText('Weaknesses')).toHaveLength(1);
    });
    it('swot falls back to the plain effect when it has no labelled parts', () => {
      mount(<Decision id="local" />, withFormat('swot'));
      expect(screen.getByText('Import the initial catalogue sooner.')).toBeInTheDocument();
    });
  });
});
describe('refusals', () => {
  afterEach(() => {
    history.replaceState({}, '', '/');
  });
  it('keeps question-specific VALUE_CHANGED wording and the answer draft in Decision', async () => {
    const source = createFixtureSource();
    source.set({
      answer: async () => ({ ok: false, refusal: { code: 'VALUE_CHANGED', detail: 'Changed' } }),
    });
    const { user } = mount(<Decision id="local" />, source);
    await user.click(screen.getByRole('radio', { name: 'Write a reply' }));
    await user.type(screen.getByRole('textbox'), 'Keep this draft');
    await user.click(screen.getByRole('button', { name: 'Send answer' }));
    expect(await screen.findByRole('alert')).toHaveTextContent(
      'This question has changed. Open the current question before answering.',
    );
    expect(screen.getByRole('textbox')).toHaveValue('Keep this draft');
  });
});

it('renders a closed unanswered question without answer controls', () => {
  const source = createFixtureSource();
  source.set({ questions: [{ ...source.getSnapshot().questions[0], open: false }] });
  mount(<Decision id="local" />, source);
  expect(screen.getByText('Closed')).toBeInTheDocument();
  expect(screen.queryByRole('button', { name: 'Send answer' })).not.toBeInTheDocument();
});
describe('plan tasks', () => {
  it('lists each planned task with its role, routine mark and the plan task it starts after', () => {
    const source = createFixtureSource(),
      q = source.getSnapshot().questions.find((q) => q.id === 'local')!;
    source.set({
      questions: [
        {
          ...q,
          plan: [
            {
              task: 'api',
              title: 'Add the import API',
              role: 'harbour-backend',
              review: 'code',
              summary: '',
            },
            {
              task: 'screen',
              title: 'Show imports',
              role: 'harbour-frontend',
              review: 'design',
              summary: '',
              after: ['api'],
            },
            {
              task: 'docs',
              title: 'Describe imports',
              role: 'harbour-clerk',
              review: 'code',
              summary: '',
              after: ['resume'],
              routine: true,
            },
          ],
        },
      ],
    });
    mount(<Decision id="local" />, source);
    const items = within(screen.getByRole('list', { name: 'Planned tasks' })).getAllByRole(
      'listitem',
    );
    expect(items.map((i) => i.textContent)).toEqual([
      'Add the import APIBackend builder',
      'Show importsFrontend builderStarts after 1 merges',
      'Describe importsClerkRoutineStarts after Keep removed records removed merges',
    ]);
  });
  it('shows no plan list for a question without a plan', () => {
    mount(<Decision id="local" />);
    expect(screen.queryByRole('list', { name: 'Planned tasks' })).not.toBeInTheDocument();
  });
});
describe('question text', () => {
  it('keeps a short first sentence as the heading and every plan step on its own line', () => {
    const source = createFixtureSource(),
      subject =
        'Start this plan? Plan the spring planting.\n\nPlan:\n1. backend — Sow beds\n2. frontend — Show beds\n3. backend — Water beds';
    source.set({
      questions: source
        .getSnapshot()
        .questions.map((q) => (q.id === 'local' ? { ...q, subject } : q)),
    });
    const { container } = mount(<Decision id="local" />, source);
    expect(screen.getByRole('heading', { level: 1 })).toHaveTextContent(/^Start this plan\?$/);
    const body = container.querySelector('.question-body')!;
    expect(body.textContent).toBe(
      'Plan the spring planting.\n\nPlan:\n1. backend — Sow beds\n2. frontend — Show beds\n3. backend — Water beds',
    );
  });
  it('shows a one-line question as its heading only', () => {
    const { container } = mount(<Decision id="local" />);
    expect(screen.getByRole('heading', { level: 1 })).toHaveTextContent(
      'Import one catalogue first?',
    );
    expect(container.querySelector('.question-body')).toBeNull();
  });
});
describe('back link', () => {
  afterEach(() => history.replaceState({}, '', '/'));
  it("returns to the project a question was opened from, named by the project's name", async () => {
    history.replaceState({}, '', '/#project/harbour');
    const source = createFixtureSource();
    render(<App source={source} />);
    fireEvent.click(await screen.findByRole('button', { name: 'Import one catalogue first?' }));
    await waitFor(() => expect(location.hash).toBe('#decision/local/project/harbour'));
    expect(screen.queryByRole('button', { name: 'Home' })).not.toBeInTheDocument();
    fireEvent.click(await screen.findByRole('button', { name: 'Harbour' }));
    await waitFor(() => expect(location.hash).toBe('#project/harbour'));
  });
  it('goes Home from a direct link', async () => {
    history.replaceState({}, '', '/#decision/local');
    render(<App source={createFixtureSource()} />);
    fireEvent.click(await screen.findByRole('button', { name: 'Home' }));
    await waitFor(() => expect(location.hash).toBe('#home'));
  });
});
