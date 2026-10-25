import { expect, it, describe } from 'vitest';
import { screen, waitFor, within, fireEvent, render } from '@testing-library/react';
import { Hint, PageTabs } from '@/components/dvb/common';
import { mount } from '../support/render';
import { useState } from 'react';
import userEvent from '@testing-library/user-event';
import { AppearanceProvider } from '@/lib/appearance';
import { createFixtureSource } from '../support/render';
import { StoreProvider } from '@/lib/store';
describe('hint', () => {
  it('shows a Hint label and its keycaps without a middot, and a label-only Hint', async () => {
    const { user } = mount(
      <>
        <Hint label="Go anywhere" keys={[['Ctrl', 'K'], ['/']]} side="right">
          <button>Search</button>
        </Hint>
        <Hint label="First-run walkthrough">
          <button>Walkthrough</button>
        </Hint>
      </>,
    );
    await user.hover(screen.getByRole('button', { name: 'Search' }));
    const tip = await screen.findByRole('tooltip');
    expect(within(tip).getByText('Go anywhere')).toBeInTheDocument();
    expect([...tip.querySelectorAll('kbd')].map((key) => key.textContent)).toEqual([
      'Ctrl',
      'K',
      '/',
    ]);
    expect(within(tip).getByText('or')).toBeInTheDocument();
    expect(tip.textContent).not.toContain(' · ');
    await user.unhover(screen.getByRole('button', { name: 'Search' }));
    await user.keyboard('{Escape}');
    await waitFor(() => expect(screen.queryByRole('tooltip')).not.toBeInTheDocument());
    await user.hover(screen.getByRole('button', { name: 'Walkthrough' }));
    const labelOnly = await screen.findByRole('tooltip');
    expect(labelOnly).toHaveTextContent('First-run walkthrough');
    expect(labelOnly.querySelector('kbd')).toBeNull();
  });
  it('hides the label of a shortcut-only Hint and keeps its keycaps', async () => {
    const { user } = mount(
      <Hint label="Usage & models" keys={[['G'], ['3']]} sequence hideLabel side="right">
        <button>Usage</button>
      </Hint>,
    );
    await user.hover(screen.getByRole('button', { name: 'Usage' }));
    const tip = await screen.findByRole('tooltip');
    expect([...tip.querySelectorAll('kbd')].map((key) => key.textContent)).toEqual(['G', '3']);
    expect(tip).not.toHaveTextContent('Usage & models');
    const content = document.querySelector<HTMLElement>("[data-slot='tooltip-content']")!;
    expect(content.querySelector('.hint-label')).toBeNull();
    expect(content.querySelector("[data-slot='tooltip-arrow']")).not.toBeNull();
  });
  it('opens a right-side Hint away from its trigger with an arrow', async () => {
    const { user } = mount(
      <Hint label="Usage & models" keys={[['G'], ['3']]} sequence side="right">
        <button>Usage</button>
      </Hint>,
    );
    await user.hover(screen.getByRole('button', { name: 'Usage' }));
    await screen.findByRole('tooltip');
    const content = document.querySelector<HTMLElement>("[data-slot='tooltip-content']")!;
    expect(content).toHaveAttribute('data-side', 'right');
    expect(content.querySelector("[data-slot='tooltip-arrow']")).not.toBeNull();
    const wrapper = content.closest<HTMLElement>('[data-radix-popper-content-wrapper]')!;
    await waitFor(() => expect(wrapper.style.transform).toMatch(/^translate\((\d+)px, 0px\)$/));
    expect(Number(/^translate\((\d+)px/.exec(wrapper.style.transform)![1])).toBeGreaterThan(0);
  });
});
describe('tabs', () => {
  function TabExample() {
    const [value, setValue] = useState('first');
    return (
      <PageTabs
        value={value}
        onChange={setValue}
        items={[
          { id: 'first', label: 'First', content: <button>First action</button> },
          { id: 'second', label: 'Second', content: <button>Second action</button> },
        ]}
      />
    );
  }
  it.each(['On', 'Off'])(
    'keeps controlled horizontal tabs and accessible panels with motion %s',
    async (motion) => {
      const source = createFixtureSource();
      source.set({ settings: { values: { motion } } });
      render(
        <StoreProvider source={source}>
          <AppearanceProvider>
            <TabExample />
          </AppearanceProvider>
        </StoreProvider>,
      );
      const first = screen.getByRole('tab', { name: 'First' });
      const second = screen.getByRole('tab', { name: 'Second' });
      expect(screen.getByRole('tablist')).toHaveAttribute('aria-orientation', 'horizontal');
      expect(screen.getByRole('tabpanel', { name: 'First' })).toContainElement(
        screen.getByRole('button', { name: 'First action' }),
      );
      const oldPanel = screen.getByRole('tabpanel', { name: 'First' });
      fireEvent.mouseDown(second, { button: 0, ctrlKey: false });
      expect(second).toHaveAttribute('aria-selected', 'true');
      if (motion === 'On') {
        expect(oldPanel).not.toHaveAttribute('hidden');
        expect(oldPanel.parentElement).toHaveAttribute('inert');
        expect(oldPanel.parentElement).toHaveAttribute('aria-hidden', 'true');
      } else {
        expect(document.documentElement).toHaveClass('effects-off');
        expect(
          screen.getByRole('button', { name: 'Second action' }).closest('.tab-panel'),
        ).toHaveStyle({ opacity: '1' });
      }
      expect(screen.getByRole('tabpanel', { name: 'Second' })).toContainElement(
        screen.getByRole('button', { name: 'Second action' }),
      );
      await waitFor(() => expect(oldPanel).not.toBeInTheDocument());
      const user = userEvent.setup();
      second.focus();
      await user.keyboard('{ArrowLeft}');
      await waitFor(() => expect(first).toHaveAttribute('aria-selected', 'true'));
      expect(first).toHaveFocus();
      expect(screen.getByRole('tabpanel', { name: 'First' })).toBeInTheDocument();
    },
  );
});
