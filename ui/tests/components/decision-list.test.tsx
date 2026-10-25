import { it, expect } from 'vitest';
import { screen } from '@testing-library/react';
import { mount } from '../support/render';
import { DecisionList } from '@/components/dvb/decision-list';
import { createFixtureSource } from '../support/render';
import type { Event } from '@/lib/data/types';
function decision(id: number, run: string, title: string, project = 'harbour'): Event {
  return {
    id,
    at: 1,
    actor: { run },
    project,
    kind: 'decision.recorded',
    payload: {
      by: 'harbour-backend',
      title,
      options: ['SQLite', 'Files'],
      choice: 'SQLite',
      reason: 'Atomic writes',
      swot: 'Strength: transactions\nWeakness: one writer',
    },
  };
}
it('shows the choice, options, reason, SWOT and author, with an empty state', () => {
  const source = createFixtureSource();
  const view = mount(
    <DecisionList
      events={[decision(1, 'r', 'Choose storage')]}
      roles={source.getSnapshot().roles.harbour}
    />,
    source,
  );
  expect(screen.getByText('Backend builder')).toBeInTheDocument();
  expect(screen.getByRole('heading', { name: 'SQLite' })).toBeInTheDocument();
  expect(screen.getByRole('list')).toHaveTextContent('SQLiteFiles');
  expect(screen.getByText('Atomic writes')).toBeInTheDocument();
  expect(screen.getByText(/Strength: transactions/)).toHaveTextContent('Weakness: one writer');
  view.rerender(<DecisionList events={[decision(1, 'r', 'Choose storage')]} roles={[]} />);
  expect(screen.getByText('harbour-backend')).toBeInTheDocument();
  view.rerender(<DecisionList events={[]} roles={[]} />);
  expect(screen.getByText(/The team has recorded no decisions yet/)).toHaveTextContent(
    "Questions you answered are in the project's Conversation.",
  );
});
