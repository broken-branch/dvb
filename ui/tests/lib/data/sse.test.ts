import { it, expect, vi } from 'vitest';
import { readEvents } from '@/lib/data/api/sse';
import { stream } from '../../support/api';
it('parses chunked CRLF, comments and multiline SSE data', async () => {
  const receive = vi.fn();
  const input = ': keepalive\r\nid: 4\r\ndata: first\r\ndata: second\r\n\r\n';
  await readEvents(stream(input.split('')), receive);
  expect(receive).toHaveBeenCalledExactlyOnceWith('first\nsecond', '4');
});
it('recognises the engine backlog boundary even when split across chunks', async () => {
  const live = vi.fn();
  await readEvents(stream([': li', 've\r', '\n\r\n']), vi.fn(), live);
  expect(live).toHaveBeenCalledOnce();
});
