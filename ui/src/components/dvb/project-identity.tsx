import { useStore } from '@/lib/store';
import { IdentityMark, type Identity } from './identity-mark';
const suits = ['Spade', 'Heart', 'Diamond', 'Club'] as const;
export const projectIdentity = (index: number): Identity => ({
  suit: suits[Math.max(0, index) % 4],
  colour: ['blue', 'magenta', 'yellow', 'green'][Math.max(0, index) % 4],
});
export function ProjectMark({ id, className = '' }: { id: string; className?: string }) {
  const {
    data: { projects },
  } = useStore();
  const index = projects.findIndex((project) => project.id === id);
  return (
    <span className={`project-suit ${className}`} aria-hidden="true">
      <IdentityMark value={projectIdentity(index)} />
    </span>
  );
}
export function ProjectLabel({
  id,
  name,
  className = '',
}: {
  id: string;
  name?: string;
  className?: string;
}) {
  const {
      data: { projects },
    } = useStore(),
    project = projects.find((p) => p.id === id);
  return (
    <span className={`project-label ${className}`}>
      <ProjectMark id={id} />
      <span>{name || project?.name || id}</span>
    </span>
  );
}
