import { describe, expect, it } from 'vitest';
import { render, screen, waitFor, within } from '@testing-library/react';
import userEvent, { PointerEventsCheckLevel } from '@testing-library/user-event';
import { Dialog as DialogPrimitive } from 'radix-ui';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import {
  Sheet,
  SheetContent,
  SheetDescription,
  SheetHeader,
  SheetTitle,
} from '@/components/ui/sheet';
import { TooltipProvider } from '@/components/ui/tooltip';
describe('accessibility', () => {
  describe.each(['dialog', 'sheet'] as const)('%s accessibility', (kind) => {
    it.each(['header close', 'Escape'] as const)(
      'keeps portal associations, trapped focus and restoration after %s',
      async (dismiss) => {
        const Root = kind === 'dialog' ? Dialog : Sheet;
        const Content = kind === 'dialog' ? DialogContent : SheetContent;
        const Header = kind === 'dialog' ? DialogHeader : SheetHeader;
        const Title = kind === 'dialog' ? DialogTitle : SheetTitle;
        const Description = kind === 'dialog' ? DialogDescription : SheetDescription;
        const user = userEvent.setup({ pointerEventsCheck: PointerEventsCheckLevel.Never });
        const { container } = render(
          <TooltipProvider>
            <Root>
              <DialogPrimitive.Trigger>Open details</DialogPrimitive.Trigger>
              <Content>
                <Header>
                  <Title>Details</Title>
                  <Description>Details help.</Description>
                </Header>
                <input aria-label="Detail" />
              </Content>
            </Root>
          </TooltipProvider>,
        );
        const trigger = screen.getByRole('button', { name: 'Open details' });
        await user.click(trigger);
        const dialog = screen.getByRole('dialog', { name: 'Details' });
        expect(container).not.toContainElement(dialog);
        expect(dialog).toHaveAccessibleDescription('Details help.');
        await user.tab();
        expect(dialog).toContainElement(document.activeElement as HTMLElement);
        await user.tab({ shift: true });
        expect(dialog).toContainElement(document.activeElement as HTMLElement);
        if (dismiss === 'header close') {
          await user.click(within(dialog).getByRole('button', { name: 'Close', exact: true }));
        } else {
          await user.click(within(dialog).getByLabelText('Detail'));
          await user.keyboard('{Escape}');
        }
        await waitFor(() => expect(trigger).toHaveFocus());
        expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
      },
    );
  });
});
