import { useAppReducedMotion } from '@/lib/appearance';
import * as React from 'react';
import { Switch as SwitchPrimitive } from 'radix-ui';
import { motion } from 'motion/react';
import { Check, Minus } from 'lucide-react';
import { cn } from 'cn';
function Switch({
  className,
  size = 'default',
  checked,
  defaultChecked = false,
  onCheckedChange,
  ...props
}: React.ComponentProps<typeof SwitchPrimitive.Root> & { size?: 'sm' | 'default' }) {
  const [local, setLocal] = React.useState(defaultChecked),
    reduced = useAppReducedMotion();
  const on = checked ?? local;
  return (
    <SwitchPrimitive.Root
      {...props}
      checked={on}
      onCheckedChange={(value) => {
        setLocal(value);
        onCheckedChange?.(value);
      }}
      data-slot="switch"
      data-size={size}
      className={cn('dvb-switch', className)}
    >
      <span className="switch-track" aria-hidden="true" />
      <SwitchPrimitive.Thumb asChild>
        <motion.span
          data-slot="switch-thumb"
          initial={false}
          animate={{ x: on ? (size === 'sm' ? 16 : 20) : 0 }}
          transition={reduced ? { duration: 0 } : { type: 'spring', stiffness: 540, damping: 32 }}
        >
          <Check size={10} className="switch-on" aria-hidden />
          <Minus size={10} className="switch-off" aria-hidden />
        </motion.span>
      </SwitchPrimitive.Thumb>
    </SwitchPrimitive.Root>
  );
}
export { Switch };
