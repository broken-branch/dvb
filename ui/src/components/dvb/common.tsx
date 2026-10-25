import type { Tone } from '@/lib/data/tone';
import { useAppReducedMotion } from '@/lib/appearance';
import { Keycaps } from '@/components/ui/keycaps';
import { Badge } from '@/components/ui/badge';
import { useState, useId, type ComponentProps, type ReactNode } from 'react';
import { AnimatePresence, motion, useIsPresent } from 'motion/react';
import { Pencil, Info } from 'lucide-react';
import { Tooltip, TooltipContent, TooltipTrigger } from '@/components/ui/tooltip';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
export function Status({ children, tone = 'neutral' }: { children: ReactNode; tone?: Tone }) {
  return (
    <Badge variant="ghost" className={`status ${tone}`}>
      <svg
        className="status-shape"
        viewBox="0 0 16 16"
        aria-hidden="true"
        focusable="false"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.5"
      >
        {tone === 'good' ? (
          <path d="m3 8 3 3 7-7" />
        ) : tone === 'warn' ? (
          <path d="m8 2 6 6-6 6-6-6Z" />
        ) : tone === 'bad' ? (
          <rect x="3" y="3" width="10" height="10" fill="currentColor" stroke="none" />
        ) : (
          <>
            <circle cx="8" cy="8" r="5" />
            {tone === 'working' && (
              <path d="M8 3a5 5 0 0 0 0 10Z" fill="currentColor" stroke="none" />
            )}
          </>
        )}
      </svg>
      <span className="status-label">{children}</span>
    </Badge>
  );
}
export function Heading({
  title,
  description,
  children,
  level = 1,
}: {
  level?: 1 | 2;
  title: ReactNode;
  description?: string;
  children?: ReactNode;
}) {
  const Title = level === 1 ? 'h1' : 'h2';
  return (
    <header className="page-heading">
      <div>
        <Title tabIndex={-1}>
          {title}
          {description && <InfoTip label="About this page" text={description} />}
        </Title>
      </div>
      {children && <div className="actions">{children}</div>}
    </header>
  );
}
export function SectionTitle({
  title,
  children,
  hint,
}: {
  hint?: string;
  title: string;
  children?: ReactNode;
}) {
  return (
    <div className="section-heading">
      <h2>
        {title}
        {hint && <InfoTip label={title} text={hint} />}
      </h2>
      {children}
    </div>
  );
}
export function Hint({
  label,
  keys,
  sequence = false,
  hideLabel = false,
  side,
  children,
}: {
  label: string;
  keys?: string[][];
  sequence?: boolean;
  hideLabel?: boolean;
  side?: ComponentProps<typeof TooltipContent>['side'];
  children: ReactNode;
}) {
  return (
    <Tooltip>
      <TooltipTrigger asChild>{children}</TooltipTrigger>
      <TooltipContent side={side} className="hint-content">
        {!hideLabel && <span className="hint-label">{label}</span>}
        {keys && <Keycaps keys={keys} sequence={sequence} />}
      </TooltipContent>
    </Tooltip>
  );
}
export function InfoTip({ text, label = 'More information' }: { text: string; label?: string }) {
  const [open, setOpen] = useState(false);
  return (
    <Tooltip open={open} onOpenChange={setOpen}>
      <TooltipTrigger asChild>
        <button
          type="button"
          className="info-tip"
          aria-label={label}
          onClick={(event) => {
            event.preventDefault();
            event.stopPropagation();
            setOpen((value) => !value);
          }}
        >
          <Info size={14} />
        </button>
      </TooltipTrigger>
      <TooltipContent>{text}</TooltipContent>
    </Tooltip>
  );
}
export function ChoiceSelect<T extends string>({
  value,
  onChange,
  options,
  label,
  disabled = false,
}: {
  value: T;
  onChange: (v: T) => void;
  options:
    | readonly T[]
    | {
        value: T;
        label: ReactNode;
      }[];
  label: string;
  disabled?: boolean;
}) {
  return (
    <Select
      disabled={disabled}
      value={value}
      onValueChange={(next) => {
        const option = options.find(
          (option) => (typeof option === 'string' ? option : option.value) === next,
        );
        if (option !== undefined) onChange(typeof option === 'string' ? option : option.value);
      }}
    >
      <SelectTrigger aria-label={label}>
        <SelectValue />
        <Pencil className="select-pencil" size={12} aria-hidden />
      </SelectTrigger>
      <SelectContent>
        {options
          .map((o) => (typeof o === 'string' ? { value: o, label: o } : o))
          .map((o) => (
            <SelectItem key={o.value} value={o.value}>
              {o.label}
            </SelectItem>
          ))}
      </SelectContent>
    </Select>
  );
}
function TabPanel({
  id,
  children,
  reduced,
}: {
  id: string;
  children: ReactNode;
  reduced: boolean | null;
}) {
  const present = useIsPresent();
  return (
    <motion.div
      className="tab-panel"
      inert={!present}
      aria-hidden={!present || undefined}
      initial={{ opacity: reduced ? 1 : 0 }}
      animate={{ opacity: 1 }}
      exit={{ opacity: 0 }}
      transition={{ duration: reduced ? 0 : 0.18 }}
      style={
        !present ? { position: 'absolute', inset: '0 0 auto', pointerEvents: 'none' } : undefined
      }
    >
      <TabsContent forceMount value={id} className="tab-content">
        {children}
      </TabsContent>
    </motion.div>
  );
}
export function PageTabs({
  items,
  value,
  onChange,
}: {
  items: {
    id: string;
    label: string;
    content: ReactNode;
  }[];
  value: string;
  onChange: (v: string) => void;
}) {
  const tabId = useId();
  const reduced = useAppReducedMotion();
  return (
    <Tabs value={value} onValueChange={onChange}>
      <TabsList className="page-tabs">
        {items.map((item) => (
          <TabsTrigger value={item.id} key={item.id} className="page-tab">
            {value === item.id && (
              <motion.span
                layoutId={`tab-${tabId}`}
                className="tab-indicator"
                transition={{ duration: reduced ? 0 : 0.2 }}
              />
            )}
            <span>{item.label}</span>
          </TabsTrigger>
        ))}
      </TabsList>
      <div className="tab-stage">
        <AnimatePresence initial={false} mode="sync">
          {items
            .filter((item) => item.id === value)
            .map((item) => (
              <TabPanel key={item.id} id={item.id} reduced={reduced}>
                {item.content}
              </TabPanel>
            ))}
        </AnimatePresence>
      </div>
    </Tabs>
  );
}
export function Meter({
  value,
  label,
  tone = 'primary',
}: {
  value: number;
  label: string;
  tone?: Tone | 'primary';
}) {
  const reduced = useAppReducedMotion();
  return (
    <div
      className={`meter ${tone}`}
      role="progressbar"
      aria-valuenow={value}
      aria-valuemin={0}
      aria-valuemax={100}
      aria-label={label}
    >
      <motion.div
        initial={{ width: reduced ? `${value}%` : 0 }}
        animate={{ width: `${value}%` }}
        transition={{ duration: reduced ? 0 : 0.38 }}
      />
    </div>
  );
}
