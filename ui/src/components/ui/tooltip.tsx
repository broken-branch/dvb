import * as React from 'react';
import { cn } from 'cn';
import { Tooltip as TooltipPrimitive } from 'radix-ui';

function TooltipProvider({
  delayDuration = 0,
  skipDelayDuration = 300,
  ...props
}: React.ComponentProps<typeof TooltipPrimitive.Provider>) {
  return (
    <TooltipPrimitive.Provider
      data-slot="tooltip-provider"
      delayDuration={delayDuration}
      skipDelayDuration={skipDelayDuration}
      {...props}
    />
  );
}

function Tooltip({ ...props }: React.ComponentProps<typeof TooltipPrimitive.Root>) {
  return <TooltipPrimitive.Root data-slot="tooltip" {...props} />;
}

function TooltipTrigger({
  onFocus,
  ...props
}: React.ComponentProps<typeof TooltipPrimitive.Trigger>) {
  // Open on focus only for keyboard focus (focus-visible), not when a sheet or dialog
  // focuses its first control on open.
  return (
    <TooltipPrimitive.Trigger
      data-slot="tooltip-trigger"
      onFocus={(event) => {
        onFocus?.(event);
        if (!event.currentTarget.matches(':focus-visible')) event.preventDefault();
      }}
      {...props}
    />
  );
}

function TooltipContent({
  className,
  sideOffset = 6,
  children,
  ...props
}: React.ComponentProps<typeof TooltipPrimitive.Content>) {
  return (
    <TooltipPrimitive.Portal>
      <TooltipPrimitive.Content
        data-slot="tooltip-content"
        sideOffset={sideOffset}
        className={cn('dvb-tooltip', className)}
        {...props}
      >
        {children}
        {/* The caret's top row covers the content border so the border line runs around it. */}
        <TooltipPrimitive.Arrow asChild width={12} height={6}>
          <svg data-slot="tooltip-arrow" viewBox="0 0 12 6">
            <path d="M0 0H12V0.5L6 6L0 0.5Z" fill="var(--popover)" />
            <path d="M0 0.5L6 6L12 0.5" fill="none" stroke="var(--border)" />
          </svg>
        </TooltipPrimitive.Arrow>
      </TooltipPrimitive.Content>
    </TooltipPrimitive.Portal>
  );
}

export { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger };
