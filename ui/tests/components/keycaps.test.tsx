import { expect, it, describe } from 'vitest';
import { render, screen } from '@testing-library/react';
import { Keycaps } from '@/components/ui/keycaps';
describe('rendering', () => {
  it('renders each key as a kbd, with accessible combination separators and or between alternatives', () => {
    const { container } = render(<Keycaps keys={[['Ctrl', 'K'], ['/']]} />);
    expect([...container.querySelectorAll('kbd')].map((key) => key.textContent)).toEqual([
      'Ctrl',
      'K',
      '/',
    ]);
    expect(container.querySelectorAll('[data-slot="keycap"]')).toHaveLength(3);
    expect(screen.getByText('or')).toHaveClass('keycap-join');
    expect(container.querySelector('.sr-only')).toHaveTextContent('+');
  });
  it('renders a plus between sequence keycaps', () => {
    const { container } = render(<Keycaps keys={[['G'], ['2']]} sequence />);
    expect([...container.querySelectorAll('kbd')].map((key) => key.textContent)).toEqual([
      'G',
      '2',
    ]);
    expect(screen.getByText('+')).toHaveClass('keycap-join');
    expect(screen.queryByText('or')).not.toBeInTheDocument();
  });
});
