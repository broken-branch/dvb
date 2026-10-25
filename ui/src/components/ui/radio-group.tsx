import * as React from 'react';
import { cn } from 'cn';
import { RadioGroup as RadioGroupPrimitive } from 'radix-ui';

function RadioGroup({
  className,
  ...props
}: React.ComponentProps<typeof RadioGroupPrimitive.Root>) {
  return <RadioGroupPrimitive.Root data-slot="radio-group" className={className} {...props} />;
}

function RadioGroupItem({
  className,
  children,
  asChild,
  ...props
}: React.ComponentProps<typeof RadioGroupPrimitive.Item>) {
  return (
    <RadioGroupPrimitive.Item
      {...(asChild ? {} : { 'data-slot': 'radio-group-item' })}
      asChild={asChild}
      className={asChild ? className : cn('group/radio-group-item peer', className)}
      {...props}
    >
      {asChild ? (
        children
      ) : (
        <RadioGroupPrimitive.Indicator data-slot="radio-group-indicator">
          <span data-slot="radio-group-dot" />
        </RadioGroupPrimitive.Indicator>
      )}
    </RadioGroupPrimitive.Item>
  );
}

export { RadioGroup, RadioGroupItem };
