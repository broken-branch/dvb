import { vi } from 'vitest';
import { render } from '@testing-library/react';
import userEvent, { PointerEventsCheckLevel } from '@testing-library/user-event';
import { cloneElement, isValidElement, useEffect, useState, type ReactNode } from 'react';
import { TaskPage } from '@/screens/task';
import { ProjectPage } from '@/screens/project';
import { Settings } from '@/screens/settings';
import { parseRoute, routes, type Route } from '@/lib/routes';
import { StoreProvider } from '@/lib/store';
import { AppearanceProvider } from '@/lib/appearance';
import { TooltipProvider } from '@/components/ui/tooltip';
import { createLiveSource } from '@/lib/data/api';
import { responses, mutationResponses } from './engine-fixtures';
import type { Fetch } from '@/lib/data/api/client';

export function createFixtureSource(options: { fetch?: Fetch } = {}) {
  const replies: Record<string, unknown> = structuredClone({ ...responses, ...mutationResponses });
  replies['/events/recent'] = {
    cursor: 0,
    events: [],
    before: null,
    activity: [],
  } satisfies import('@/api/schema').components['schemas']['EventPage'];
  replies['/github/checks'] = {
    status: 'discovered',
    default_branch: 'main',
    head: 'abcdef123',
    checks: [{ name: 'tests', status: 'success', conclusion: 'success', run: 1, url: null }],
  } satisfies import('@/api/schema').components['schemas']['GithubCheckDiscovery'];
  replies['/github/repositories'] = {
    organization: 'example-org',
    repository_access_url: 'https://github.com/organizations/example-org/settings/installations/1',
    repositories: [{ repo: 'example-org/notes', private: true, default_branch: 'main' }],
  } satisfies import('@/api/schema').components['schemas']['GithubRepositories'];
  replies['/github/prefill'] = {
    organization: null,
    owner_name: null,
    owner_email: null,
  } satisfies import('@/api/schema').components['schemas']['GithubPrefill'];
  const statuses: Record<string, number> = {};
  const eventPages: Record<string, import('@/lib/data/types').Event[]> = {
    'task:resume': structuredClone(
      responses['/events/recent'].events.filter((e) => [101, 102].includes(e.id)),
    ),
    'task:checks': structuredClone(
      responses['/events/recent'].events.filter((e) => [112, 113].includes(e.id)),
    ),
  };
  const fetcher = vi.fn<Fetch>(async (input, options) => {
    const url = new URL(String(input), 'http://localhost');
    const key = `${options?.method && options.method !== 'GET' ? `${options.method} ` : ''}${url.pathname.replace(/^\/api/, '')}`;
    const scope = url.searchParams.get('task') ? `task:${url.searchParams.get('task')}` : undefined;
    const rows = scope && eventPages[scope];
    const value =
      key === '/events/recent' && rows
        ? { cursor: rows.at(-1)?.id ?? 0, before: null, activity: [], events: rows }
        : replies[key];
    if (!(key in replies)) throw new Error(`Missing engine fixture: ${key}`);
    return new Response(value === undefined ? null : JSON.stringify(value), {
      status: statuses[key] ?? (value === undefined ? 204 : 200),
      headers: { 'Content-Type': 'application/json' },
    });
  });
  const source = createLiveSource({ fetch: options.fetch || fetcher, version: '0.1.0' });
  source.set({
    projects: structuredClone(responses['/projects']),
    goals: structuredClone(responses['/goals']),
    tasks: structuredClone(responses['/tasks']),
    questions: structuredClone(responses['/questions']),
    hires: structuredClone(responses['/hires']),
    settings: structuredClone(responses['/settings']),
    status: structuredClone(responses['/status']),
    models: structuredClone(responses['/models']),
    roles: {
      harbour: structuredClone(responses['/projects/harbour/roles']),
      fieldnotes: structuredClone(responses['/projects/fieldnotes/roles']),
    },
    events: structuredClone(responses['/events/recent'].events),
    connection: { state: 'connected', engineVersion: '0.1.0' },
  });
  // Screen tests begin at a fixed hydrated snapshot; reads and writes use the real client.
  return { ...source, replies, statuses, eventPages, fetch: fetcher, start() {}, dispose() {} };
}

export function mount(child: ReactNode, source = createFixtureSource()) {
  if (isValidElement<{ id?: string; tab?: string }>(child)) {
    const path =
      child.type === TaskPage
        ? routes.task(child.props.id!, child.props.tab || 'changes')
        : child.type === ProjectPage
          ? routes.project(child.props.id!, child.props.tab || 'board')
          : child.type === Settings
            ? routes.settings(child.props.tab || 'appearance')
            : undefined;
    if (path) history.replaceState({}, '', `/#${path}`);
  }
  const view = render(child, {
    wrapper: function FixtureProviders({ children }) {
      const [route, setRoute] = useState<Route>();
      useEffect(() => {
        const navigate = () => setRoute(parseRoute(location.hash));
        window.addEventListener('hashchange', navigate);
        return () => window.removeEventListener('hashchange', navigate);
      }, []);
      let page = children;
      if (
        isValidElement<{ id?: string; tab?: string }>(children) &&
        route &&
        ((children.type === TaskPage && route.page === 'task' && route.id === children.props.id) ||
          (children.type === ProjectPage &&
            route.page === 'project' &&
            route.id === children.props.id) ||
          (children.type === Settings && route.page === 'settings'))
      )
        page = cloneElement(children, { tab: 'tab' in route ? route.tab : undefined });
      return (
        <StoreProvider source={source}>
          <AppearanceProvider>
            <TooltipProvider>{page}</TooltipProvider>
          </AppearanceProvider>
        </StoreProvider>
      );
    },
  });
  return {
    ...view,
    source,
    // jsdom screens load no CSS; Chrome checks rendered controls.
    user: userEvent.setup({ pointerEventsCheck: PointerEventsCheckLevel.Never }),
  };
}
