import * as React from 'react';
import { Slot } from 'radix-ui';

type BadgeVariant = 'default' | 'ghost';

function Badge({
  className,
  variant = 'default',
  asChild = false,
  ...props
}: React.ComponentProps<'span'> & { variant?: BadgeVariant; asChild?: boolean }) {
  const Comp = asChild ? Slot.Root : 'span';

  return <Comp data-slot="badge" data-variant={variant} className={className} {...props} />;
}

export { Badge };
