import * as React from 'react';
import { cn } from 'cn';
import { Tabs as TabsPrimitive } from 'radix-ui';

function Tabs({
  className,
  ...props
}: Omit<React.ComponentProps<typeof TabsPrimitive.Root>, 'orientation'>) {
  return (
    <TabsPrimitive.Root
      data-slot="tabs"
      data-orientation="horizontal"
      orientation="horizontal"
      className={cn('group/tabs', className)}
      {...props}
    />
  );
}

function TabsList({ className, ...props }: React.ComponentProps<typeof TabsPrimitive.List>) {
  return (
    <TabsPrimitive.List
      data-slot="tabs-list"
      data-variant="default"
      className={cn('group/tabs-list', className)}
      {...props}
    />
  );
}

function TabsTrigger({ className, ...props }: React.ComponentProps<typeof TabsPrimitive.Trigger>) {
  return <TabsPrimitive.Trigger data-slot="tabs-trigger" className={className} {...props} />;
}

function TabsContent({ className, ...props }: React.ComponentProps<typeof TabsPrimitive.Content>) {
  return <TabsPrimitive.Content data-slot="tabs-content" className={className} {...props} />;
}

export { Tabs, TabsList, TabsTrigger, TabsContent };
