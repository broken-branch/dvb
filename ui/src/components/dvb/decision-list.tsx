import { Card } from './card';
import type { Event, Role } from '@/lib/data/types';

export function DecisionList({ events, roles }: { events: Event[]; roles: Role[] }) {
  const decisions = events.filter((e) => e.kind === 'decision.recorded').toReversed();
  return (
    <div role="region" aria-label="Recorded decisions">
      {decisions.length ? (
        decisions.map(
          (e) =>
            e.kind === 'decision.recorded' && (
              <Card key={e.id}>
                <h2>{e.payload.title}</h2>
                <p>{roles.find((r) => r.id === e.payload.by)?.name || e.payload.by}</p>
                {e.payload.options?.length ? (
                  <ul>
                    {e.payload.options.map((option, i) => (
                      <li key={i}>{option}</li>
                    ))}
                  </ul>
                ) : null}
                <h3>{e.payload.choice}</h3>
                <p>{e.payload.reason}</p>
                {e.payload.swot && <p className="whitespace-pre-wrap">{e.payload.swot}</p>}
              </Card>
            ),
        )
      ) : (
        <p>
          The team has recorded no decisions yet. Questions you answered are in the project's
          Conversation.
        </p>
      )}
    </div>
  );
}
