import { stateLabels } from './data/display';
export const boardColumns = [
  ['blocked', 'Blocked', 'warn'],
  ['planned', stateLabels.planned, 'neutral'],
  ['working', stateLabels.running, 'working'],
  ['review', stateLabels.in_review, 'working'],
  ['changes', 'Changes requested', 'warn'],
  ['checks', stateLabels.awaiting_checks, 'working'],
  ['ready', stateLabels.merging, 'good'],
  ['stopped', stateLabels.stopped, 'bad'],
  ['done', stateLabels.done, 'good'],
] as const;
export type BoardStatus = (typeof boardColumns)[number][0];
export type BoardTask = {
  id: string;
  project: string;
  title: string;
  status: BoardStatus;
  role: string;
  ci: string;
  reason?: string;
  pr?: number;
  routine: boolean;
  roundModel?: string;
};
