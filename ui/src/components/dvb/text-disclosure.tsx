import { useLayoutEffect, useRef, useState } from 'react';
import { ChevronDown } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { useAppearance } from '@/lib/appearance';

/** Measure the collapsed rendering, including after font and container changes. */
export function TextDisclosure({
  text,
  as: Tag = 'h2',
  className = '',
}: {
  text: string;
  as?: 'h1' | 'h2';
  className?: string;
}) {
  const ref = useRef<HTMLHeadingElement>(null),
    [overflow, setOverflow] = useState(false),
    [expanded, setExpanded] = useState(false);
  const { textSize, font } = useAppearance();
  useLayoutEffect(() => {
    const element = ref.current;
    if (!element) return;
    const measure = () => {
      element.classList.add('text-clamped');
      const cut = element.scrollHeight > element.clientHeight + 1;
      element.classList.toggle('text-clamped', !expanded);
      setOverflow(cut);
    };
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(element);
    window.addEventListener('resize', measure);
    document.fonts?.addEventListener('loadingdone', measure);
    return () => {
      observer.disconnect();
      window.removeEventListener('resize', measure);
      document.fonts?.removeEventListener('loadingdone', measure);
    };
  }, [text, expanded, textSize, font]);
  return (
    <div className="text-disclosure">
      <Tag
        ref={ref}
        tabIndex={Tag === 'h1' ? -1 : undefined}
        className={`disclosure-text ${expanded ? '' : 'text-clamped'} ${className}`}
      >
        {text}
      </Tag>
      {overflow && (
        <Button variant="ghost" aria-expanded={expanded} onClick={() => setExpanded(!expanded)}>
          {expanded ? 'Show less' : 'Show more'}
          <ChevronDown size={14} className={expanded ? 'expanded' : ''} />
        </Button>
      )}
    </div>
  );
}
