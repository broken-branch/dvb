import { Rail, CommandMenu, KeyboardGuide, useShortcuts } from '@/components/dvb/workspace';
import { parseRoute } from '@/lib/routes';
import { useAppReducedMotion } from '@/lib/appearance';
import { useEffect, useRef, useState } from 'react';
import { AnimatePresence, LayoutGroup, motion } from 'motion/react';
import { PanelLeftOpen } from 'lucide-react';
import { Toaster } from 'sonner';
import { SidebarProvider, useSidebar } from '@/components/ui/sidebar';
import { TooltipProvider } from '@/components/ui/tooltip';
import { Button } from '@/components/ui/button';
import type { DataSource } from '@/lib/data/source';
import { StoreProvider, useStore, go } from '@/lib/store';
import { AppearanceProvider } from '@/lib/appearance';
import { WorkspaceInfo } from '@/components/dvb/workspace-info';
import { Hint } from '@/components/dvb/common';
import { Home, Projects } from '@/screens/home';
import { Decision } from '@/screens/decision';
import { DataError, useAction } from '@/components/dvb/data-feedback';
import { ProjectPage, AddProject } from '@/screens/project';
import { TaskPage } from '@/screens/task';
import { Usage } from '@/screens/usage';
import { Company } from '@/screens/company';
import { Settings } from '@/screens/settings';
import { Setup } from '@/screens/setup';
import { ReportSheet } from '@/components/dvb/report-sheet';
import type { ReportKind } from '@/lib/data/types';
import { connectDeskNavigation } from '@/lib/desk-navigation';
function Workspace() {
  const s = useStore(),
    action = useAction(),
    side = useSidebar(),
    reduced = useAppReducedMotion();
  const [route, setRoute] = useState(location.hash.slice(1) || 'home'),
    [command, setCommand] = useState(false),
    [guide, setGuide] = useState(false),
    [report, setReport] = useState<ReportKind | null>(null);
  const main = useRef<HTMLElement>(null);
  const parsed = parseRoute(route),
    page = parsed.page;
  const arg =
    'id' in parsed
      ? parsed.id || ''
      : parsed.page === 'company'
        ? parsed.profile || ''
        : parsed.page === 'settings'
          ? parsed.tab || ''
          : '';
  const tab = 'tab' in parsed ? parsed.tab || '' : '';
  const projectId = parsed.page === 'company' ? parsed.project || '' : '';
  const currentProject = page === 'project' ? s.data.projects.find((p) => p.id === arg) : undefined;
  const currentTask = page === 'task' ? s.data.tasks.find((t) => t.id === arg) : undefined;
  const taskControl =
    currentTask && !['done', 'cancelled', 'failed'].includes(currentTask.state.state);
  const active =
    page === 'decision' ? 'home' : page === 'project' || page === 'task' ? 'projects' : page;
  useEffect(() => {
    const change = () => {
      setRoute(location.hash.slice(1) || 'home');
      side.setOpenMobile(false);
    };
    window.addEventListener('hashchange', change);
    return () => window.removeEventListener('hashchange', change);
  }, [side.setOpenMobile]);
  useEffect(() => connectDeskNavigation(go), []);
  const screenKey = `${page}/${page === 'company' || page === 'settings' ? '' : arg}`;
  useEffect(() => {
    window.scrollTo({ top: 0 });
  }, [screenKey]);
  useEffect(() => {
    document.title = `dvb / ${page === 'home' ? 'Now' : page === 'projects' ? 'Overview' : page.charAt(0).toUpperCase() + page.slice(1)}`;
    const timer = setTimeout(
      () =>
        !document.querySelector('[role="dialog"]') &&
        main.current?.querySelector<HTMLHeadingElement>('h1')?.focus({ preventScroll: true }),
      350,
    );
    // A key or pointer press before then keeps the focus the owner chose, including focus a
    // closing dialog returned to its opener.
    const keep = () => clearTimeout(timer);
    window.addEventListener('keydown', keep, true);
    window.addEventListener('pointerdown', keep, true);
    return () => {
      clearTimeout(timer);
      window.removeEventListener('keydown', keep, true);
      window.removeEventListener('pointerdown', keep, true);
    };
  }, [screenKey, page]);
  useShortcuts({
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
  });
  let content;
  switch (page) {
    case 'home':
      content = <Home />;
      break;
    case 'projects':
      content = <Projects />;
      break;
    case 'decision':
      content = <Decision id={arg} from={parsed.page === 'decision' ? parsed.from : undefined} />;
      break;
    case 'project':
      content =
        arg === 'new' ? <AddProject /> : <ProjectPage key={arg} id={arg} tab={tab || undefined} />;
      break;
    case 'task':
      content = <TaskPage key={arg} id={arg} tab={tab || undefined} />;
      break;
    case 'usage':
      content = <Usage />;
      break;
    case 'company':
      content = <Company profileId={arg} projectId={projectId} />;
      break;
    case 'settings':
      content = <Settings tab={arg || undefined} />;
      break;
    case 'setup':
      content = <Setup />;
      break;
    default:
      content = <Home />;
  }
  const navigate = (path: string) => {
    setCommand(false);
    go(path);
  };
  return (
    <>
      <a
        className="skip-link"
        href="#workspace"
        onClick={(e) => {
          e.preventDefault();
          main.current?.focus();
        }}
      >
        Skip to workspace
      </a>
      <Rail active={active} setCommand={setCommand} setGuide={setGuide} />
      <main className="workspace" id="workspace" ref={main} tabIndex={-1}>
        <div className="usage-dock">
          <Hint label="Open navigation" keys={[['[']]} side="right">
            <Button
              variant="ghost"
              className="mobile-nav-trigger"
              size="icon"
              onClick={side.toggleSidebar}
              aria-label="Open navigation"
            >
              <PanelLeftOpen size={18} />
            </Button>
          </Hint>
          <WorkspaceInfo />
        </div>
        <LayoutGroup>
          <AnimatePresence mode="popLayout" initial={false}>
            <motion.div
              key={screenKey}
              className="page"
              initial={{ opacity: reduced ? 1 : 0 }}
              animate={{ opacity: 1 }}
              exit={{ opacity: 0 }}
              transition={{ duration: reduced ? 0 : 0.18 }}
            >
              {s.data.connection.state === 'reconnecting' ? (
                <div className="notice" role="status">
                  <strong>dvb's engine isn't running. Reconnecting…</strong>
                  <p>Displayed work is the last known state.</p>
                  {(action.error || s.issue) && <small>{action.error || s.issue}</small>}
                </div>
              ) : (
                <DataError message={action.error || s.issue} />
              )}
              {s.data.connection.state === 'version-mismatch' && (
                <div className="notice" role="alert">
                  Update dvb before making changes. App {s.data.connection.appVersion}; service{' '}
                  {s.data.connection.engineVersion}. Your displayed work is still available.
                </div>
              )}
              {content}
            </motion.div>
          </AnimatePresence>
        </LayoutGroup>
      </main>
      <CommandMenu
        command={command}
        setCommand={setCommand}
        navigate={navigate}
        arg={arg}
        currentProject={currentProject}
        currentTask={currentTask}
        taskControl={taskControl}
        action={action}
        setGuide={setGuide}
        setReport={setReport}
      />
      <ReportSheet kind={report} onClose={() => setReport(null)} />
      <KeyboardGuide guide={guide} setGuide={setGuide} />
      <Toaster
        theme="system"
        position="bottom-right"
        toastOptions={{
          style: {
            background: 'var(--popover)',
            color: 'var(--popover-foreground)',
            border: '1px solid var(--border)',
            borderRadius: 0,
            fontFamily: 'var(--app-font)',
          },
        }}
      />
    </>
  );
}
export default function App({ source }: { source?: DataSource } = {}) {
  return (
    <StoreProvider source={source}>
      <AppearanceProvider>
        <TooltipProvider delayDuration={350}>
          <SidebarProvider>
            <Workspace />
          </SidebarProvider>
        </TooltipProvider>
      </AppearanceProvider>
    </StoreProvider>
  );
}
