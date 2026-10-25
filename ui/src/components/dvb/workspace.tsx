import { toggleWork } from './data-feedback';
import { routes } from '@/lib/routes';
import { useAppReducedMotion } from '@/lib/appearance';
import { useEffect, useRef, useState } from 'react';
import { motion } from 'motion/react';
import {
  Home as HomeIcon,
  Layers,
  ChartNoAxesCombined,
  Users,
  Settings as SettingsIcon,
  Search,
  PanelLeftClose,
  PanelLeftOpen,
  Keyboard,
  CircleHelp,
  ArrowUpRight,
} from 'lucide-react';
import {
  Sidebar,
  SidebarHeader,
  SidebarContent,
  SidebarFooter,
  useSidebar,
} from '@/components/ui/sidebar';
import { Keycaps } from '@/components/ui/keycaps';
import { Button } from '@/components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogDescription,
} from '@/components/ui/dialog';
import {
  Command,
  CommandDialog,
  CommandInput,
  CommandList,
  CommandEmpty,
  CommandGroup,
  CommandItem,
  CommandShortcut,
} from '@/components/ui/command';
import { useStore, go } from '@/lib/store';
import { Logo } from '@/components/dvb/logo';
import { ProjectLabel, ProjectMark } from '@/components/dvb/project-identity';
import { Hint, InfoTip } from '@/components/dvb/common';
import { railGroups, railStatus, activeTask, needs, pageQuestion } from '@/lib/data/work';
import { useWorkClock } from '@/lib/data/use-work-clock';
import { useAction } from '@/components/dvb/data-feedback';
import type { ReportKind } from '@/lib/data/types';
const nav = [
  { path: routes.home(), label: 'Now', Icon: HomeIcon },
  { path: routes.projects(), label: 'Overview', Icon: Layers },
  { path: routes.usage(), label: 'Usage & models', Icon: ChartNoAxesCombined },
  { path: routes.company(), label: 'Company', Icon: Users },
  { path: routes.settings(), label: 'Settings', Icon: SettingsIcon },
];
/** Radix refocuses only its own trigger; Go anywhere, the guide and the Report sheet open from shortcuts and plain buttons. */
let opener: HTMLElement | null = null,
  inner: HTMLElement | null = null;
