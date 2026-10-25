import * as React from 'react';
import { cn } from 'cn';
import { Slot } from 'radix-ui';

type ButtonVariant = 'default' | 'outline' | 'ghost';
type ButtonSize = 'default' | 'xs' | 'sm' | 'icon' | 'icon-sm';

function Button({
  className,
  variant = 'default',
  size = 'default',
  asChild = false,
  ...props
}: React.ComponentProps<'button'> & {
  variant?: ButtonVariant;
  size?: ButtonSize;
  asChild?: boolean;
}) {
  const Comp = asChild ? Slot.Root : 'button';

  return (
    <Comp
      data-slot="button"
      data-variant={variant}
      data-size={size}
      className={cn('dvb-button', className)}
      {...props}
    />
  );
}

export { Button };
