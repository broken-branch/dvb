import { it, expect, vi, describe } from 'vitest';
import { act, screen } from '@testing-library/react';
import { mount } from '../support/render';
import { TextDisclosure } from '@/components/dvb/text-disclosure';
describe('overflow', () => {
  it('discloses only measured overflow and updates on resize', async () => {
    let height = 30;
    vi.spyOn(HTMLElement.prototype, 'clientHeight', 'get').mockImplementation(() => 30);
    vi.spyOn(HTMLElement.prototype, 'scrollHeight', 'get').mockImplementation(() => height);
    const { user } = mount(<TextDisclosure text="Measured question" />);
    expect(screen.queryByRole('button', { name: /Show more/ })).not.toBeInTheDocument();
    height = 90;
    act(() => window.dispatchEvent(new Event('resize')));
    await user.click(screen.getByRole('button', { name: /Show more/ }));
    expect(screen.getByRole('button', { name: /Show less/ })).toHaveAttribute(
      'aria-expanded',
      'true',
    );
    await user.click(screen.getByRole('button', { name: /Show less/ }));
    expect(screen.getByRole('button', { name: /Show more/ })).toHaveAttribute(
      'aria-expanded',
      'false',
    );
    height = 30;
    act(() => window.dispatchEvent(new Event('resize')));
    expect(screen.queryByRole('button', { name: /Show more/ })).not.toBeInTheDocument();
  });
});
