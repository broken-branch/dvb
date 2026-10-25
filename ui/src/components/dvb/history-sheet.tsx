import { useState, type ReactNode } from 'react';
import { History } from 'lucide-react';
import { Button } from '@/components/ui/button';
import {
  Sheet,
  SheetContent,
  SheetDescription,
  SheetHeader,
  SheetTitle,
  SheetTrigger,
} from '@/components/ui/sheet';
import { Card } from './card';
import { DataError, useRead } from './data-feedback';
import { Status } from './common';
import { refusalText } from '@/lib/data/display';
import { useStore } from '@/lib/store';
import type { ApiResult, Event, SettingsKey } from '@/lib/data/types';

type HistoryTarget =
  | { kind: 'role'; id: string; name: string }
  | { kind: 'hire'; id: string; name: string }
  | { kind: 'setting'; key: SettingsKey; name: string };
type Version = { event: number; at: number; actor: Event['actor']; value: unknown };

const show = (value: unknown): string =>
  value === null || value === undefined || value === ''
    ? 'none'
    : typeof value === 'boolean'
      ? value
        ? 'on'
        : 'off'
      : Array.isArray(value)
        ? value.map(show).join(', ') || 'none'
        : typeof value === 'object'
          ? JSON.stringify(value)
          : String(value);
const who = (actor: Event['actor']) =>
  actor === 'owner' ? 'You' : actor === 'engine' ? 'dvb' : 'An agent';
/** What a version set: its fields that differ from the next older one (all of them for the oldest), or the plain value. */
function describe(value: unknown, older?: unknown) {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return show(value);
  const before = (older && typeof older === 'object' ? older : {}) as Record<string, unknown>;
  const changed = Object.entries(value).filter(
    ([key, v]) => older === undefined || show(before[key]) !== show(v),
  );
  return (changed.length ? changed : Object.entries(value))
    .map(([key, v]) => `${key.replaceAll('_', ' ')}: ${show(v)}`)
    .join(', ');
}

/** The versions of one role, hire or setting, newest first, each with Restore. Its trigger gets focus back on close. */
function HistorySheet({
  target,
  setOpen,
  children,
}: {
  target: HistoryTarget | null;
  setOpen: (open: boolean) => void;
  children: ReactNode;
}) {
  const { data: s, canWrite } = useStore(),
    [reload, setReload] = useState(0),
    [notice, setNotice] = useState(''),
    [error, setError] = useState(''),
    [busy, setBusy] = useState(false);
  const history = useRead<Version[]>(async (): Promise<ApiResult<Version[]>> => {
    if (!target) return { ok: true, value: [] };
    const result =
      target.kind === 'role'
        ? await s.roleHistory(target.id)
        : target.kind === 'hire'
          ? await s.hireHistory(target.id)
          : await s.settingsHistory(target.key);
    return result.ok ? { ok: true, value: result.value.versions } : result;
  }, [target?.kind, target && ('id' in target ? target.id : target.key), reload]);
  const versions = history.value || [],
    current = versions[0]?.event;
  async function restore(event: number) {
    if (!target || current === undefined || busy) return;
    setBusy(true);
    setNotice('');
    setError('');
    try {
      const body = { event, current };
      const result =
        target.kind === 'role'
          ? await s.restoreRole(target.id, body)
          : target.kind === 'hire'
            ? await s.restoreHire(target.id, body)
            : await s.restoreSettings({ ...body, key: target.key });
      if (result.ok) setNotice('Restored.');
      else if (result.refusal.code === 'VALUE_CHANGED')
        setNotice(
          'This changed while you were looking. The history below is up to date; choose again.',
        );
      else setError(refusalText(result.refusal));
      if (result.ok || result.refusal.code === 'VALUE_CHANGED') setReload((n) => n + 1);
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Unable to connect. Try again.');
    } finally {
      setBusy(false);
    }
  }
  return (
    <Sheet
      open={target !== null}
      onOpenChange={(open) => {
        if (!open) {
          setNotice('');
          setError('');
        }
        setOpen(open);
      }}
    >
      <SheetTrigger asChild>{children}</SheetTrigger>
      <SheetContent>
        <SheetHeader>
          <SheetTitle>History: {target?.name}</SheetTitle>
          <SheetDescription>
            Earlier values, newest first. Restoring one makes it the current value again.
          </SheetDescription>
        </SheetHeader>
        {notice && (
          <div className="notice" role="status">
            {notice}
          </div>
        )}
        <DataError message={error || history.error} />
        <div className="activity-list" role="region" aria-label="History">
          {versions.map((v, i) => (
            <Card key={v.event} density="compact">
              <time>{new Date(v.at).toLocaleString()}</time>
              <p>{describe(v.value, versions[i + 1]?.value)}</p>
              <small>{who(v.actor)}</small>
              {i === 0 ? (
                <Status>Current</Status>
              ) : (
                <Button
                  variant="outline"
                  disabled={busy || !canWrite}
                  aria-label={`Restore the value from ${new Date(v.at).toLocaleString()}`}
                  onClick={() => void restore(v.event)}
                >
                  Restore
                </Button>
              )}
            </Card>
          ))}
          {history.value && !versions.length && <p>No earlier values.</p>}
        </div>
      </SheetContent>
    </Sheet>
  );
}

/** A small History button that opens the sheet for one target. */
export function HistoryButton({ target }: { target: HistoryTarget }) {
  const [open, setOpen] = useState(false);
  return (
    <HistorySheet target={open ? target : null} setOpen={setOpen}>
      <Button type="button" variant="ghost" size="sm" aria-label={`History of ${target.name}`}>
        <History size={14} />
        History
      </Button>
    </HistorySheet>
  );
}
