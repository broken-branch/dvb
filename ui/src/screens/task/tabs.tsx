import { Card } from '@/components/dvb/card';
import { Status } from '@/components/dvb/common';
import { DataError, PagedFeed } from '@/components/dvb/data-feedback';
import { DecisionList } from '@/components/dvb/decision-list';
import { useEventPages } from '@/lib/data/pages';
import type { useTask } from './use-task';
type TaskData = ReturnType<typeof useTask>;
export function TaskChanges({
  diff,
  reverse,
}: {
  diff: TaskData['diff'];
  reverse: TaskData['reverse'];
}) {
  return (
    <>
      <DataError message={diff.error} />
      {diff.value && (
        <div className={`diff-layout ${reverse ? 'reversed' : ''}`}>
          <aside className="file-list">
            <h3>Changed files</h3>
            {diff.value.diff
              .split('\n')
              .filter((line) => line.startsWith('+++ '))
              .map((line, i) => (
                <Card key={i} density="compact">
                  {line.slice(4).replace(/^b\//, '')}
                </Card>
              ))}
          </aside>
          <Card className="diff-panel">
            <div className="diff-heading">Changes on {diff.value.head}</div>
            <div className="diff-code" aria-label="Changes">
              {diff.value.diff.split('\n').map((line, i) => (
                <div
                  key={i}
                  className={
                    line.startsWith('+') ? 'diff-add' : line.startsWith('-') ? 'diff-remove' : ''
                  }
                >
                  <code>{line || ' '}</code>
                </div>
              ))}
            </div>
            <div className="diff-caption">
              {diff.value.base} → {diff.value.head}
            </div>
          </Card>
        </div>
      )}
    </>
  );
}

export function TaskReview({
  verdict,
  head,
}: {
  verdict: TaskData['verdict'];
  head: string | null | undefined;
}) {
  return (
    <Card className="review-content">
      <h2>Independent review</h2>
      <DataError message={verdict.error} />
      {verdict.value && (
        <>
          <Status
            tone={
              verdict.value.head !== head ? 'warn' : verdict.value.open_findings ? 'warn' : 'good'
            }
          >
            {verdict.value.head !== head
              ? 'Review is on an earlier change'
              : verdict.value.open_findings
                ? `${verdict.value.open_findings} open findings`
                : 'No open findings'}
          </Status>
          <p>
            {verdict.value.model} / {verdict.value.vendor}
          </p>
          <p>
            <span className="task-written-text">{verdict.value.summary}</span>
          </p>
          <small>Reviewed change: {verdict.value.head}</small>
          {verdict.value.findings.map((f, i) => (
            <Card key={i} className="review-finding">
              <Status tone={f.open ? 'warn' : 'good'}>
                {f.open ? 'Open' : 'Resolved'} / {f.severity}
              </Status>
              <h3>{f.title}</h3>
              <p>{f.detail}</p>
              <small>
                {f.file}
                {f.line ? `:${f.line}` : ''}
              </small>
            </Card>
          ))}
        </>
      )}
    </Card>
  );
}

export function TaskChecks({ check }: { check: TaskData['check'] }) {
  return (
    <Card className="review-content">
      <h2>Checks on the latest change</h2>
      {check?.kind === 'checks.observed' ? (
        <>
          <Status tone={check.payload.status === 'success' ? 'good' : 'warn'}>
            {check.payload.status}
          </Status>
          <p>{check.payload.names.join(', ')}</p>
          <small>Checked change: {check.payload.head}</small>
        </>
      ) : (
        <p>No checks reported on this change.</p>
      )}
    </Card>
  );
}

export function TaskDecisions({
  events,
  roles,
}: {
  events: TaskData['events'];
  roles: TaskData['s']['roles'][string];
}) {
  return <DecisionList events={events} roles={roles} />;
}
export function TaskFeed({ task, project }: { task: string; project: string }) {
  const page = useEventPages({ task, project });
  return <PagedFeed page={page} events={page.events} />;
}
