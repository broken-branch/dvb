import { describe, expect, it } from 'vitest';
import { applyPalette, contrast, palettes } from '@/lib/theme';
describe('theme', () => {
  it('keeps secondary text at WCAG AA on every surface it sits on', () => {
    for (const [id, palette] of Object.entries(palettes)) {
      applyPalette(palette, 'Auto');
      const token = (name: string) => document.documentElement.style.getPropertyValue(`--${name}`);
      for (const surface of ['background', 'card', 'popover', 'sidebar', 'muted', 'accent'])
        expect(
          contrast(token('muted-foreground'), token(surface)),
          `${id} (${palette.mode}) muted-foreground on ${surface}`,
        ).toBeGreaterThanOrEqual(4.5);
    }
  });
});
