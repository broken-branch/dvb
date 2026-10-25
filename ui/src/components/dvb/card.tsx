import { useAppReducedMotion } from '@/lib/appearance';
import type { ComponentProps, PointerEvent } from 'react';
import { Slot } from 'radix-ui';

import { cn } from 'cn';

/** Every surface uses the same stationary edge and pointer light. asChild keeps
 * native links/buttons, Motion lifecycle, and the screen's layout intact. */
export function Card({
  asChild = false,
  density = 'default',
  className,
  onPointerMove,
  onPointerLeave,
  ...props
}: ComponentProps<'div'> & { asChild?: boolean; density?: 'default' | 'compact' }) {
  const reduced = useAppReducedMotion();
  const Component = asChild ? Slot.Root : 'div';
  function move(event: PointerEvent<HTMLDivElement>) {
    if (!reduced && event.pointerType !== 'touch') {
      const bounds = event.currentTarget.getBoundingClientRect();
      event.currentTarget.style.setProperty('--pointer-x', `${event.clientX - bounds.left}px`);
      event.currentTarget.style.setProperty('--pointer-y', `${event.clientY - bounds.top}px`);
    }
    onPointerMove?.(event);
  }
  return (
    <Component
      {...props}
      data-slot="surface"
      data-density={density}
      className={cn('surface', className)}
      onPointerMove={move}
      onPointerLeave={(event) => {
        event.currentTarget.style.removeProperty('--pointer-x');
        event.currentTarget.style.removeProperty('--pointer-y');
        onPointerLeave?.(event);
      }}
    />
  );
}

export function ListRow({
  asChild = false,
  className,
  ...props
}: ComponentProps<'div'> & { asChild?: boolean }) {
  const Component = asChild ? Slot.Root : 'div';
  return <Component {...props} data-slot="list-row" className={cn('list-row', className)} />;
}
