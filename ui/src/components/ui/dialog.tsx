import { InfoTip } from '@/components/dvb/common';
import * as React from 'react';
import { cn } from 'cn';
import { Dialog as DialogPrimitive } from 'radix-ui';

import { Button } from '@/components/ui/button';
import { XIcon } from 'lucide-react';

function Dialog({ ...props }: React.ComponentProps<typeof DialogPrimitive.Root>) {
  return <DialogPrimitive.Root data-slot="dialog" {...props} />;
}

function DialogPortal({ ...props }: React.ComponentProps<typeof DialogPrimitive.Portal>) {
  return <DialogPrimitive.Portal data-slot="dialog-portal" {...props} />;
}

function DialogOverlay({
  className,
  ...props
}: React.ComponentProps<typeof DialogPrimitive.Overlay>) {
  return (
    <DialogPrimitive.Overlay
      data-slot="dialog-overlay"
      className={cn('fixed inset-0 isolate z-50 duration-100', className)}
      {...props}
    />
  );
}

function DialogContent({
  className,
  children,
  ...props
}: React.ComponentProps<typeof DialogPrimitive.Content>) {
  return (
    <DialogPortal>
      <DialogOverlay />
      <DialogPrimitive.Content
        data-slot="dialog-content"
        className={cn('dialog-panel', className)}
        {...props}
      >
        {children}
      </DialogPrimitive.Content>
    </DialogPortal>
  );
}

function DialogHeader({ className, children, ...props }: React.ComponentProps<'div'>) {
  const nodes = React.Children.toArray(children);
  const title = nodes.find((child) => React.isValidElement(child) && child.type === DialogTitle);
  const description = nodes.find(
    (child) => React.isValidElement(child) && child.type === DialogDescription,
  );
  const help =
    React.isValidElement<{ children?: React.ReactNode }>(description) &&
    typeof description.props.children === 'string'
      ? description.props.children
      : undefined;
  return (
    <div data-slot="dialog-header" className={cn('flex flex-col gap-2', className)} {...props}>
      <div className="overlay-heading">
        <div className="overlay-title-help">
          {title}
          {help && <InfoTip label="About this dialog" text={help} />}
        </div>
        <DialogPrimitive.Close asChild>
          <Button variant="ghost" size="icon-sm" aria-label="Close">
            <XIcon />
          </Button>
        </DialogPrimitive.Close>
      </div>
      {nodes.filter((child) => child !== title)}
    </div>
  );
}

function DialogTitle({ className, ...props }: React.ComponentProps<typeof DialogPrimitive.Title>) {
  return (
    <DialogPrimitive.Title
      data-slot="dialog-title"
      className={cn('font-heading text-base leading-none font-medium', className)}
      {...props}
    />
  );
}

function DialogDescription({
  className,
  ...props
}: React.ComponentProps<typeof DialogPrimitive.Description>) {
  return (
    <DialogPrimitive.Description
      data-slot="dialog-description"
      className={cn(
        'sr-only text-sm text-muted-foreground *:[a]:underline *:[a]:underline-offset-3 *:[a]:hover:text-foreground',
        className,
      )}
      {...props}
    />
  );
}

export { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle };
