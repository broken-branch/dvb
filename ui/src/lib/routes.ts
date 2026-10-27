/** Hash routes use explicit project and role segments. */
/** The project or task page a question was opened from, for its back link. */
export type Origin = { page: 'project' | 'task'; id: string };
export type Route =
  | { page: 'home' | 'projects' | 'usage' | 'setup' }
  | { page: 'decision'; id?: string; from?: Origin }
  | { page: 'project' | 'task'; id: string; tab?: string }
  | { page: 'settings'; tab?: string }
  | { page: 'company'; profile?: string; project?: string };
const part = encodeURIComponent;
export function buildRoute(route: Route): string {
  switch (route.page) {
    case 'project':
    case 'task':
      return `${route.page}/${part(route.id)}${route.tab ? `/${part(route.tab)}` : ''}`;
    case 'decision':
      return `decision${route.id ? `/${part(route.id)}` : ''}${route.id && route.from ? `/${route.from.page}/${part(route.from.id)}` : ''}`;
    case 'settings':
      return `settings${route.tab ? `/${part(route.tab)}` : ''}`;
    case 'company': {
      const base = route.project ? `company/project/${part(route.project)}` : 'company';
      return route.profile ? `${base}/role/${part(route.profile)}` : base;
    }
    default:
      return route.page;
  }
}
function decode(value = '') {
  try {
    return decodeURIComponent(value);
  } catch {
    return value;
  }
}
export function parseRoute(hash: string): Route {
  const parts = hash.replace(/^#/, '').split('/').map(decode);
  const [page, id, tab, project] = parts;
  switch (page) {
    case 'home':
    case 'projects':
    case 'usage':
    case 'setup':
      return { page };
    case 'project':
    case 'task':
      return { page, id: id || '', tab: tab || undefined };
    case 'decision':
      return {
        page,
        id: id || undefined,
        from:
          id && (tab === 'project' || tab === 'task') && project
            ? { page: tab, id: project }
            : undefined,
      };
    case 'settings':
      return { page, tab: id || undefined };
    case 'company':
      if (id === 'project' && (parts.length === 3 || project === 'role'))
        return { page, project: tab || undefined, profile: parts[4] || undefined };
      if (id === 'role' && parts.length === 3) return { page, profile: tab || undefined };
      return { page };
    default:
      return { page: 'home' };
  }
}
export const routes = {
  home: () => buildRoute({ page: 'home' }),
  projects: () => buildRoute({ page: 'projects' }),
  usage: () => buildRoute({ page: 'usage' }),
  setup: () => buildRoute({ page: 'setup' }),
  project: (id: string, tab?: string) => buildRoute({ page: 'project', id, tab }),
  task: (id: string, tab?: string) => buildRoute({ page: 'task', id, tab }),
  decision: (id?: string, from?: Origin) => buildRoute({ page: 'decision', id, from }),
  settings: (tab?: string) => buildRoute({ page: 'settings', tab }),
  company: (profile?: string, project?: string) =>
    buildRoute({ page: 'company', profile, project }),
};
