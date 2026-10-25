import { useEffect, useLayoutEffect, useRef, useState } from 'react';
import type { ApiResult, Event, DataStore, Project, Task } from '@/lib/data/types';
import { eventText, refusalText } from '@/lib/data/display';
import { Button } from '@/components/ui/button';
import { Card } from './card';
import { useStore } from '@/lib/store';
export function toggleWork(s: DataStore, work: Project | Task): Promise<ApiResult<Task | void>> {
  if ('paused' in work) return work.paused ? s.resumeProject(work.id) : s.pauseProject(work.id);
  return work.state.state === 'stopped' ? s.resumeTask(work.id) : s.stopTask(work.id);
}
export function useAction() {
  const pending = useRef(false);
  const [error, setError] = useState(''),
    [busy, setBusy] = useState(false);
  async function act<T>(operation: () => Promise<ApiResult<T>>, done?: (value: T) => void) {
    if (pending.current) return;
    pending.current = true;
    setBusy(true);
    setError('');
    try {
      const result = await operation();
      if (result.ok) {
        done?.(result.value);
        return true;
      }
      setError(refusalText(result.refusal));
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Unable to connect. Try again.');
    } finally {
      pending.current = false;
      setBusy(false);
    }
    return false;
  }
  return { error, busy, act };
}
export function DataError({ message }: { message: string }) {
  return message ? (
    <div className="notice" role="alert">
      {message}
    </div>
  ) : null;
}
export function useRead<T>(load: () => Promise<ApiResult<T>>, dependencies: unknown[]) {
  const [value, setValue] = useState<T>(),
    [error, setError] = useState('');
  useEffect(() => {
    let alive = true;
    setValue(undefined);
    setError('');
    load()
      .then((result) => {
        if (!alive) return;
        if (result.ok) setValue(result.value);
        else setError(refusalText(result.refusal));
      })
      .catch((e) => {
        if (alive) setError(e instanceof Error ? e.message : 'Unable to load this item.');
      });
    return () => {
      alive = false;
    };
  }, dependencies);
  return { value, error };
}
export function Feed({ events }: { events: Event[] }) {
  const { data } = useStore();
  const lines = events.flatMap((e) => {
    const text = eventText(e, data);
    return text ? [{ e, text }] : [];
  });
  return (
    <div className="activity-list" role="region" aria-label="Activity feed" tabIndex={-1}>
      {lines.length ? (
        lines.map(({ e, text }) => (
          <Card key={e.id} density="compact">
            <time>
              {new Date(e.at).toDateString() === new Date().toDateString()
                ? new Date(e.at).toLocaleTimeString()
                : new Date(e.at).toLocaleString()}
            </time>
            <p
              className={
                ['goal.stated', 'question.opened'].includes(e.kind) ? 'written-subject' : undefined
              }
            >
              {text}
            </p>
          </Card>
        ))
      ) : (
        <p>No activity yet.</p>
      )}
    </div>
  );
}

type OlderPage = { more: boolean; busy: boolean; error: string; older(): Promise<void> };
export function OlderEvents({ page }: { page: OlderPage }) {
  return (
    <>
      <DataError message={page.error} />
      {page.more && (
        <Button variant="outline" disabled={page.busy} onClick={() => void page.older()}>
          {page.busy ? 'Loading…' : 'Show older'}
        </Button>
      )}
    </>
  );
}

/** An oldest-first feed whose older events load above it. The row the owner was
 * reading keeps its place on screen, measured against the viewport so a
 * browser's own scroll anchoring is not added twice. */
export function PagedFeed({ page, events }: { page: OlderPage; events: Event[] }) {
  const list = useRef<HTMLDivElement>(null),
    button = useRef<HTMLButtonElement>(null),
    kept = useRef<{ row: Element; top: number } | null>(null);
  function older() {
    const row = list.current?.querySelector('.activity-list > *');
    kept.current = row ? { row, top: row.getBoundingClientRect().top } : null;
    void page.older();
  }
  useLayoutEffect(() => {
    const k = kept.current;
    if (!k) return;
    if (k.row.isConnected) {
      const moved = k.row.getBoundingClientRect().top - k.top;
      if (moved) window.scrollBy({ top: moved, behavior: 'instant' });
    }
    if (page.busy) return;
    kept.current = null;
    // The disabled button drops focus while loading and is gone after the last page.
    if (!document.activeElement || document.activeElement === document.body)
      (button.current ?? list.current?.querySelector<HTMLElement>('.activity-list'))?.focus({
        preventScroll: true,
      });
  });
  useEffect(() => {
    if (!page.busy) return;
    // The owner may scroll while a page loads; keep the place they scrolled to.
    const follow = () => {
      if (kept.current) kept.current.top = kept.current.row.getBoundingClientRect().top;
    };
    window.addEventListener('scroll', follow, { passive: true });
    return () => window.removeEventListener('scroll', follow);
  }, [page.busy]);
  return (
    <>
      <DataError message={page.error} />
      {page.more && (
        <Button ref={button} variant="outline" disabled={page.busy} onClick={older}>
          {page.busy ? 'Loading…' : 'Show older'}
        </Button>
      )}
      <div ref={list}>
        <Feed events={events} />
      </div>
    </>
  );
}
