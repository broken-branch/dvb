import {
  createContext,
  useContext,
  useEffect,
  useState,
  useMemo,
  useSyncExternalStore,
  type ReactNode,
} from 'react';
import { createLiveSource } from './data/api';
import type { DataSource } from './data/source';
import { createBoardCards } from './data/board';
import { mergeEvents } from './data/selectors';
import { parseSettings } from './data/settings';
import { needsYou } from './data/work';
import type { Event } from './data/types';
type Snapshot = ReturnType<DataSource['getSnapshot']>;
type Store = {
  data: Snapshot;
  issue: string;
  canWrite: boolean;
  settings: ReturnType<typeof parseSettings>;
  evidence: Event[];
  needsYou: ReturnType<typeof needsYou>;
  boardCards: ReturnType<ReturnType<typeof createBoardCards>>;
};
const Context = createContext<Store | null>(null);
export function StoreProvider({
  children,
  source: supplied,
}: {
  children: ReactNode;
  source?: DataSource;
}) {
  const [source] = useState(() => supplied || createLiveSource());
  const data = useSyncExternalStore(source.subscribe, source.getSnapshot);
  useEffect(() => {
    source.start();
    return () => source.dispose();
  }, [source]);
  const evidence = useMemo(
    () => mergeEvents(data.evidence, data.events),
    [data.evidence, data.events],
  );
  const [deriveCards] = useState(createBoardCards);
  const boardCards = useMemo(
    () => deriveCards(data.tasks, evidence),
    [deriveCards, data.tasks, evidence],
  );
  const settings = parseSettings(data.settings);
  const attention = useMemo(() => needsYou(data), [data.questions, data.status]);
  const value: Store = useMemo(
    () => ({
      data,
      issue: source.getIssue?.() || '',
      canWrite: data.connection.state === 'connected',
      settings,
      evidence,
      needsYou: attention,
      boardCards,
    }),
    [data, source, evidence, settings, attention, boardCards],
  );
  return <Context.Provider value={value}>{children}</Context.Provider>;
}
export function useStore() {
  const value = useContext(Context);
  if (!value) throw new Error('StoreProvider required');
  return value;
}
export function go(path: string) {
  location.hash = path;
}
