import { useCallback, useRef, useState } from 'react';
import { routes } from '@/lib/routes';
import {
  ArrowLeft,
  MessageSquareReply,
  Play,
  RotateCcw,
  SlidersHorizontal,
  Square,
} from 'lucide-react';
import { Card } from '@/components/dvb/card';
import { Heading, PageTabs, Status } from '@/components/dvb/common';
import { DataError, toggleWork, useAction } from '@/components/dvb/data-feedback';
import { ProjectLabel } from '@/components/dvb/project-identity';
import { Button } from '@/components/ui/button';
import { Popover, PopoverContent, PopoverTrigger } from '@/components/ui/popover';
import { Switch } from '@/components/ui/switch';
import { Textarea } from '@/components/ui/textarea';
import { go } from '@/lib/store';
import { eventText, outputText, stateLabels, word } from '@/lib/data/display';
import { taskNote, waitingText } from '@/lib/data/work';
import { useTask } from './task/use-task';
import { TaskChanges, TaskReview, TaskChecks, TaskDecisions, TaskFeed } from './task/tabs';
export function TaskPage({ id, tab = 'changes' }: { id: string; tab?: string }) {
  const {
    s,
    canWrite,
    task,
    events,
    detailError,
    run,
    output,
    diff,
    verdict,
    panels,
    reverse,
    purpose,
    role,
    mismatch,
    check,
    pr,
    rerouted,
  } = useTask(id);
  const action = useAction(),
    noteAction = useAction();
  const [note, setNote] = useState<string>(),
    // The task's state when it was sent back, then each builder state it passes through.
    [sent, setSent] = useState<string>(),
    opener = useRef<HTMLElement | null>(null),
    // The form and its opener unmount once sent, so focus moves to the confirmation.
    focusSent = useCallback((el: HTMLElement | null) => el?.focus(), []);
  const state = task?.state.state,
    building = ['preparing', 'running', 'collecting', 'publishing'].includes(state || '');
  if (sent && state !== sent) setSent(building ? state : undefined);
  if (!task)
    return (
      <Card className="empty-state">
        <h1>Task unavailable.</h1>
      </Card>
    );
  const stopped = task.state.state === 'stopped',
    failed = task.state.state === 'failed',
    ended = ['done', 'cancelled', 'failed'].includes(task.state.state),
    finished = ended && !failed,
    disabled = action.busy || !canWrite,
    // The engine sends back only a task with a PR that is stopped or in review.
    sendable = !sent && !!task.pr && !!task.head && (stopped || task.state.state === 'in_review');
  const closeNote = () => {
    setNote(undefined);
    if (opener.current?.isConnected) opener.current.focus();
  };
  return (
    <>
      <Button variant="ghost" onClick={() => go(routes.project(task.project))}>
        <ArrowLeft size={14} />
        <ProjectLabel id={task.project} />
      </Button>
      <Heading
        title={<span className="task-written-text">{task.title}</span>}
        description="Follow the work, changes and review."
      >
        {!ended && (
          <Button
            variant={stopped ? 'default' : 'outline'}
            disabled={disabled}
            onClick={() => void action.act(() => toggleWork(s, task))}
          >
            {stopped ? <Play size={14} /> : <Square size={14} />}
            {stopped ? 'Resume task' : 'Stop task'}
          </Button>
        )}
        {failed && (
          <Button disabled={disabled} onClick={() => void action.act(() => s.retryTask(id))}>
            <RotateCcw size={14} />
            Retry task
          </Button>
        )}
        {sendable && (
          <Button
            variant="outline"
            disabled={disabled}
            aria-expanded={note !== undefined}
            title="Send this task back to its builder for one more round on the same PR."
            onClick={(e) => {
              opener.current = e.currentTarget;
              setNote((kept) => kept ?? '');
            }}
          >
            <MessageSquareReply size={14} />
            Send back with a note
          </Button>
        )}
        <Popover>
          <PopoverTrigger asChild>
            <Button variant="outline" size="icon" aria-label="Choose task panels">
              <SlidersHorizontal size={16} />
            </Button>
          </PopoverTrigger>
          <PopoverContent>
            <h3>Task panels</h3>
            {(['review', 'checks'] as const).map((key) => (
              <label key={key} className="switch-row">
                {key === 'review' ? 'Review' : 'Checks'}
                <Switch
                  checked={panels[key] !== false}
                  disabled={disabled}
                  onCheckedChange={(value) => {
                    void action.act(() =>
                      s.changeSettings({ values: { task_panels: { ...panels, [key]: value } } }),
                    );
                    if (tab === key && !value) go(routes.task(id, 'changes'));
                  }}
                />
              </label>
            ))}
            <label className="switch-row">
              Files on the right
              <Switch
                checked={reverse}
                disabled={disabled}
                onCheckedChange={(value) =>
                  void action.act(() => s.changeSettings({ values: { task_files_right: value } }))
                }
              />
            </label>
          </PopoverContent>
        </Popover>
      </Heading>
      <DataError message={action.error} />
      <DataError message={detailError} />
      {sendable && note !== undefined && (
        <Card>
          <form
            onKeyDown={(e) => {
              if (e.key !== 'Escape') return;
              e.preventDefault();
              closeNote();
            }}
            onSubmit={(e) => {
              e.preventDefault();
              const from = task.state.state;
              void noteAction.act(
                () => s.retryTask(id, { text: note.trim() }),
                () => {
                  setSent(from);
                  setNote(undefined);
                },
              );
            }}
          >
            <label className="field">
              Note for the builder
              <Textarea
                autoFocus
                required
                className="written-input"
                value={note}
                onChange={(e) => setNote(e.target.value)}
              />
            </label>
            <DataError message={noteAction.error} />
            <div className="actions">
              <Button disabled={disabled || noteAction.busy || !note.trim()}>Send back</Button>
              <Button type="button" variant="ghost" onClick={closeNote}>
                Cancel
              </Button>
            </div>
          </form>
        </Card>
      )}
      {purpose && (
        <p data-task-summary className="task-written-text">
          {purpose}
        </p>
      )}
      <Card className="task-live">
        <div>
          <Status tone={ended ? 'neutral' : stopped ? 'bad' : 'working'}>
            <span data-task-state>
              {waitingText(s.tasks, task) ? word('task.waiting') : stateLabels[task.state.state]}
            </span>
          </Status>
          <h2>
            {taskNote(s.tasks, task) ||
              task.reason ||
              (task.state.state === 'failed'
                ? task.state.reason
                : stopped
                  ? 'Work is saved.'
                  : task.state.state === 'done'
                    ? 'This task is finished.'
                    : task.state.state === 'cancelled'
                      ? 'This task was cancelled.'
                      : canWrite
                        ? 'Following the latest work.'
                        : 'Waiting to reconnect for a fresh status.')}
          </h2>
          {run.value && (
            <p data-run-line>
              {run.value.model} {role?.name || task.role} {run.value.effort} {run.value.reason}
            </p>
          )}
          {sent && (
            <p ref={focusSent} tabIndex={-1} role="status">
              Sent back to its builder with your note.
            </p>
          )}
          {rerouted && <p data-rerouted>{eventText(rerouted, s)}</p>}
          {mismatch && (
            <p data-model-mismatch role="alert">
              Model mismatch: asked for {mismatch.requested}, served {mismatch.served}
            </p>
          )}
          {/* A finished task's last run lines are history, not its status; a failed one keeps them. */}
          {!finished &&
            output.value?.slice(-3).map((line) => <p key={line.id}>{outputText(line.item)}</p>)}
          <DataError message={output.error} />
        </div>
        <div className="task-live-side">
          {pr?.kind === 'pr.opened' ? (
            <a href={pr.payload.url} target="_blank" rel="noreferrer">
              PR #{pr.payload.pr} on GitHub
            </a>
          ) : (
            task.pr && <span>PR #{task.pr}</span>
          )}
          <small>{task.head || 'No published change yet'}</small>
        </div>
      </Card>
      <PageTabs
        value={tab}
        onChange={(value) => {
          go(routes.task(id, value));
        }}
        items={[
          {
            id: 'changes',
            label: 'Changes',
            content: <TaskChanges diff={diff} reverse={reverse} />,
          },
          ...(panels.review !== false
            ? [
                {
                  id: 'review',
                  label: 'Review',
                  content: <TaskReview verdict={verdict} head={task.head} />,
                },
              ]
            : []),
          ...(panels.checks !== false
            ? [{ id: 'checks', label: 'Checks', content: <TaskChecks check={check} /> }]
            : []),
          {
            id: 'decisions',
            label: 'Team decisions',
            content: <TaskDecisions events={events} roles={s.roles[task.project] || []} />,
          },
          {
            id: 'feed',
            label: 'Feed',
            content: <TaskFeed task={id} project={task.project} />,
          },
        ]}
      />
    </>
  );
}
