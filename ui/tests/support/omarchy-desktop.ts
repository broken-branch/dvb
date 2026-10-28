import type { paths } from '@/api/schema';
import type { Reply } from './engine-fixtures';

type Colors = Record<string, string>;

/** A waiting item as the widget lists it: dvb status --json's order, failed and stopped tasks then questions. */
const items = (
  status: Reply<paths['/status']['get']>,
  questions: Reply<paths['/questions']['get']>,
  projects: Reply<paths['/projects']['get']>,
) => {
  const name = (id: string) => projects.find((p) => p.id === id)?.name ?? id;
  return [
    ...(status.tasks ?? []).map((t) => ({
      title: t.title,
      kind: t.state.state.toUpperCase(),
      urgent: t.state.state === 'failed',
      project: name(t.project),
    })),
    ...questions
      .filter((q) => q.open)
      .map((q) => ({
        title: q.subject,
        kind: 'QUESTION',
        urgent: false,
        project: name(q.project),
      })),
  ];
};

const escape = (text: string) =>
  text.replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' })[c]!);

/**
 * The Omarchy plugin preview: an Omarchy-style desktop in one theme's colours, with the bar on top (the dvb
 * icon and its count badge), the cockpit at `cockpit` in a tiled window, and the widget's panel open over its
 * corner, drawn as Panel.qml draws it. The badge, the panel and the cockpit read the same fixtures.
 */
export function omarchyDesktop({
  colors,
  cockpit,
  status,
  questions,
  projects,
  now,
  timeZone,
}: {
  colors: Colors;
  now: number;
  timeZone: string;
  cockpit: string;
  status: Reply<paths['/status']['get']>;
  questions: Reply<paths['/questions']['get']>;
  projects: Reply<paths['/projects']['get']>;
}) {
  const c = colors;
  const clock = (options: Intl.DateTimeFormatOptions) =>
    new Intl.DateTimeFormat('en-GB', { timeZone, ...options }).format(now);
  const time = clock({ hour: '2-digit', minute: '2-digit' });
  const rows = items(status, questions, projects)
    .map(
      (item) => `<li>
        <div class="row-top"><span class="row-title">${escape(item.title)}</span><b class="kind${item.urgent ? ' urgent' : ''}">${item.kind}</b></div>
        <div class="project">${escape(item.project)}</div>
      </li>`,
    )
    .join('');
  return `<!doctype html>
<html><head><meta charset="utf-8"><style>
* { box-sizing: border-box; margin: 0; padding: 0; }
html, body { width: 100%; height: 100%; overflow: hidden; }
body {
  background: radial-gradient(circle at 30% 20%, ${c.lighter_background}, ${c.darker_background} 70%);
  color: ${c.foreground};
  font: 12px 'JetBrainsMono Nerd Font', monospace;
}
.bar {
  position: absolute; inset: 0 0 auto 0; height: 26px;
  display: grid; grid-template-columns: 1fr auto 1fr; align-items: center;
  padding: 0 8px; background: ${c.background};
}
.bar section { display: flex; gap: 15px; align-items: center; }
.bar section:last-child { justify-content: flex-end; }
.ws { opacity: 0.5; }
.ws.active { opacity: 1; }
.glyph { position: relative; display: inline-block; font-size: 14px; line-height: 1; }
.badge {
  position: absolute; left: calc(100% - 5px); bottom: calc(100% - 5px);
  min-width: 11px; height: 11px; padding: 0 2px; border-radius: 6px;
  background: ${c.red}; color: ${c.background}; outline: 1px solid ${c.background};
  font-size: 8px; font-weight: bold; line-height: 11px; text-align: center;
}
.window {
  position: absolute; top: 36px; left: 10px; right: 10px; bottom: 10px;
  border: 2px solid ${c.accent}; background: ${c.background};
}
.window iframe { display: block; width: 100%; height: 100%; border: 0; }
.panel {
  position: absolute; top: 32px; right: 10px; width: 336px; padding: 16px;
  background: ${c.background}; border: 2px solid ${c.accent};
  display: flex; flex-direction: column; gap: 10px;
  box-shadow: 0 8px 28px rgb(0 0 0 / 0.45);
}
.hero { display: flex; gap: 12px; align-items: center; }
.hero .glyph { font-size: 24px; }
.hero h1 { font-size: 14px; }
.hero p, h2, .kind { font-size: 10px; font-weight: bold; letter-spacing: 1.2px; }
.hero p, h2, .project, .updated { color: color-mix(in srgb, ${c.foreground} 62%, ${c.background}); }
hr { border: 0; border-top: 1px solid color-mix(in srgb, ${c.foreground} 18%, ${c.background}); }
ul { list-style: none; display: flex; flex-direction: column; gap: 2px; margin-top: 6px; }
li { padding: 7px 10px; border: 1px solid transparent; }
li:first-child { border-color: color-mix(in srgb, ${c.foreground} 30%, ${c.background}); background: ${c.lighter_background}; }
.row-top { display: flex; gap: 12px; align-items: baseline; }
.row-title { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.kind { color: color-mix(in srgb, ${c.foreground} 62%, ${c.background}); }
.kind.urgent { color: ${c.red}; }
.project { font-size: 10px; margin-top: 2px; }
.button {
  display: flex; gap: 8px; justify-content: center; padding: 7px;
  border: 1px solid color-mix(in srgb, ${c.foreground} 30%, ${c.background});
}
.updated { font-size: 10px; text-align: center; }
</style></head><body>
<header class="bar">
  <section><span class="ws active">1</span><span class="ws">2</span><span class="ws">3</span><span class="ws">4</span><span class="ws">5</span></section>
  <section>${clock({ weekday: 'long' })} ${time}</section>
  <section>
    <span class="glyph">󰘬<span class="badge">${status.needs_you > 9 ? '9+' : status.needs_you}</span></span>
    <span>󰂯</span><span>󰤨</span><span>󰕾</span><span>󰍛</span><span>󰁹</span>
  </section>
</header>
<main class="window"><iframe src="${escape(cockpit)}" title="dvb"></iframe></main>
<aside class="panel">
  <div class="hero"><span class="glyph">󰘬</span><div><h1>dvb</h1><p>${status.needs_you} NEED YOU</p></div></div>
  <hr>
  <div><h2>WAITING</h2><ul>${rows}</ul></div>
  <hr>
  <div class="button"><span>󰏌</span>Open cockpit</div>
  <div class="updated">Updated ${time}</div>
</aside>
</body></html>`;
}
