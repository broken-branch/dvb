import type { paths } from '@/api/schema';
import type { Reply } from './engine-fixtures';

const role = (slug: string, name: string) => ({
  id: `catalogue-${slug}`,
  slug,
  name,
  job: name,
  lane: name,
  lead: false,
  reports_to: slug === 'orchestrator' ? null : 'catalogue-orchestrator',
  reviewed_by: null,
  review_instructions: '',
});
const hire = (slug: string, model: string) => ({
  id: `hire-catalogue-${slug}`,
  role: `catalogue-${slug}`,
  model,
  effort: 'high' as const,
  status: 'active' as const,
});
const task = (
  id: string,
  title: string,
  slug: string,
  state: 'done' | 'in_review' | 'running' | 'planned',
  pr: number | null,
  model?: string,
) => ({
  id,
  title,
  project: 'catalogue',
  goal: 'catalogue-goal',
  role: `catalogue-${slug}`,
  review: 'code' as const,
  state: { state },
  head: pr ? 'c83b7a1' : null,
  pr,
  current_run: model
    ? {
        id: `run-${id}`,
        project: 'catalogue',
        task: id,
        role: `catalogue-${slug}`,
        hire: `hire-catalogue-${slug}`,
        model,
        vendor: model.startsWith('gpt') ? ('codex' as const) : ('claude' as const),
        effort: 'high' as const,
        reason: 'Continue the approved work.',
        started_at: 1790810460000,
      }
    : null,
});

/** The README screenshot (docs/cockpit.png): one small demo project with real model ids. */
export const demoResponses = {
  '/projects': [
    {
      id: 'catalogue',
      name: 'catalogue',
      repo: 'example-org/catalogue',
      default_branch: 'main',
      ci: 'no_ci',
      paused: false,
      last_work_at: 1790811240000,
      done_goals: 1,
      last_finished: { text: 'Add a search box to the catalogue.', pr: 1, at: 1790807640000 },
    },
  ] satisfies Reply<paths['/projects']['get']>,
  '/goals': [
    {
      id: 'catalogue-goal',
      project: 'catalogue',
      text: "Show prices in the visitor's currency.",
      state: 'approved',
    },
  ] satisfies Reply<paths['/goals']['get']>,
  '/tasks': [
    task('rates', 'Fetch daily exchange rates', 'backend', 'done', 2),
    task('locale', "Detect the visitor's currency", 'frontend', 'done', 3),
    task(
      'convert',
      'Convert prices on product pages',
      'code-reviewer',
      'in_review',
      4,
      'claude-opus-5-5',
    ),
    task('switcher', 'Add a currency switcher', 'frontend', 'running', null, 'gpt-6.1-sol'),
    task('checkout', 'Show the converted total at checkout', 'backend', 'planned', null),
  ] satisfies Reply<paths['/tasks']['get']>,
  '/questions': [] satisfies Reply<paths['/questions']['get']>,
  '/hires': [
    hire('orchestrator', 'claude-opus-5-5'),
    hire('frontend', 'gpt-6.1-sol'),
    hire('backend', 'gpt-6.1-sol'),
    hire('code-reviewer', 'claude-opus-5-5'),
  ] satisfies Reply<paths['/hires']['get']>,
  '/status': { running: 2, needs_you: 0 } satisfies Reply<paths['/status']['get']>,
  '/events/recent': { cursor: 0, events: [], activity: [] } satisfies Reply<
    paths['/events/recent']['get']
  >,
  '/projects/catalogue/roles': [
    role('orchestrator', 'Orchestrator'),
    role('frontend', 'Frontend builder'),
    role('backend', 'Backend builder'),
    role('code-reviewer', 'Code reviewer'),
  ] satisfies Reply<paths['/projects/{id}/roles']['get']>,
  '/projects/catalogue/brief': { text: '' } satisfies Reply<paths['/projects/{id}/brief']['get']>,
  '/projects/catalogue/issues': [] satisfies Reply<paths['/projects/{id}/issues']['get']>,
  '/projects/catalogue/issues/counts': { open: 0, trusted: 0, labels: [] } satisfies Reply<
    paths['/projects/{id}/issues/counts']['get']
  >,
  'POST /projects/catalogue/seen': undefined,
};

/** The colour roles the desk reads from each Omarchy theme's colors.toml, copied from Omarchy. */
const omarchyThemes = {
  'tokyo-night': {
    mode: 'dark',
    accent: '#7aa2f7',
    selection: '#292e42',
    background: '#1a1b26',
    dark_background: '#13141c',
    darker_background: '#0e0e14',
    lighter_background: '#24283b',
    foreground: '#a9b1d6',
    red: '#f7768e',
    yellow: '#e0af68',
    green: '#9ece6a',
    cyan: '#449dab',
    blue: '#7aa2f7',
    magenta: '#ad8ee6',
  },
  catppuccin: {
    mode: 'dark',
    accent: '#89b4fa',
    selection: '#45475a',
    background: '#1e1e2e',
    dark_background: '#161622',
    darker_background: '#101019',
    lighter_background: '#313244',
    foreground: '#cdd6f4',
    red: '#f38ba8',
    yellow: '#f9e2af',
    green: '#a6e3a1',
    cyan: '#94e2d5',
    blue: '#89b4fa',
    magenta: '#f5c2e7',
  },
  gruvbox: {
    mode: 'dark',
    accent: '#7daea3',
    selection: '#504945',
    background: '#282828',
    dark_background: '#1e1e1e',
    darker_background: '#161616',
    lighter_background: '#3c3836',
    foreground: '#d4be98',
    red: '#ea6962',
    yellow: '#d8a657',
    green: '#a9b665',
    cyan: '#89b482',
    blue: '#7daea3',
    magenta: '#d3869b',
  },
};

/** The theme screenshots (docs/themes/*.png): /theme.css as the desk serves it in each theme. */
export const demoThemes = Object.entries(omarchyThemes).map(([name, colors]) => ({
  name,
  background: colors.background,
  css: `:root {\n${Object.entries(colors)
    .map(([key, value]) => `  --dvb-${key}: ${value};`)
    .join('\n')}\n}\n`,
}));
