import { vi } from 'vitest';
export const json = (value: unknown, version = '0.1.0', status = 200) =>
  new Response(JSON.stringify(value), {
    status,
    headers: { 'dvb-version': version, 'Content-Type': 'application/json' },
  });
export const stream = (chunks: string[]) =>
  new ReadableStream<Uint8Array>({
    start(controller) {
      chunks.forEach((chunk) => controller.enqueue(new TextEncoder().encode(chunk)));
      controller.close();
    },
  });
export const flush = async () => {
  for (let i = 0; i < 50; i++) await Promise.resolve();
};
export function fakeApi(eventResponses: Response[], version = '0.1.0') {
  return vi.fn<typeof fetch>().mockImplementation(async (input) => {
    const url = String(input);
    if (url.startsWith('/events/recent'))
      return json({ cursor: 20, events: [], before: null, activity: [] }, version);
    if (url.startsWith('/events?'))
      return (
        eventResponses.shift() || new Response(stream([]), { headers: { 'dvb-version': version } })
      );
    if (url === '/version') return json({ version }, version);
    if (url === '/status') return json({ running: 0, needs_you: 0 }, version);
    if (url === '/settings') return json({ values: {} }, version);
    if (url === '/projects')
      return json(
        [
          {
            id: 'p',
            name: 'Project',
            repo: 'example-org/repo',
            default_branch: 'main',
            ci: 'no_ci',
            paused: false,
          },
        ],
        version,
      );
    return json([], version);
  });
}
