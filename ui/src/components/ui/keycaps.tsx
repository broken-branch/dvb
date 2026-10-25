import { Fragment } from 'react';

export function Keycaps({ keys, sequence = false }: { keys: string[][]; sequence?: boolean }) {
  return (
    <span className="keycaps">
      {keys.map((combination, i) => (
        <Fragment key={i}>
          {i > 0 && <span className="keycap-join">{sequence ? ' + ' : ' or '}</span>}
          <span className="keycap-combination">
            {combination.map((key, j) => (
              <Fragment key={j}>
                {j > 0 && <span className="sr-only"> + </span>}
                <kbd data-slot="keycap">{key}</kbd>
              </Fragment>
            ))}
          </span>
        </Fragment>
      ))}
    </span>
  );
}
