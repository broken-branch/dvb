import { routes } from '@/lib/routes';
import { useId, useState } from 'react';
import { Card, ListRow } from '@/components/dvb/card';
import { Heading, Status, Meter, ChoiceSelect, InfoTip } from '@/components/dvb/common';
import { DataError, useRead } from '@/components/dvb/data-feedback';
import { ProjectLabel } from '@/components/dvb/project-identity';
import { useStore } from '@/lib/store';
const equivalentInfo =
  "What these tokens would cost at the vendor's published API prices. For comparison only; your subscriptions are not charged this.";
const averageInfo =
  "What one million of these tokens costs on average at the vendor's published API prices: Est. equivalent divided by tokens, times 1,000,000. For comparison only.";

function UsageCost({ label, info, value }: { label: string; info: string; value: string }) {
  const labelId = useId();
  return (
    <div className="usage-cost" role="group" aria-labelledby={labelId}>
      <span className="usage-cost-label">
        <span id={labelId}>{label}</span>
        <InfoTip label={`About ${label}`} text={info} />
      </span>
      <span>{value}</span>
    </div>
  );
}

export function Usage() {
  const { data: s } = useStore(),
    [project, setProject] = useState('all'),
    [since, setSince] = useState('all'),
    [today] = useState(() => String(new Date().setHours(0, 0, 0, 0)));
  const roles = Object.values(s.roles).flat();
  const usage = useRead(
    () =>
      s.usage({
        project: project === 'all' ? undefined : project,
        since: since === 'all' ? undefined : Number(since),
      }),
    [s.usage, project, since, s.usageVersion],
  );
  return (
    <>
      <Heading title="Usage & models" description="Usage reported by your subscriptions.">
        <ChoiceSelect
          label="Usage project"
          value={project}
          onChange={setProject}
          options={[
            { value: 'all', label: 'All projects' },
            ...s.projects.map((p) => ({ value: p.id, label: p.name })),
          ]}
        />
        <ChoiceSelect
          label="Usage period"
          value={since}
          onChange={setSince}
          options={[
            { value: 'all', label: 'All time' },
            { value: today, label: 'Today' },
          ]}
        />
      </Heading>
      <DataError message={usage.error} />
      {usage.value && (
        <>
          <div className="usage-windows">
            {usage.value.windows.map((w, i) => (
              <Card key={`${w.vendor}-${w.window}-${i}`} className="usage-window">
                <div className="section-heading">
                  <h2>{w.vendor}</h2>
                  <Status tone={w.limited ? 'warn' : 'good'}>
                    {w.limited ? 'Usage paused' : 'Available'}
                  </Status>
                </div>
                <div className="usage-number">
                  {w.used_pct}
                  <span>%</span>
                  <small>{w.window} window used</small>
                </div>
                <Meter value={w.used_pct} label={`${w.vendor} ${w.window}`} />
                {w.resets_at && <p>Resets {new Date(w.resets_at).toLocaleString()}</p>}
              </Card>
            ))}
          </div>
          <div className="role-table usage-role-table">
            <div className="role-table-head">
              <span>Project</span>
              <span>Role</span>
              <span>Model</span>
              <span>Tokens</span>
              <span>
                Est. equivalent <InfoTip label="About Est. equivalent" text={equivalentInfo} />
              </span>
              <span>
                Avg. per 1M tokens <InfoTip label="About Avg. per 1M tokens" text={averageInfo} />
              </span>
            </div>
            {usage.value.rows.map((row, i) => (
              <ListRow asChild key={i}>
                <div className="role-table-row">
                  <ProjectLabel id={row.project} />
                  <a href={`#${routes.company(row.role, row.project)}`}>
                    {roles.find((r) => r.id === row.role)?.name || row.role}
                  </a>
                  <span>{row.model}</span>
                  <span>
                    {(row.uncached_in + row.cached_in + row.out).toLocaleString()}
                    <small>{row.cached_in.toLocaleString()} cached</small>
                  </span>
                  <UsageCost
                    label="Est. equivalent"
                    info={equivalentInfo}
                    value={`$${row.est_cost.toFixed(2)}`}
                  />
                  <UsageCost
                    label="Avg. per 1M tokens"
                    info={averageInfo}
                    value={averageTokenCost(
                      row.est_cost,
                      row.uncached_in + row.cached_in + row.out,
                    )}
                  />
                </div>
              </ListRow>
            ))}
          </div>
          {!usage.value.rows.length && (
            <Card className="empty-state">No usage reported for this period.</Card>
          )}
          <p className="muted">
            Estimates use token prices for comparison. They are not subscription charges.
          </p>
        </>
      )}
    </>
  );
}

export function averageTokenCost(cost: number, tokens: number) {
  return tokens > 0 ? `$${((cost / tokens) * 1_000_000).toFixed(2)}` : '—';
}
