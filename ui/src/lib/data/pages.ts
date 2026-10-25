import { useEffect, useRef, useState } from 'react';
import { useStore } from '../store';
import type { Event, EventFilter } from './types';
import { refusalText } from './display';
import { mergeEvents } from './selectors';

/** Historical reads add rows only. They never enter the projection reducer. */
export function useEventPages(filter: EventFilter = {}) {
  const { data: s } = useStore();
  const key = JSON.stringify(filter);
  const [page, setPage] = useState<{
    key: string;
    events: Event[];
    before?: number | null;
    expanded?: boolean;
    cursor?: number;
    streamCursor?: number;
    gapBefore?: number;
  }>({ key, events: [] });
  const [busy, setBusy] = useState(false),
    [error, setError] = useState('');
  const read = s.eventPage;
  const refreshed = useRef({ events: s.events, connection: s.connection.state });
  useEffect(() => {
    let alive = true;
    setBusy(true);
    setError('');
    setPage({ key, events: [] });
    read(filter)
      .then((r) => {
        if (!alive) return;
        if (r.ok)
          setPage((old) => {
            const merged = mergeEvents(
              old.key === key ? old.events.filter((e) => e.id > r.value.cursor) : [],
              r.value.events,
            );
            const events = merged.slice(-(filter.limit ?? 50));
            return {
              key,
              events,
              before: merged.length > events.length ? events[0]?.id : r.value.before,
              cursor: r.value.cursor,
              streamCursor: Math.max(old.streamCursor ?? 0, r.value.cursor),
              gapBefore: old.gapBefore,
            };
          });
        else setError(refusalText(r.refusal));
      })
      .catch((e) => {
        if (alive) setError(String(e));
      })
      .finally(() => {
        if (alive) setBusy(false);
      });
    return () => {
      alive = false;
    };
  }, [key, read]);
  const scoped = !!(filter.task || filter.role);
  const mine = (e: Event) =>
    !scoped &&
    (!filter.project || e.project === filter.project) &&
    (!filter.kinds || filter.kinds.split(',').includes(e.kind));
  useEffect(() => {
    if (!scoped || page.key !== key || page.cursor === undefined) return;
    if (
      refreshed.current.events === s.events &&
      refreshed.current.connection === s.connection.state
    )
      return;
    refreshed.current = { events: s.events, connection: s.connection.state };
    let alive = true;
    setError('');
    const after = page.cursor;
    void (async () => {
      let before: number | undefined;
      let boundary = after;
      let rows: Event[] = [];
      do {
        const r = await read({ ...filter, before });
        if (!alive) return;
        if (!r.ok) throw new Error(refusalText(r.refusal));
        if (before === undefined) boundary = r.value.cursor;
        rows = mergeEvents(
          rows,
          r.value.events.filter((e) => e.id > after),
        );
        before = r.value.before && r.value.before > after ? r.value.before : undefined;
      } while (before);
      setPage((old) => {
        if (old.key !== key) return old;
        const merged = mergeEvents(old.events, rows);
        const events = old.expanded ? merged : merged.slice(-(filter.limit ?? 50));
        return {
          ...old,
          events,
          cursor: boundary,
          before: merged.length > events.length ? events[0]?.id : old.before,
        };
      });
    })().catch((e) => {
      if (alive) setError(String(e));
    });
    return () => {
      alive = false;
    };
  }, [key, read, s.events, s.connection.state, page.key, page.cursor, scoped]);
  useEffect(() => {
    if (scoped) return;
    setPage((old) => {
      if (old.key !== key) return old;
      const merged = mergeEvents(
        old.events,
        s.events.filter((e) => mine(e) && e.id > (old.cursor ?? 0)),
      );
      const events = old.expanded ? merged : merged.slice(-(filter.limit ?? 50));
      const gap = old.streamCursor !== undefined && (s.events[0]?.id ?? 0) > old.streamCursor + 1;
      const gapBefore = gap ? Math.max(old.gapBefore ?? 0, s.events[0].id) : old.gapBefore;
      const before = merged.length > events.length ? events[0]?.id : old.before;
      return {
        ...old,
        events,
        before,
        gapBefore,
        streamCursor: Math.max(old.streamCursor ?? 0, s.events.at(-1)?.id ?? 0),
      };
    });
  }, [s.events, key]);
  const merged = mergeEvents(
    page.key === key ? page.events : [],
    s.events.filter((e) => mine(e) && e.id > (page.cursor ?? 0)),
  );
  const events = page.expanded ? merged : merged.slice(-(filter.limit ?? 50));
  async function older() {
    const before = page.gapBefore ?? page.before;
    if (busy || !before) return;
    const fillingGap = page.gapBefore !== undefined;
    setPage((old) => (old.key === key ? { ...old, expanded: true } : old));
    setBusy(true);
    setError('');
    try {
      const r = await read({ ...filter, before });
      if (r.ok)
        setPage((old) => {
          if (old.key !== key) return old;
          // Historical completion cannot consume a gap discovered while awaiting
          // this request. A gap read likewise preserves a newer reconnect gap.
          const gapBefore =
            fillingGap && old.gapBefore === before
              ? r.value.before && r.value.before > (old.cursor ?? 0)
                ? r.value.before
                : undefined
              : old.gapBefore;
          return {
            ...old,
            events: mergeEvents(old.events, r.value.events),
            before: fillingGap ? old.before : r.value.before,
            gapBefore,
            expanded: true,
          };
        });
      else setError(refusalText(r.refusal));
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }
  return {
    events,
    older,
    more: page.key === key && !!(page.gapBefore ?? page.before),
    busy,
    error,
  };
}

/** Task details retain their scoped history, without downloading run output. */
export function useTaskEvents(task: string) {
  const { data: s } = useStore();
  const [history, setHistory] = useState<{ task: string; events: Event[] }>({ task, events: [] });
  const read = s.eventPage;
  const cursor = useRef({ task, id: 0 });
  const view = s.tasks.find((t) => t.id === task);
  const contextVersion = `${view?.head}/${view?.pr}/${view?.state.state}/${s.events.at(-1)?.id}`;
  const [error, setError] = useState('');
  useEffect(() => {
    let alive = true;
    setError('');
    void (async () => {
      const after = cursor.current.task === task ? cursor.current.id : 0;
      let before: number | undefined,
        boundary = after,
        rows: Event[] = [];
      do {
        const r = await read({
          task,
          before,
          kinds:
            'plan.proposed,task.started,task.failed,task.state,task.stopped,task.resumed,task.done,task.cancelled,run.started,run.ended,run.model,run.rerouted,run.preflight,run.collected,branch.published,branch.updated,pr.opened,pr.merged,checks.observed,verdict.recorded,decision.recorded,merge.refused,scan.hit,github.mirror_observed',
          limit: 200,
        });
        if (!alive) return;
        if (!r.ok) throw new Error(refusalText(r.refusal));
        if (before === undefined) boundary = r.value.cursor;
        rows = mergeEvents(
          rows,
          r.value.events.filter((e) => e.id > after),
        );
        before = r.value.before && r.value.before > after ? r.value.before : undefined;
        setHistory((old) => ({
          task,
          events: mergeEvents(old.task === task ? old.events : [], rows),
        }));
      } while (before);
      cursor.current = { task, id: boundary };
    })().catch((e) => {
      if (alive) setError(String(e));
    });
    return () => {
      alive = false;
    };
  }, [task, read, contextVersion, s.connection.state]);
  return {
    events: history.task === task ? history.events : [],
    error,
  };
}
