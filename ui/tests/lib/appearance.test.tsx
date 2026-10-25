import { memo } from 'react';
import { act } from '@testing-library/react';
import { expect, it, vi, describe } from 'vitest';
import { useAppearance, useAppReducedMotion } from '@/lib/appearance';
import { mount } from '../support/render';
describe('isolation', () => {
  it('appearance and motion consumers retain their context values on unrelated events', async () => {
    const appearance = vi.fn(),
      motion = vi.fn();
    const Appearance = memo(() => {
      appearance(useAppearance().theme);
      return null;
    });
    const Motion = memo(() => {
      motion(useAppReducedMotion());
      return null;
    });
    const { source } = mount(
      <>
        <Appearance />
        <Motion />
      </>,
    );
    appearance.mockClear();
    motion.mockClear();
    await act(async () => source.set({ status: { running: 42, needs_you: 5 } }));
    expect(appearance).not.toHaveBeenCalled();
    expect(motion).not.toHaveBeenCalled();
    await act(async () => {
      source.set({
        settings: { values: { ...source.getSnapshot().settings.values, notify_completed: true } },
      });
    });
    expect(appearance).not.toHaveBeenCalled();
    expect(motion).not.toHaveBeenCalled();
    await act(async () => {
      source.set({
        settings: { values: { ...source.getSnapshot().settings.values, theme: 'tokyo-night' } },
      });
    });
    expect(appearance).toHaveBeenCalledWith('tokyo-night');
  });
});
