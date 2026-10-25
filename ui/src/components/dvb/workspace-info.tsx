import { routes } from '@/lib/routes';
import { useStore } from '@/lib/store';
import { Status } from './common';
import { Button } from '@/components/ui/button';
export function WorkspaceInfo() {
  const { data: s, canWrite } = useStore();
  return (
    <div className="info-bar" role="region" aria-label="Live workspace information">
      <Button asChild variant="ghost" size="xs" className="info-chip">
        <a href={`#${routes.home()}`}>
          {s.status.needs_you} {s.status.needs_you === 1 ? 'needs' : 'need'} you
        </a>
      </Button>
      <Button asChild variant="ghost" size="xs" className="info-chip">
        <a href={`#${routes.projects()}`}>{s.status.running} active</a>
      </Button>
      <Button asChild variant="ghost" size="xs" className="info-chip">
        <a href={`#${routes.usage()}`}>Usage & models</a>
      </Button>
      <Status tone={canWrite ? 'good' : 'warn'}>{canWrite ? 'Live' : 'Last known state'}</Status>
    </div>
  );
}
