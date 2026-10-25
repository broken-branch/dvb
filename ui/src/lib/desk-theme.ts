import { palettes, type Palette } from './theme';

/** Only accept desk palette roles, never executable CSS or arbitrary properties. */
export function deskPalette(css: string): Palette | undefined {
  const colors: Palette = { ...palettes.current };
  let found = false;
  for (const match of css.matchAll(/--dvb-([a-z_]+):\s*(#[0-9a-f]{6}|dark|light)\s*;/gi)) {
    const [, key, value] = match;
    if (key in colors) {
      colors[key as keyof Palette] = value;
      found = true;
    }
  }
  return found ? colors : undefined;
}

/** Closing the window disposes the poll; a theme swap does not reload the page. */
export function followDeskTheme(
  receive: (palette: Palette) => void,
  fetcher: typeof fetch = fetch,
) {
  const controller = new AbortController();
  let timer: ReturnType<typeof setTimeout> | undefined,
    previous = '';
  async function poll() {
    try {
      const response = await fetcher('/theme.css', {
        credentials: 'same-origin',
        cache: 'no-store',
        signal: controller.signal,
      });
      if (response.ok) {
        const css = await response.text();
        if (!controller.signal.aborted && css !== previous) {
          const palette = deskPalette(css);
          if (palette) receive(palette);
          previous = css;
        }
      }
    } catch {
      /* Retain the last palette during a desk restart. */
    }
    if (!controller.signal.aborted) timer = setTimeout(poll, 1000);
  }
  void poll();
  return () => {
    controller.abort();
    clearTimeout(timer);
  };
}
