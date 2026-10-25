import { Switch } from '@/components/ui/switch';
import { useRead } from '@/components/dvb/data-feedback';
import { useStore } from '@/lib/store';

/** Open issues to attach to a new goal. Loading or a failed read never blocks the goal. */
export function GoalIssues({
  project,
  picked,
  onChange,
}: {
  project: string;
  picked: number[];
  onChange: (picked: number[]) => void;
}) {
  const { data: s } = useStore();
  const { value, error } = useRead(() => s.projectIssues(project), [project]);
  return (
    <fieldset className="field goal-issues">
      <legend>Attach issues (optional)</legend>
      {error ? (
        <p>Open issues could not be loaded. You can still state the goal.</p>
      ) : !value ? (
        <p role="status">Loading open issues…</p>
      ) : !value.length ? (
        <p>No open issues.</p>
      ) : (
        <ul>
          {value.map((issue) => (
            <li key={issue.number}>
              {issue.trusted ? (
                <label className="switch-row">
                  <span>
                    #{issue.number} {issue.title}
                    {!!issue.labels.length && <small>{issue.labels.join(', ')}</small>}
                  </span>
                  <Switch
                    size="sm"
                    checked={picked.includes(issue.number)}
                    onCheckedChange={(on) =>
                      onChange(
                        on ? [...picked, issue.number] : picked.filter((n) => n !== issue.number),
                      )
                    }
                  />
                </label>
              ) : (
                <span className="switch-row">
                  <span>
                    <a href={issue.url} target="_blank" rel="noreferrer">
                      {issue.title}
                    </a>
                    <small>Can't attach: it was not opened by a member.</small>
                  </span>
                </span>
              )}
            </li>
          ))}
        </ul>
      )}
    </fieldset>
  );
}
