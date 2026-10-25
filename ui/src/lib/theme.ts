import { palettes, type Palette } from './palettes';
export { palettes };
export type { Palette };
const mix = (a: string, b: string, weight: number) =>
  '#' +
  [0, 1, 2]
    .map((i) =>
      Math.round(
        parseInt(a.slice(1 + i * 2, 3 + i * 2), 16) * (1 - weight) +
          parseInt(b.slice(1 + i * 2, 3 + i * 2), 16) * weight,
      )
        .toString(16)
        .padStart(2, '0'),
    )
    .join('');
const luminance = (hex: string) => {
  const rgb = [1, 3, 5]
    .map((i) => parseInt(hex.slice(i, i + 2), 16) / 255)
    .map((v) => (v <= 0.04045 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4));
  return rgb[0] * 0.2126 + rgb[1] * 0.7152 + rgb[2] * 0.0722;
};
export const contrast = (a: string, b: string) =>
  (Math.max(luminance(a), luminance(b)) + 0.05) / (Math.min(luminance(a), luminance(b)) + 0.05);
function readableOn(ink: string, surfaces: string[], target = 4.8) {
  if (surfaces.every((surface) => contrast(ink, surface) >= target)) return ink;
  const end =
    surfaces.reduce((sum, s) => sum + contrast('#ffffff', s), 0) >
    surfaces.reduce((sum, s) => sum + contrast('#000000', s), 0)
      ? '#ffffff'
      : '#000000';
  for (let w = 0.02; w <= 1.01; w += 0.02) {
    const candidate = mix(ink, end, Math.min(1, w));
    if (surfaces.every((surface) => contrast(candidate, surface) >= target)) return candidate;
  }
  return end;
}
export function applyPalette(p: Palette, font: string) {
  const bg = p.background,
    card = p.lighter_background || mix(bg, p.foreground, 0.04),
    side = p.dark_background || bg;
  const fg = readableOn(p.foreground, [bg, card]),
    cardFg = fg,
    selected = p.selection || mix(bg, fg, 0.12);
  const primary = readableOn(p.accent, [bg], 3),
    // Secondary text also sits on selected rows and tab lists, the lightest dark surface.
    muted = readableOn(mix(bg, fg, 0.65), [bg, card, side, selected]);
  const success = readableOn(p.green, [bg, card]),
    destructive = readableOn(p.red, [bg, card]),
    warning = readableOn(p.yellow, [bg, card]);
  const roles: Record<string, string> = {
    background: bg,
    foreground: fg,
    card,
    'card-foreground': cardFg,
    popover: card,
    'popover-foreground': cardFg,
    primary,
    'primary-foreground': readableOn(bg, [primary], 4.5),
    secondary: card,
    'secondary-foreground': cardFg,
    muted: selected,
    'muted-foreground': muted,
    accent: selected,
    'accent-foreground': readableOn(fg, [selected], 4.5),
    border: mix(bg, fg, 0.17),
    input: readableOn(mix(bg, fg, 0.22), [card], 3),
    ring: readableOn(p.accent, [card], 3),
    destructive,
    success,
    warning,
    'sidebar-warning': readableOn(warning, [side, selected]),
    'diff-add-foreground': readableOn(success, [mix(card, success, 0.07)], 4.8),
    'diff-remove-foreground': readableOn(destructive, [mix(card, destructive, 0.07)], 4.8),
    'accent-ink': readableOn(p.accent, [bg, card]),
    'shadow-color': p.darker_background || bg,
    sidebar: side,
    'sidebar-foreground': readableOn(fg, [side], 4.5),
    'sidebar-primary': primary,
    'sidebar-primary-foreground': readableOn(bg, [primary], 4.5),
    'sidebar-accent': selected,
    'sidebar-accent-foreground': readableOn(fg, [selected], 4.5),
    'sidebar-border': mix(side, fg, 0.14),
    'sidebar-ring': readableOn(p.accent, [side], 3),
    'logo-start': p.accent,
    'logo-end': p.magenta || p.accent,
  };
  (['accent', 'blue', 'cyan', 'magenta', 'green', 'yellow', 'red'] as const).forEach(
    (colour) =>
      (roles[`identity-${colour}`] = readableOn(p[colour] || p.accent, [bg, card, side, selected])),
  );
  Object.entries(p).forEach(([key, value]) => {
    if (typeof value === 'string' && value.startsWith('#')) roles[`palette-${key}`] = value;
  });
  Object.entries(roles).forEach(([k, v]) =>
    document.documentElement.style.setProperty(`--${k}`, v),
  );
  document.documentElement.style.setProperty(
    '--app-font',
    font === 'Auto' ? 'ui-monospace, monospace' : `"${font}", monospace`,
  );
  document.documentElement.style.colorScheme = p.mode;
  document.documentElement.classList.toggle('dark', p.mode !== 'light');
}
export const themeName = (id: string) =>
  id === 'current'
    ? 'Event Horizon'
    : id
        .split('-')
        .map((s) => s[0].toUpperCase() + s.slice(1))
        .join(' ');
