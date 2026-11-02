import { routes } from '@/lib/routes';
import { useEffect, useId, useRef, useState } from 'react';
import { Card, ListRow } from '@/components/dvb/card';
import { Heading, Status, Meter, ChoiceSelect, InfoTip } from '@/components/dvb/common';
import { DataError, useRead } from '@/components/dvb/data-feedback';
import { ProjectLabel } from '@/components/dvb/project-identity';
import { useStore } from '@/lib/store';
import { Button } from '@/components/ui/button';
import { refusalText } from '@/lib/data/display';
import type { Vendor } from '@/lib/data/types';
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

/**
 * A check-again on a paused vendor: running, or its result sentence and whether
 * the sentence is about a paused window (still limited, or refused) or not.
 */
type Check = { busy: true } | { busy: false; text: string; refused: boolean; limited: boolean };

export function Usage() {
  const { data: s, canWrite } = useStore(),
    // Kept here so a result outlives the reread its vendor.resumed or vendor.limited causes.
    // A result shows only while it agrees with its window, and a reread drops one that no longer does.
    [checks, setChecks] = useState<Partial<Record<Vendor, Check>>>({}),
    [project, setProject] = useState('all'),
    [since, setSince] = useState('all'),
    [today] = useState(() => String(new Date().setHours(0, 0, 0, 0)));
  const connection = `${s.connection.state}/${'engineVersion' in s.connection ? s.connection.engineVersion : ''}`;
  const generation = useRef(0);
  useEffect(() => {
    setChecks({});
    return () => {
      generation.current++;
    };
  }, [connection]);
  const roles = Object.values(s.roles).flat();
  const usage = useRead(
    () =>
      s.usage({
        project: project === 'all' ? undefined : project,
        since: since === 'all' ? undefined : Number(since),
      }),
    [s.usage, project, since, s.usageVersion, connection],
  );
  const windows = usage.value?.windows,
    [readWindows, setReadWindows] = useState(windows);
  if (windows !== readWindows) {
    setReadWindows(windows);
    if (windows)
      setChecks((c) => {
        const kept = Object.entries(c).filter(
          ([vendor, check]) =>
            check.busy || windows.some((w) => w.vendor === vendor && w.limited === check.limited),
        );
        return kept.length === Object.keys(c).length ? c : Object.fromEntries(kept);
      });
  }
  async function checkAgain(vendor: Vendor) {
    const started = generation.current;
    setChecks((c) => ({ ...c, [vendor]: { busy: true } }));
    let done: Check;
    try {
      const result = await s.checkUsage(vendor);
      done = result.ok
        ? {
            busy: false,
            refused: false,
            limited: result.value.limited,
            text: !result.value.limited
              ? 'Running again: the usage limit has lifted.'
              : result.value.resets_at
                ? `Still limited until ${new Date(result.value.resets_at).toLocaleString()}.`
                : 'Still limited.',
          }
        : { busy: false, refused: true, limited: true, text: refusalText(result.refusal) };
    } catch (e) {
      done = {
        busy: false,
        refused: true,
        limited: true,
        text: e instanceof Error ? e.message : 'Unable to connect. Try again.',
      };
    }
    if (started === generation.current) setChecks((c) => ({ ...c, [vendor]: done }));
  }
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
                {w.limited && (
                  <Button
                    type="button"
                    variant="outline"
                    disabled={checks[w.vendor]?.busy || !canWrite}
                    onClick={() => void checkAgain(w.vendor)}
                  >
                    {checks[w.vendor]?.busy ? 'Checking…' : 'Check again'}
                  </Button>
                )}
                <CheckResult check={checks[w.vendor]} limited={w.limited} />
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

function CheckResult({ check, limited }: { check?: Check; limited: boolean }) {
  if (!check || check.busy || check.limited !== limited) return null;
  return check.refused ? (
    <p role="alert" className="warning-text">
      {check.text}
    </p>
  ) : (
    <p role="status">{check.text}</p>
  );
}

export function averageTokenCost(cost: number, tokens: number) {
  return tokens > 0 ? `$${((cost / tokens) * 1_000_000).toFixed(2)}` : '—';
}
