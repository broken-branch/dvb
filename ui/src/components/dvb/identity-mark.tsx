import {
  Compass,
  DraftingCompass,
  PanelsTopLeft,
  Braces,
  ScanEye,
  NotebookPen,
  Accessibility,
  type LucideIcon,
} from 'lucide-react';

type Suit = 'Spade' | 'Heart' | 'Diamond' | 'Club';
export type Identity = { colour: string } & (
  { icon: string; suit?: never } | { suit: Suit; icon?: never }
);
const roleIcons: Record<string, LucideIcon> = {
  Compass,
  DraftingCompass,
  PanelsTopLeft,
  Braces,
  ScanEye,
  NotebookPen,
  Accessibility,
};
const suits: Record<Suit, string> = { Spade: '♠', Heart: '♥', Diamond: '♦', Club: '♣' };
export function IdentityMark({ value, size = 22 }: { value: Identity; size?: number }) {
  const Icon = roleIcons[value.icon || 'Compass'] || Compass;
  return (
    <span
      className="identity-mark"
      style={{ color: `var(--identity-${value.colour})`, width: size, height: size }}
      aria-hidden="true"
    >
      {value.suit ? (
        <span style={{ fontSize: size * 1.1 }}>{suits[value.suit]}</span>
      ) : (
        <Icon size={size} strokeWidth={1.7} />
      )}
    </span>
  );
}
