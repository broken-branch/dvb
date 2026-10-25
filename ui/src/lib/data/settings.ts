import type { components } from '@/api/schema';
import type { Settings } from './types';
import { palettes } from '../palettes';
export const textSizes = { S: 0.6, M: 0.8, L: 1, XL: 1.25 } as const;
export type TextSize = keyof typeof textSizes;
export type MotionPreference = 'Follow system' | 'On' | 'Off';
export type SettingValues = {
  response_style: components['schemas']['ResponseStyle'];
  question_format: components['schemas']['QuestionFormat'];
  notify_faults: boolean;
  notify_completed: boolean;
  model_pool: string[];
  model_mode: 'Custom' | 'Auto';
  vendor_fallback: boolean;
  max_runs: number;
  theme: string;
  font: string;
  text_size: TextSize;
  motion: MotionPreference;
  task_panels: { review: boolean; checks: boolean };
  task_files_right: boolean;
  autonomous_projects: string[];
};
export const settingDefaults: SettingValues = {
  response_style: 'Clear',
  question_format: 'quick_compare',
  notify_faults: true,
  notify_completed: false,
  model_pool: [],
  model_mode: 'Custom',
  vendor_fallback: true,
  max_runs: 3,
  theme: 'auto',
  font: 'Auto',
  text_size: 'L',
  motion: 'Follow system',
  task_panels: { review: true, checks: true },
  task_files_right: false,
  autonomous_projects: [],
};
const cache = new WeakMap<Settings, SettingValues>();
/** Read each immutable settings snapshot once; the engine validated and defaulted its values. */
export function parseSettings(settings: Settings): SettingValues {
  const cached = cache.get(settings);
  if (cached) return cached;
  const values = { ...settingDefaults, ...settings.values } as SettingValues;
  // A custom or retired theme name reads as auto.
  if (!['auto', ...Object.keys(palettes)].includes(values.theme)) values.theme = 'auto';
  cache.set(settings, values);
  return values;
}
