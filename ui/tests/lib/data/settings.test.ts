import { expect, it, describe } from 'vitest';
import { parseSettings, settingDefaults } from '@/lib/data/settings';
describe('parsing', () => {
  it('parses each settings snapshot once and defaults missing values', () => {
    expect(parseSettings({ values: {} })).toEqual(settingDefaults);
    const settings = { values: { text_size: 'XL', notify_faults: false } };
    const parsed = parseSettings(settings);
    expect(parseSettings(settings)).toBe(parsed);
    expect(parsed).toMatchObject({ text_size: 'XL', notify_faults: false, max_runs: 3 });
  });
  it('reads a custom or unknown saved theme as auto', () => {
    for (const theme of ['auto', 'tokyo-night'])
      expect(parseSettings({ values: { theme } }).theme).toBe(theme);
    for (const theme of ['custom', 'retired-theme', 5])
      expect(parseSettings({ values: { theme } }).theme).toBe('auto');
  });
});
