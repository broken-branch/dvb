'use client';

import * as React from 'react';
import { Command as CommandPrimitive } from 'cmdk';
import { cn } from 'cn';

import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { SearchIcon, CheckIcon } from 'lucide-react';

function Command({ className, ...props }: React.ComponentProps<typeof CommandPrimitive>) {
  return <CommandPrimitive data-slot="command" className={className} {...props} />;
}

function CommandDialog({
  title = 'Command Palette',
  description = 'Search for a command to run...',
  children,
  className,
  onCloseAutoFocus,
  ...props
}: React.ComponentProps<typeof Dialog> & {
  title?: string;
  description?: string;
  className?: string;
  onCloseAutoFocus?: React.ComponentProps<typeof DialogContent>['onCloseAutoFocus'];
}) {
  return (
    <Dialog {...props}>
      <DialogContent
        className={cn('command-dialog', className)}
        onOpenAutoFocus={(event) => {
          event.preventDefault();
          requestAnimationFrame(() =>
            document.querySelector<HTMLInputElement>('[data-slot=command-input]')?.focus(),
          );
        }}
        onCloseAutoFocus={onCloseAutoFocus}
      >
        <DialogHeader className="command-header">
          <DialogTitle>{title}</DialogTitle>
          <DialogDescription className="sr-only">{description}</DialogDescription>
        </DialogHeader>
        {children}
      </DialogContent>
    </Dialog>
  );
}

function CommandInput({
  className,
  ...props
}: React.ComponentProps<typeof CommandPrimitive.Input>) {
  return (
    <div data-slot="command-input-wrapper">
      <div data-slot="input-group" role="group" className="command-field group/input-group">
        <CommandPrimitive.Input data-slot="command-input" className={className} {...props} />
        <div
          role="group"
          data-slot="input-group-addon"
          data-align="inline-start"

          onClick={(event) => event.currentTarget.parentElement?.querySelector('input')?.focus()}
        >
          <SearchIcon data-slot="command-search-icon" />
        </div>
      </div>
    </div>
  );
}

function CommandList({ className, ...props }: React.ComponentProps<typeof CommandPrimitive.List>) {
  return <CommandPrimitive.List data-slot="command-list" className={className} {...props} />;
}

function CommandEmpty({
  className,
  ...props
}: React.ComponentProps<typeof CommandPrimitive.Empty>) {
  return <CommandPrimitive.Empty data-slot="command-empty" className={className} {...props} />;
}

function CommandGroup({
  className,
  ...props
}: React.ComponentProps<typeof CommandPrimitive.Group>) {
  return <CommandPrimitive.Group data-slot="command-group" className={className} {...props} />;
}

function CommandItem({
  className,
  children,
  ...props
}: React.ComponentProps<typeof CommandPrimitive.Item>) {
  return (
    <CommandPrimitive.Item
      data-slot="command-item"
      className={cn('group/command-item', className)}
      {...props}
    >
      {children}
      <CheckIcon data-slot="command-check-icon" />
    </CommandPrimitive.Item>
  );
}

function CommandShortcut({ className, ...props }: React.ComponentProps<'span'>) {
  return <span data-slot="command-shortcut" className={className} {...props} />;
}

export {
  Command,
  CommandDialog,
  CommandInput,
  CommandList,
  CommandEmpty,
  CommandGroup,
  CommandItem,
  CommandShortcut,
};
