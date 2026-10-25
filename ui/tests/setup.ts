import '@testing-library/jest-dom/vitest';
import { afterEach, vi } from 'vitest';
import { cleanup } from '@testing-library/react';
afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  vi.useRealTimers();
});
Object.defineProperty(window, 'matchMedia', {
  writable: true,
  value: vi.fn().mockImplementation((query) => ({
    matches: false,
    media: query,
    addEventListener() {},
    removeEventListener() {},
  })),
});
class ResizeObserverStub {
  observe() {}
  unobserve() {}
  disconnect() {}
}
class EventSourceStub extends EventTarget {
  close() {}
}
vi.stubGlobal('EventSource', EventSourceStub);
vi.stubGlobal('ResizeObserver', ResizeObserverStub);
Element.prototype.scrollIntoView = vi.fn();
window.scrollTo = vi.fn();
window.scrollBy = vi.fn();
Element.prototype.hasPointerCapture = () => false;
Element.prototype.setPointerCapture = () => {};
Element.prototype.releasePointerCapture = () => {};

// The desk injects this in the real app; tests play a 0.1.0 desk.
document.head.insertAdjacentHTML('beforeend', '<meta name="dvb-version" content="0.1.0">');
