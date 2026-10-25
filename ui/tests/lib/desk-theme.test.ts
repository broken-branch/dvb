import { flush } from '../support/api';
import { describe, it, expect, vi } from 'vitest';
import { deskPalette, followDeskTheme } from '@/lib/desk-theme';
describe('follow', () => {
  describe('desk theme bridge', () => {
    it('follows a changed theme without reloading and stops on disposal', async () => {
      vi.useFakeTimers();
      const fetcher = vi
        .fn<typeof fetch>()
        .mockResolvedValue(new Response(':root { --dvb-accent: #112233; --dvb-mode: light; }'));
      const receive = vi.fn(),
        stop = followDeskTheme(receive, fetcher);
      await flush();
      expect(receive).toHaveBeenCalledWith(
        expect.objectContaining({ accent: '#112233', mode: 'light' }),
      );
      fetcher.mockResolvedValue(new Response(':root { --dvb-accent: #445566; }'));
      await vi.advanceTimersByTimeAsync(1000);
      await flush();
      expect(receive).toHaveBeenLastCalledWith(expect.objectContaining({ accent: '#445566' }));
      stop();
      const count = fetcher.mock.calls.length;
      await vi.advanceTimersByTimeAsync(10000);
      expect(fetcher).toHaveBeenCalledTimes(count);
      expect(deskPalette('--dvb-accent: red; --dvb-background: url(evil);')).toBeUndefined();
    });
  });
});
