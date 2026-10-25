import { it, expect, vi, describe } from 'vitest';
import { act, render, screen, waitFor } from '@testing-library/react';
import App from '@/App';
import { createFixtureSource } from '../support/render';
describe('notifications', () => {
  function fireHash() {
    act(() => window.dispatchEvent(new HashChangeEvent('hashchange')));
  }
  it('an open cockpit follows a desk notification route and closes its stream on unmount', async () => {
    class Events extends EventTarget {
      close = vi.fn();
    }
    const events = new Events(),
      create = vi.fn(function () {
        return events;
      });
    vi.stubGlobal('EventSource', create);
    history.replaceState({}, '', '/#home');
    const source = createFixtureSource();
    const view = render(<App source={source} />);
    expect(create).toHaveBeenCalledWith('/desk/events');
    act(() => events.dispatchEvent(new MessageEvent('navigate', { data: 'decision/local' })));
    await waitFor(() => expect(location.hash).toBe('#decision/local'));
    fireHash();
    expect(await screen.findByRole('button', { name: 'Send answer' })).toBeInTheDocument();
    for (const route of ['setup', 'settings', 'home', 'task/my_task-1', '']) {
      act(() => events.dispatchEvent(new MessageEvent('navigate', { data: route })));
      expect(location.hash).toBe(route ? `#${route}` : '');
    }
    view.unmount();
    expect(events.close).toHaveBeenCalledOnce();
    history.replaceState({}, '', '/');
  });
});
