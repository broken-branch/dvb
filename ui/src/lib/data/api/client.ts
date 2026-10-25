import type { paths } from '@/api/schema';
import type { ApiResult, Connection, Refusal } from '../types';
type Method<P extends keyof paths> = Extract<keyof paths[P], 'get' | 'post' | 'put' | 'patch'>;
type Success<O> = O extends {
  responses: infer R;
}
  ? {
      [K in Extract<keyof R, 200 | 201 | 202 | 204>]: R[K] extends {
        content: {
          'application/json': infer T;
        };
      }
        ? T
        : void;
    }[Extract<keyof R, 200 | 201 | 202 | 204>]
  : never;
type Body<O> = O extends {
  requestBody?: infer B;
}
  ? [NonNullable<B>] extends [never]
    ? never
    : NonNullable<B> extends {
          content: {
            'application/json': infer T;
          };
        }
      ? T
      : never
  : never;
type PathParams<O> = O extends { parameters: { path: infer T } } ? T : never;
export type Fetch = typeof fetch;
/** Types come from the generated OpenAPI operations. Paths are relative to the desk. */
export function createClient(
  fetcher: Fetch,
  base: string,
  appVersion: string,
  connection: () => Connection,
  setConnection: (value: Connection) => void,
) {
  const version = (value: string | null) => {
    if (value)
      setConnection(
        value === appVersion
          ? { state: 'connected', engineVersion: value }
          : { state: 'version-mismatch', engineVersion: value, appVersion },
      );
  };
  async function call<P extends keyof paths, M extends Method<P>>(
    template: P,
    method: M,
    options: {
      path?: string;
      params?: PathParams<paths[P][M]>;
      body?: Body<paths[P][M]>;
      query?: Record<string, string | number | boolean | undefined>;
      signal?: AbortSignal;
    } = {},
  ): Promise<ApiResult<Success<paths[P][M]>>> {
    if (method !== 'get') {
      const link = connection();
      if (link.state === 'version-mismatch')
        return {
          ok: false,
          refusal: {
            code: 'VERSION_MISMATCH',
            client: appVersion,
            engine: link.engineVersion,
            detail: 'Update dvb before making changes.',
          },
        };
      if (link.state !== 'connected')
        throw new Error('Waiting to reconnect before making changes.');
    }
    const query = new URLSearchParams(
      Object.entries(options.query || {})
        .filter((entry): entry is [string, string | number] => entry[1] !== undefined)
        .map(([key, value]) => [key, String(value)]),
    );
    const response = await fetcher(
      `${base}${options.path || template.replace(/\{([^}]+)\}/g, (_, key: string) => encodeURIComponent((options.params as Record<string, string> | undefined)?.[key] ?? ''))}${query.size ? `?${query}` : ''}`,
      {
        method: method.toUpperCase(),
        credentials: 'same-origin',
        signal: options.signal,
        headers: {
          Accept: 'application/json',
          ...(options.body ? { 'Content-Type': 'application/json' } : {}),
        },
        body: options.body ? JSON.stringify(options.body) : undefined,
      },
    );
    version(response.headers.get('dvb-version'));
    // The desk's GitHub bridge replaces unsuccessful engine replies with plain text.
    if (!response.ok && response.headers.get('Content-Type')?.startsWith('text/plain'))
      throw new Error((await response.text()).trim() || `Request failed (${response.status}).`);
    if (!response.ok) return { ok: false, refusal: (await response.json()) as Refusal };
    return {
      ok: true,
      value: (response.status === 204 ? undefined : await response.json()) as Success<paths[P][M]>,
    };
  }
  return { call, version };
}
