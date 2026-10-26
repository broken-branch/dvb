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