function rememberOpener() {
  const el = document.activeElement;
  // Go anywhere opening the guide keeps the element focused before Go anywhere.
  if (el instanceof HTMLElement && el.closest('.command-dialog')) return;
  // Ctrl+K inside the guide or the Report sheet: that dialog's opener stays remembered.
  if (el instanceof HTMLElement && el.closest('.keyboard-dialog,.report-sheet')) {
    inner = el;
    return;
  }
  opener = el instanceof HTMLElement && el !== document.body ? el : null;
  inner = null;
}
export function returnFocus(e: Event) {
  e.preventDefault();
  const other = document.querySelector('.keyboard-dialog,.report-sheet');
  // Go anywhere closing over the guide or the Report sheet refocuses the element it was opened
  // from there, if any; that dialog returns focus to its opener when it closes.
  if (other) {
    if (inner && other.contains(inner)) inner.focus();
    return;
  }
  (opener?.isConnected ? opener : document.getElementById('workspace'))?.focus();
}
const shortcuts: { keys: string[][]; sequence?: boolean; action: string }[] = [
  {
    keys: [['J'], ['K']],
    action: 'Select the next / previous Needs you item; J on an answered question opens the next',
  },
  { keys: [['Enter'], ['O']], action: 'Open the selected item' },
  {
    keys: [['A']],
    action: "Open this task's or project's question; elsewhere the oldest open question",
  },
  {
    keys: [['1–9']],
    action: 'Select a decision answer; the number after the last answer writes a reply',
  },
  { keys: [['Ctrl', 'Enter']], action: 'Send the answer' },
  { keys: [['P']], action: 'Pause / resume the current project' },
  { keys: [['X']], action: 'Stop / resume the current task' },
  { keys: [['G'], ['1–5']], sequence: true, action: nav.map((item) => item.label).join(', ') },
  { keys: [['Ctrl', 'K'], ['/']], action: 'Go anywhere' },
  { keys: [['[']], action: 'Collapse / expand navigation' },
  { keys: [['?']], action: 'Open this keyboard guide' },
  { keys: [['Esc']], action: 'Close the current dialog, menu or drawer' },
];
type Controls = {
  s: ReturnType<typeof useStore>;
  side: ReturnType<typeof useSidebar>;
  page: string;
  arg: string;
  taskControl: boolean | undefined;
  currentTask: ReturnType<typeof useStore>['data']['tasks'][number] | undefined;
  currentProject: ReturnType<typeof useStore>['data']['projects'][number] | undefined;
  action: ReturnType<typeof useAction>;
  setCommand: React.Dispatch<React.SetStateAction<boolean>>;
  setGuide: React.Dispatch<React.SetStateAction<boolean>>;
};
export function useShortcuts({
  s,
  side,
  page,
  arg,
  taskControl,
  currentTask,
  currentProject,
  action,
  setCommand,
  setGuide,
}: Controls) {
  const goPrefix = useRef(0);
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.defaultPrevented || e.isComposing || e.metaKey) return;
      const target = e.target as HTMLElement,
        editing = !!target.closest('input,textarea,select,[contenteditable=true]'),
        overlay = !!document.querySelector(
          '[role="dialog"],[role="menu"],[role="listbox"],[data-slot="popover-content"]',
        );
      if (e.ctrlKey && !e.shiftKey && !e.altKey && e.key.toLowerCase() === 'k') {
        e.preventDefault();
        rememberOpener();
        setCommand((v) => !v);
        return;
      }
      if (
        e.ctrlKey &&
        !e.shiftKey &&
        !e.altKey &&
        e.key === 'Enter' &&
        page === 'decision' &&
        !overlay
      ) {
        e.preventDefault();
        document.querySelector<HTMLFormElement>('#decision-form')?.requestSubmit();
        return;
      }
      if (editing || overlay || e.ctrlKey || e.metaKey || e.altKey) return;
      const key = e.key.toLowerCase();
      if (e.key === '/') {
        e.preventDefault();
        rememberOpener();
        setCommand(true);
        return;
      }
      if (key === 'g') {
        goPrefix.current = Date.now();
        return;
      }
      if (goPrefix.current && Date.now() - goPrefix.current < 1500 && /^[1-5]$/.test(key)) {
        e.preventDefault();
        go(nav[Number(key) - 1].path);
        goPrefix.current = 0;
        return;
      }
      goPrefix.current = 0;
      if (key === '?') {
        e.preventDefault();
        rememberOpener();
        setGuide(true);
      } else if (key === '[') {
        e.preventDefault();
        side.toggleSidebar();
      } else if (key === 'a') {
        e.preventDefault();
        const q = pageQuestion(
          s.data.questions,
          page === 'project' ? arg : currentTask?.project,
          currentTask?.id,
        );
        if (q)
          go(
            routes.decision(
              q.id,
              page === 'project' || page === 'task' ? { page, id: arg } : undefined,
            ),
          );
      } else if (page === 'decision' && key === 'j') {
        e.preventDefault();
        document.querySelector<HTMLButtonElement>('#next-question')?.click();
      } else if (page === 'decision' && /^[1-9]$/.test(key)) {
        e.preventDefault();
        // The answers in page order, with Write a reply last: key n picks the nth.
        document
          .querySelectorAll<HTMLButtonElement>('#decision-form [role="radio"]')
          [Number(key) - 1]?.click();
      } else if (key === 'p' && currentProject) {
        e.preventDefault();
        const id = arg;
        if (id) void action.act(() => toggleWork(s.data, currentProject!));
      } else if (key === 'x' && taskControl) {
        e.preventDefault();
        void action.act(() => toggleWork(s.data, currentTask!));
      } else if (page === 'home' && (key === 'j' || key === 'k')) {
        e.preventDefault();
        const list = Array.from(document.querySelectorAll<HTMLAnchorElement>('[data-needs]'));
        const i = list.indexOf(document.activeElement as HTMLAnchorElement);
        if (list.length) {
          const selected =
            i < 0
              ? key === 'j'
                ? 0
                : list.length - 1
              : (i + (key === 'j' ? 1 : -1) + list.length) % list.length;
          list[selected].focus();
        }
      } else if (
        page === 'home' &&
        (key === 'enter' || key === 'o') &&
        (target === document.body || target.matches('h1') || target.matches('[data-needs]'))
      ) {
        e.preventDefault();
        (target.matches('[data-needs]')
          ? target
          : document.querySelector<HTMLAnchorElement>('[data-needs]')
        )?.click();
      }
    };
    window.addEventListener('keydown', handler);
    return () => window.removeEventListener('keydown', handler);
  }, [s, side, page, arg, taskControl, currentTask, currentProject, action, setCommand, setGuide]);
}
export function Rail({
  active,
  setCommand,
  setGuide,
}: Pick<Controls, 'setCommand' | 'setGuide'> & { active: string }) {
  const s = useStore(),
    side = useSidebar(),
    reduced = useAppReducedMotion(),
    now = useWorkClock();
  const [quietOpen, setQuietOpen] = useState(false),
    working = s.data.status.running > 0;
  return (
    <Sidebar className="app-sidebar">
      <SidebarHeader className="rail-header">
        <a href={`#${routes.home()}`} className="logo-link" aria-label="dvb home">
          <Logo active={working} attention={!!s.data.status.needs_you} />
        </a>
      </SidebarHeader>
      <SidebarContent className="rail-content">
        <Hint label="Go anywhere" keys={[['Ctrl', 'K'], ['/']]} side="right">
          <Button
            variant="outline"
            className="rail-search"
            onClick={() => {
              rememberOpener();
              setCommand(true);
            }}
          >
            <Search size={17} />
            <span className="rail-label">Go anywhere</span>
            <span className="search-shortcut">
              <Keycaps keys={[['Ctrl', 'K']]} />
            </span>
          </Button>
        </Hint>
        <nav className="rail-nav" aria-label="Main navigation">
          {nav.map(({ path, label, Icon }, i) => (
            <Hint
              label={label}
              keys={[['G'], [String(i + 1)]]}
              sequence
              hideLabel={side.open}
              side="right"
              key={path}
            >
              <a
                href={`#${path}`}
                aria-current={active === path ? 'page' : undefined}
                className={`nav-item ${active === path ? 'active' : ''}`}
              >
                {active === path && (
                  <motion.span
                    layoutId={reduced ? undefined : 'nav-active'}
                    className="nav-active"
                    transition={{
                      type: 'spring',
                      stiffness: 420,
                      damping: 40,
                      duration: reduced ? 0 : 0.25,
                    }}
                  />
                )}
                <Icon size={19} />
                <span className="rail-label">{label}</span>
                {i === 0 && (
                  <span className={`nav-count ${s.data.status.needs_you ? 'attention-count' : ''}`}>
                    {s.data.status.needs_you || '✓'}
                  </span>
                )}
              </a>
            </Hint>
          ))}
        </nav>
        <div className="rail-projects">
          {railGroups(s.data, now).map((group) => (
            <section key={group.label}>
              {group.label === 'Quiet' ? (
                <button
                  className="rail-section-label quiet-toggle"
                  aria-expanded={quietOpen}
                  aria-label={`Quiet (${group.projects.length})`}
                  onClick={() => setQuietOpen((v) => !v)}
                >
                  <span className="rail-label">Quiet ({group.projects.length})</span>
                  <span aria-hidden>{quietOpen ? '−' : '+'}</span>
                </button>
              ) : (
                <span className="rail-section-label rail-label">{group.label}</span>
              )}
              {(group.label !== 'Quiet' || quietOpen ? group.projects : []).map((p) => (
                <a
                  href={`#${routes.project(p.id)}`}
                  key={p.id}
                  aria-label={`${p.name}: ${railStatus(s.data, p, now)}`}
                  title={`${p.name}: ${railStatus(s.data, p, now)}`}
                >
                  <span className="rail-project-mark">
                    <ProjectMark id={p.id} />
                    {activeTask(s.data, p) && (
                      <i className="project-pulse" role="img" aria-label="Working" />
                    )}
                    {needs(s.data, p) > 0 && <b className="project-needs">{needs(s.data, p)}</b>}
                  </span>
                  <span className="rail-label">
                    {p.name}
                    <small>{railStatus(s.data, p, now)}</small>
                  </span>
                </a>
              ))}
            </section>
          ))}
        </div>
      </SidebarContent>
      <SidebarFooter className="rail-footer">
        <div className="rail-bottom">
          <Hint label="Keyboard guide" keys={[['?']]} side="right">
            <Button
              variant="ghost"
              size="icon"
              onClick={() => {
                rememberOpener();
                setGuide(true);
              }}
              aria-label="Keyboard guide"
            >
              <Keyboard size={18} />
            </Button>
          </Hint>
          <Hint label="First-run walkthrough" side="right">
            <Button
              variant="ghost"
              size="icon"
              onClick={() => go(routes.setup())}
              aria-label="First-run walkthrough"
            >
              <CircleHelp size={18} />
            </Button>
          </Hint>
          <Hint
            label={side.open ? 'Collapse navigation' : 'Expand navigation'}
            keys={[['[']]}
            side="right"
          >
            <Button
              variant="ghost"
              size="icon"
              onClick={side.toggleSidebar}
              aria-label={side.open ? 'Collapse navigation' : 'Expand navigation'}
            >
              {side.open ? <PanelLeftClose size={18} /> : <PanelLeftOpen size={18} />}
            </Button>
          </Hint>
        </div>
      </SidebarFooter>
    </Sidebar>
  );
}
export function CommandMenu({
  command,
  setCommand,
  navigate,
  arg,
  currentProject,
  currentTask,
  taskControl,
  action,
  setGuide,
  setReport,
}: Pick<
  Controls,
  'setCommand' | 'arg' | 'currentProject' | 'currentTask' | 'taskControl' | 'action' | 'setGuide'
> & {
  command: boolean;
  navigate(path: string): void;
  setReport: React.Dispatch<React.SetStateAction<ReportKind | null>>;
}) {
  const s = useStore(),
    side = useSidebar();
  return (
    <CommandDialog
      open={command}
      onOpenChange={setCommand}
      onCloseAutoFocus={returnFocus}
      title="Go anywhere"
      description="Search screens and actions. Keyboard shortcuts are listed beside each command."
    >
      <Command>
        <CommandInput placeholder="Where do you want to go?" />
        <CommandList>
          <CommandEmpty>No matching command.</CommandEmpty>
          <CommandGroup heading="Workspace">
            {nav.map((n, i) => (
              <CommandItem key={n.path} onSelect={() => navigate(n.path)}>
                <n.Icon size={17} />
                {n.label}
                <CommandShortcut>
                  <Keycaps keys={[['G'], [String(i + 1)]]} sequence />
                </CommandShortcut>
              </CommandItem>
            ))}
          </CommandGroup>
          <CommandGroup heading="Projects">
            {s.data.projects.map((project) => (
              <CommandItem key={project.id} onSelect={() => navigate(routes.project(project.id))}>
                <ProjectLabel id={project.id} />
                <CommandShortcut>Open project</CommandShortcut>
              </CommandItem>
            ))}
          </CommandGroup>
          <CommandGroup heading="Work">
            {s.data.questions
              .filter((q) => q.open)
              .map((q) => (
                <CommandItem key={q.id} onSelect={() => navigate(routes.decision(q.id))}>
                  <span className="written-subject">{q.subject}</span>
                </CommandItem>
              ))}
            {s.data.tasks.map((t) => (
              <CommandItem key={t.id} onSelect={() => navigate(routes.task(t.id))}>
                <span className="task-written-text">{t.title}</span>
              </CommandItem>
            ))}
            {currentProject && (
              <CommandItem
                onSelect={() => {
                  setCommand(false);
                  const id = arg;
                  if (id) void action.act(() => toggleWork(s.data, currentProject!));
                }}
              >
                Pause / resume project
                <CommandShortcut>
                  <Keycaps keys={[['P']]} />
                </CommandShortcut>
              </CommandItem>
            )}
            {taskControl && (
              <CommandItem
                onSelect={() => {
                  setCommand(false);
                  void action.act(() => toggleWork(s.data, currentTask!));
                }}
              >
                Stop / resume task
                <CommandShortcut>
                  <Keycaps keys={[['X']]} />
                </CommandShortcut>
              </CommandItem>
            )}
            <CommandItem onSelect={() => navigate(routes.project('new'))}>
              Add a project
            </CommandItem>
          </CommandGroup>
          <CommandGroup heading="Help">
            <CommandItem onSelect={() => navigate(routes.setup())}>
              First-run walkthrough
              <ArrowUpRight size={15} />
            </CommandItem>
            <CommandItem
              onSelect={() => {
                setCommand(false);
                setReport('bug');
              }}
            >
              Report a bug
            </CommandItem>
            <CommandItem
              onSelect={() => {
                setCommand(false);
                setReport('feature');
              }}
            >
              Suggest a feature
            </CommandItem>
            <CommandItem
              onSelect={() => {
                setCommand(false);
                setGuide(true);
              }}
            >
              Keyboard guide
              <CommandShortcut>
                <Keycaps keys={[['?']]} />
              </CommandShortcut>
            </CommandItem>
            <CommandItem
              onSelect={() => {
                setCommand(false);
                side.toggleSidebar();
              }}
            >
              Collapse / expand navigation
              <CommandShortcut>
                <Keycaps keys={[['[']]} />
              </CommandShortcut>
            </CommandItem>
          </CommandGroup>
        </CommandList>
      </Command>
    </CommandDialog>
  );
}
export function KeyboardGuide({
  guide,
  setGuide,
}: Pick<Controls, 'setGuide'> & { guide: boolean }) {
  return (
    <Dialog open={guide} onOpenChange={setGuide}>
      <DialogContent
        className="keyboard-dialog"
        onOpenAutoFocus={(e) => {
          // The first tabbable control is an InfoTip, whose tooltip would take the first Escape.
          e.preventDefault();
          (e.currentTarget as HTMLElement).focus();
        }}
        onCloseAutoFocus={returnFocus}
      >
        <DialogHeader>
          <DialogTitle>Keep your hands on the keyboard.</DialogTitle>
          <DialogDescription>
            Super belongs to Omarchy. Shortcuts use Ctrl, Shift, Alt or single keys. Single-key
            actions pause while typing; Ctrl K opens search and Ctrl Enter sends a decision answer.
            Browser shortcuts stay available.
          </DialogDescription>
        </DialogHeader>
        <div className="actions">
          <span>Drag controls</span>
          <InfoTip
            label="Keyboard dragging"
            text="Org chart: Space picks up a card, arrows choose a target, Space drops, Escape cancels. Drop on your lead to swap."
          />
        </div>
        <dl className="keyboard-list">
          {shortcuts.map(({ keys, sequence, action }) => (
            <div key={action}>
              <dt>
                <Keycaps keys={keys} sequence={sequence} />
              </dt>
              <dd>{action}</dd>
            </div>
          ))}
        </dl>
      </DialogContent>
    </Dialog>
  );
}
