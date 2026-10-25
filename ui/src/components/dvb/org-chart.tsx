import { routes } from '@/lib/routes';
import { useLayoutEffect, useRef, useState, type ReactNode } from 'react';
import {
  DndContext,
  KeyboardSensor,
  PointerSensor,
  closestCenter,
  pointerWithin,
  DragOverlay,
  type KeyboardCoordinateGetter,
  useDraggable,
  useDroppable,
  useSensor,
  useSensors,
} from '@dnd-kit/core';
import { Card } from './card';
import { Status, InfoTip } from './common';
import { DataError, useAction } from './data-feedback';
import { IdentityMark } from './identity-mark';
import { useStore, go } from '@/lib/store';
import { stateLabels } from '@/lib/data/display';
import type { ChartMove, Role } from '@/lib/data/types';
const keyboardCoordinates: KeyboardCoordinateGetter = (event, { context, currentCoordinates }) => {
  if (!['ArrowDown', 'ArrowUp', 'ArrowLeft', 'ArrowRight'].includes(event.code)) return;
  event.preventDefault();
  const targets = context.droppableContainers.getEnabled().filter((target) => target.rect.current);
  const index = targets.findIndex((target) => target.id === context.over?.id);
  const direction = ['ArrowDown', 'ArrowRight'].includes(event.code) ? 1 : -1;
  const target =
    targets[
      index < 0
        ? direction > 0
          ? 0
          : targets.length - 1
        : (index + direction + targets.length) % targets.length
    ];
  const rect = target?.rect.current,
    active = context.collisionRect;
  if (!rect || !active) return;
  return {
    x: currentCoordinates.x + rect.left + rect.width / 2 - active.left - active.width / 2,
    y: currentCoordinates.y + rect.top + rect.height / 2 - active.top - active.height / 2,
  };
};
export function canReport(roles: Role[], id: string, parent: string | null) {
  if (parent && !roles.some((r) => r.id === parent)) return false;
  const seen = new Set([id]);
  let node = parent;
  while (node) {
    if (seen.has(node)) return false;
    seen.add(node);
    node = roles.find((r) => r.id === node)?.reports_to || null;
  }
  return true;
}
/** Persisted sibling order takes precedence over group names. Each depth stays on one row. */
export function chartLevels(roles: Role[]) {
  roles = [...roles].sort((a, b) => (a.order || 0) - (b.order || 0));
  const levels: Role[][] = [],
    seen = new Set<string>();
  let row = roles.filter((r) => !r.reports_to || !roles.some((p) => p.id === r.reports_to));
  while (row.length) {
    levels.push(row);
    row.forEach((r) => seen.add(r.id));
    row = roles.filter((r) => !seen.has(r.id) && levels.at(-1)!.some((p) => p.id === r.reports_to));
  }
  const orphaned = roles.filter((r) => !seen.has(r.id));
  if (orphaned.length) levels.push(orphaned);
  return levels;
}
type Target = { movement: ChartMove; label: string };
function DropTarget({
  id,
  target,
  disabled,
  className = '',
  children,
}: {
  id: string;
  target?: Target;
  disabled: boolean;
  className?: string;
  children: ReactNode;
}) {
  const { setNodeRef, isOver } = useDroppable({ id, data: target, disabled: disabled || !target });
  return (
    <div
      ref={setNodeRef}
      data-org-target={target ? id : undefined}
      role="group"
      aria-label={target?.label}
      aria-disabled={disabled}
      className={`org-drop ${className} ${target ? (disabled ? 'unavailable' : 'available') : ''} ${isOver ? 'over' : ''}`}
    >
      {children}
    </div>
  );
}
function RoleNode({
  role,
  roles,
  project,
  active,
  disabled,
}: {
  role: Role;
  roles: Role[];
  project: string;
  active: string | null;
  disabled: boolean;
}) {
  const { data: s, canWrite } = useStore(),
    index = roles.indexOf(role),
    hire = s.hires.find((h) => h.role === role.id && h.status === 'active');
  const drag = useDraggable({ id: role.id, disabled: disabled || !role.reports_to });
  const moving = roles.find((r) => r.id === active),
    sibling =
      !!moving &&
      moving.id !== role.id &&
      moving.reports_to === role.reports_to &&
      !!role.reports_to;
  const swap = moving?.reports_to === role.id;
  const cardTarget: Target | undefined = active
    ? {
        movement: swap
          ? { kind: 'swap', role: active, lead: role.id }
          : { kind: 'report', role: active, parent: role.id },
        label: swap
          ? `Swap ${moving?.name} with ${role.name}`
          : !role.reports_to
            ? `Detach ${moving?.name} to ${role.name}`
            : `Report ${moving?.name} to ${role.name}`,
      }
    : undefined;
  const siblings = roles
    .filter((r) => r.reports_to === role.reports_to && r.id !== active)
    .sort((a, b) => (a.order || 0) - (b.order || 0));
  const after = siblings[siblings.findIndex((r) => r.id === role.id) + 1]?.id || null;
  const group = role.group || `${role.name} team`;
  const groupMembers = role.group
    ? siblings.filter((r) => r.group === role.group).map((r) => r.id)
    : [role.id];
  const reviewer = roles.find((r) => r.id === role.reviewed_by),
    task = s.tasks.find(
      (t) => t.role === role.id && !['done', 'cancelled'].includes(t.state.state),
    );
  const state = !canWrite
    ? 'Last known state'
    : task
      ? stateLabels[task.state.state]
      : hire
        ? 'Idle'
        : 'Not hired';
  return (
    <div className={`org-node-wrap ${drag.isDragging ? 'dragging' : ''}`}>
      {active && (
        <DropTarget
          id={`before-${role.id}`}
          className="org-sibling before"
          target={{
            movement: { kind: 'reorder', role: active, before: role.id },
            label: `Place ${moving?.name} before ${role.name}`,
          }}
          disabled={disabled || !sibling}
        >
          <span>Before</span>
        </DropTarget>
      )}
      <DropTarget
        id={`under-${role.id}`}
        className="org-card-target"
        target={cardTarget}
        disabled={
          disabled ||
          !moving ||
          (!swap && (moving.reports_to === role.id || !canReport(roles, moving.id, role.id)))
        }
      >
        <Card asChild density="compact">
          <button
            ref={drag.setNodeRef}
            {...(role.reports_to ? drag.attributes : {})}
            {...drag.listeners}
            data-org-role={role.id}
            className={`org-node ${role.reports_to ? 'org-movable' : ''}`}
            onClick={() => {
              if (!role.reports_to) go(routes.company(role.id, project));
            }}
            onDoubleClick={() => go(routes.company(role.id, project))}
            aria-label={
              role.reports_to
                ? `Move ${role.name}. Space to pick up, arrows to choose a destination, Space to drop, Escape to cancel.`
                : `Open ${role.name} profile`
            }
          >
            <span className="org-node-top">
              <IdentityMark
                value={{
                  icon:
                    [
                      'Compass',
                      'DraftingCompass',
                      'PanelsTopLeft',
                      'Braces',
                      'ScanEye',
                      'NotebookPen',
                      'Accessibility',
                    ][index] || 'Compass',
                  colour: 'accent',
                }}
                size={18}
              />
              <Status>{state}</Status>
            </span>
            <strong>{role.name}</strong>
            <span className="org-model">
              {hire ? `${hire.model}, ${hire.effort}` : 'Choose a model'}
            </span>
            {role.group && <span className="org-role-group">{role.group}</span>}
            {role.lead && <span className="org-lead-label">Lead, adds a model run per task</span>}
            <span className="org-review">
              {reviewer ? `Reviewed by ${reviewer.name}` : 'Review by task area'}
            </span>
          </button>
        </Card>
        {cardTarget && (
          <span className="org-target-label" aria-hidden>
            {cardTarget.label}
          </span>
        )}
      </DropTarget>
      <a className="org-profile-link" href={`#${routes.company(role.id, project)}`}>
        Profile →
      </a>
      {active && (
        <>
          <DropTarget
            id={`after-${role.id}`}
            className="org-sibling after"
            target={{
              movement: { kind: 'reorder', role: active, before: after },
              label: `Place ${moving?.name} after ${role.name}`,
            }}
            disabled={disabled || !sibling}
          >
            <span>After</span>
          </DropTarget>
          <DropTarget
            id={`group-${role.id}`}
            className="org-group-target"
            target={{
              movement: { kind: 'group', roles: [...groupMembers, active], group },
              label: `Group ${moving?.name} with ${role.name} in ${group}`,
            }}
            disabled={disabled || !sibling || moving?.group === group}
          >
            <span>{role.group ? `Join ${group}` : `Group with ${role.name}`}</span>
          </DropTarget>
        </>
      )}
    </div>
  );
}
export function OrgChart({ project }: { project: string }) {
  const { data: s, canWrite } = useStore(),
    roles = s.roles[project] || [],
    action = useAction(),
    [active, setActive] = useState<string | null>(null),
    levels = chartLevels(roles);
  const keyboardMoved = useRef(false),
    [announcement, announce] = useState('');
  const sensors = useSensors(
    useSensor(PointerSensor, { activationConstraint: { distance: 8 } }),
    useSensor(KeyboardSensor, {
      coordinateGetter: (event, context) => {
        const coordinates = keyboardCoordinates(event, context);
        if (coordinates) keyboardMoved.current = true;
        return coordinates;
      },
    }),
  );
  const canvas = useRef<HTMLDivElement>(null),
    [paths, setPaths] = useState<string[]>([]);
  useLayoutEffect(() => {
    const container = canvas.current;
    if (!container) return;
    const draw = () => {
      const bounds = container.getBoundingClientRect(),
        owner = container.querySelector('[data-org-owner]')?.getBoundingClientRect();
      const positions = new Map(
        Array.from(container.querySelectorAll<HTMLElement>('[data-org-role]')).map((element) => [
          element.dataset.orgRole,
          element.getBoundingClientRect(),
        ]),
      );
      const next = roles.flatMap((role) => {
        const from = role.reports_to ? positions.get(role.reports_to) : owner,
          to = positions.get(role.id);
        if (!from || !to) return [];
        const x1 = from.left + from.width / 2 - bounds.left,
          y1 = from.bottom - bounds.top,
          x2 = to.left + to.width / 2 - bounds.left,
          y2 = to.top - bounds.top,
          middle = (y1 + y2) / 2;
        return [`M ${x1} ${y1} C ${x1} ${middle}, ${x2} ${middle}, ${x2} ${y2}`];
      });
      setPaths((old) => (JSON.stringify(old) === JSON.stringify(next) ? old : next));
    };
    const observer = new ResizeObserver(draw);
    observer.observe(container);
    draw();
    window.addEventListener('resize', draw);
    return () => {
      observer.disconnect();
      window.removeEventListener('resize', draw);
    };
  }, [roles]);
  const region = useRef<HTMLDivElement>(null),
    loaded = roles.length > 0;
  // Centre the top of the chart once per project, and again while layout settles. The owner's
  // first scroll, pointer or key in the chart stops it, so later resizes never move their scroll.
  useLayoutEffect(() => {
    const container = region.current,
      owner = container?.querySelector('[data-org-owner]');
    if (!container || !owner || !loaded) return;
    let placed = container.scrollLeft;
    const centre = () => {
      if (container.scrollWidth <= container.clientWidth) return;
      const bounds = container.getBoundingClientRect(),
        card = owner.getBoundingClientRect();
      container.scrollLeft +=
        card.left +
        card.width / 2 -
        (bounds.left + container.clientLeft + container.clientWidth / 2);
      placed = container.scrollLeft;
    };
    const observer = new ResizeObserver(centre),
      events = ['pointerdown', 'wheel', 'touchstart', 'keydown'];
    const scrolled = () => {
      if (container.scrollLeft !== placed) stop();
    };
    const stop = () => {
      observer.disconnect();
      container.removeEventListener('scroll', scrolled);
      events.forEach((name) => container.removeEventListener(name, stop));
    };
    [container, canvas.current, owner].forEach((element) => element && observer.observe(element));
    container.addEventListener('scroll', scrolled);
    events.forEach((name) => container.addEventListener(name, stop));
    centre();
    return stop;
  }, [project, loaded]);
  const moving = roles.find((r) => r.id === active),
    root = roles.find((r) => !r.reports_to);
  return (
    <>
      <div className="org-toolbar">
        <InfoTip
          label="Shape your team"
          text="Drag onto a role to report to it, onto your own lead to swap, or before and after siblings to reorder. Drop onto a group target to group siblings. Drop on the top role or detach zone to detach. Space picks up, arrows choose a destination, Space drops, Escape cancels. Edit review assignment in the profile."
        />
      </div>
      <DataError message={action.error} />
      <div
        className="sr-only"
        role="status"
        aria-label="Chart move"
        aria-live="polite"
        aria-atomic="true"
      >
        {announcement}
      </div>
      <DndContext
        sensors={sensors}
        collisionDetection={(args) =>
          args.pointerCoordinates
            ? pointerWithin(args)
            : keyboardMoved.current
              ? closestCenter(args)
              : []
        }
        accessibility={{
          screenReaderInstructions: {
            draggable:
              'Press Space to pick up a role, arrows to choose a named reporting, swap, sibling, group or detach destination, Space to drop, Escape to cancel.',
          },
          announcements: {
            onDragStart: () => undefined,
            onDragOver: () => undefined,
            onDragEnd: () => undefined,
            onDragCancel: () => undefined,
          },
        }}
        onDragStart={({ active }) => {
          keyboardMoved.current = false;
          setActive(String(active.id));
          announce(
            `Picked up ${roles.find((r) => r.id === active.id)?.name}. Choose a destination.`,
          );
        }}
        onDragOver={({ over }) =>
          announce(
            over?.data.current?.label || 'No destination. Drop here to leave the chart unchanged.',
          )
        }
        onDragCancel={() => {
          announce(`Move cancelled for ${moving?.name}. No changes.`);
          setActive(null);
        }}
        onDragEnd={({ over }) => {
          setActive(null);
          const target = over?.data.current as Target | undefined;
          if (!target) {
            announce(`No changes to ${moving?.name}.`);
            return;
          }
          announce(`Applying: ${target.label}.`);
          void action
            .act(() => s.moveChart(project, target.movement))
            .then((done) =>
              announce(
                done
                  ? `Completed: ${target.label}.`
                  : `Move rejected for ${moving?.name}. No changes.`,
              ),
            );
        }}
      >
        {active && root && (
          <>
            <DropTarget
              id="detach"
              className="org-detach"
              target={{
                movement: { kind: 'report', role: active, parent: root.id },
                label: `Detach ${moving?.name} to ${root.name}`,
              }}
              disabled={
                action.busy || moving?.reports_to === root.id || !canReport(roles, active, root.id)
              }
            >
              <span>Detach to {root.name}</span>
            </DropTarget>
            {moving?.group && (
              <DropTarget
                id="ungroup"
                className="org-ungroup"
                target={{
                  movement: { kind: 'group', roles: [active], group: null },
                  label: `Remove ${moving.name} from ${moving.group}`,
                }}
                disabled={action.busy}
              >
                <span>Leave {moving.group}</span>
              </DropTarget>
            )}
          </>
        )}
        <div
          ref={region}
          className="org-chart org-editor"
          role="region"
          aria-label="Company reporting chart"
          tabIndex={0}
        >
          <div ref={canvas} className="org-canvas org-level-canvas">
            <svg className="org-connectors" aria-hidden="true">
              {paths.map((d, i) => (
                <path key={i} d={d} />
              ))}
            </svg>
            <Card density="compact" className="org-owner" data-org-owner>
              <strong>Owner</strong>
              <span>Direction and approvals</span>
            </Card>
            {levels.map((row, depth) => (
              <div
                className="org-level"
                data-org-level={depth}
                key={depth}
                role="group"
                aria-label={`Reporting level ${depth + 1}`}
              >
                {row.map((role) => (
                  <RoleNode
                    key={role.id}
                    role={role}
                    roles={roles}
                    project={project}
                    active={active}
                    disabled={action.busy || !canWrite}
                  />
                ))}
              </div>
            ))}
          </div>
        </div>
        <DragOverlay>
          {active && (
            <Card className="org-node org-overlay" density="compact">
              {moving?.name}
            </Card>
          )}
        </DragOverlay>
      </DndContext>
    </>
  );
}
