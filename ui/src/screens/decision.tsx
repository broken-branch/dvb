import { routes, type Origin } from '@/lib/routes';
import { TextDisclosure } from '@/components/dvb/text-disclosure';
import { useEffect, useState } from 'react';
import { ArrowLeft, ArrowRight, Check, Send } from 'lucide-react';
import { Card } from '@/components/dvb/card';
import { Status } from '@/components/dvb/common';
import { ProjectLabel } from '@/components/dvb/project-identity';
import { DataError, useAction } from '@/components/dvb/data-feedback';
import { Button } from '@/components/ui/button';
import { RadioGroup, RadioGroupItem } from '@/components/ui/radio-group';
import { Textarea } from '@/components/ui/textarea';
import { Badge } from '@/components/ui/badge';
import { go, useStore } from '@/lib/store';
import type { Question } from '@/lib/data/types';
import { goalTitle, pageQuestion, taskList } from '@/lib/data/work';
/** The question dvb chose for the owner (bare #decision or Next question): no option starts selected. */
let unchosen = '';
function show(id: string, replace = false) {
  unchosen = id;
  if (replace) location.replace(`#${routes.decision(id)}`);
  else go(routes.decision(id));
}
export function Decision({ id = '', from }: { id?: string; from?: Origin }) {
  const { data: s } = useStore(),
    pick = id ? undefined : pageQuestion(s.questions)?.id,
    q = s.questions.find((q) => q.id === id);
  // Bare #decision pins one question so an answer never swaps the next one in under the owner.
  useEffect(() => {
    if (pick) show(pick, true);
  }, [pick]);
  if (pick) return null;
  if (!q)
    return (
      <Card className="empty-state">
        <h1>No current question.</h1>
        <Button onClick={() => go(routes.home())}>Home</Button>
      </Card>
    );
  return <QuestionForm key={q.id} question={q} from={from} />;
}
function QuestionForm({ question: q, from }: { question: Question; from?: Origin }) {
  const { data: s, canWrite } = useStore(),
    action = useAction();
  const [choice, setChoice] = useState(unchosen === q.id ? '' : String(q.recommended)),
    [reply, setReply] = useState(''),
    [receipt, setReceipt] = useState<Question>();
  useEffect(() => {
    // Only this showing starts unselected; opening the question again keeps the recommendation.
    if (unchosen === q.id) unchosen = '';
  }, [q.id]);
  const answer = receipt?.answer || q.answer,
    next = pageQuestion(s.questions.filter((o) => o.id !== q.id)),
    // The page the question was opened from; Now and direct links go Home.
    back =
      from?.page === 'project'
        ? s.projects.find((p) => p.id === from.id)?.name
        : from?.page === 'task'
          ? s.tasks.find((t) => t.id === from.id)?.title
          : undefined,
    heading = goalTitle(q.subject),
    body = q.subject.trim().slice(heading.length).trim();
  return (
    <div className="decision-page">
      <Button
        variant="ghost"
        onClick={() => go(from && back ? routes[from.page](from.id) : routes.home())}
      >
        <ArrowLeft size={14} />
        {back || 'Home'}
      </Button>
      <Card className="decision-detail">
        <div className="decision-meta">
          <Status tone={answer ? 'good' : q.open ? 'warn' : 'neutral'}>
            {answer ? 'Answered' : q.open ? 'Waiting for you' : 'Closed'}
          </Status>
          <ProjectLabel id={q.project} />
        </div>
        <TextDisclosure as="h1" text={heading} />
        {body && <p className="question-body written-subject">{body}</p>}
        {!!q.plan?.length && <PlanTasks question={q} />}
        <DataError message={action.error} />
        {answer ? (
          <div className="answer-receipt">
            <Check size={32} />
            {/* Only this session's own answer is known to be the owner's; dvb may have given any other. */}
            <h2>{receipt ? 'Your direction is with the team.' : 'This question is answered.'}</h2>
            <p>
              {'reply' in answer
                ? answer.reply
                : q.options.find((o) => o.id === answer.choice)?.text}
            </p>
            <div className="actions">
              <Button onClick={() => go(routes.project(q.project))}>Back to project</Button>
              {next && (
                <Button id="next-question" variant="outline" onClick={() => show(next.id)}>
                  Next question
                  <ArrowRight size={14} />
                </Button>
              )}
            </div>
          </div>
        ) : !q.open ? (
          <p>This question is closed.</p>
        ) : (
          <form
            id="decision-form"
            onSubmit={(e) => {
              e.preventDefault();
              if (action.busy || !choice) return;
              void action.act(async () => {
                const result = await s.answer(
                  q.id,
                  choice === 'reply' ? { reply: reply.trim() } : { choice: Number(choice) },
                );
                if (
                  !result.ok &&
                  (result.refusal.code === 'QUESTION_LAPSED' ||
                    result.refusal.code === 'VALUE_CHANGED')
                )
                  return {
                    ...result,
                    refusal: {
                      ...result.refusal,
                      detail:
                        'This question has changed. Open the current question before answering.',
                    },
                  };
                return result;
              }, setReceipt);
            }}
          >
            <RadioGroup
              value={choice}
              onValueChange={setChoice}
              className={`decision-choices ${q.format === 'quick_compare' ? 'compare' : ''}`}
            >
              {q.options.map((o) => (
                <Card asChild key={o.id} density="compact">
                  <label
                    className={`choice-row ${choice === String(o.id) ? 'selected' : ''}`}
                    data-format={q.format}
                  >
                    <RadioGroupItem id={`answer-${o.id}`} value={String(o.id)} />
                    <span>
                      <span className="label-group">
                        <strong>{o.text}</strong>
                        {o.id === q.recommended && <Badge>Recommended</Badge>}
                      </span>
                      {q.format === 'swot' ? (
                        <SwotGrid effect={o.effect} />
                      ) : (
                        <small>{o.effect}</small>
                      )}
                    </span>
                  </label>
                </Card>
              ))}
              <Card asChild density="compact">
                <label className={`choice-row ${choice === 'reply' ? 'selected' : ''}`}>
                  <RadioGroupItem value="reply" id="answer-reply" />
                  <strong>Write a reply</strong>
                </label>
              </Card>
            </RadioGroup>
            {choice === 'reply' && (
              <label className="field">
                Your reply
                {/* Mounts only when the owner picks Write a reply, by click or number key. */}
                <Textarea
                  autoFocus
                  required
                  value={reply}
                  onChange={(e) => setReply(e.target.value)}
                />
              </label>
            )}
            <div className="decision-submit">
              <Button
                type="submit"
                disabled={
                  action.busy || !canWrite || !choice || (choice === 'reply' && !reply.trim())
                }
              >
                <Send size={15} />
                {action.busy ? 'Sending…' : 'Send answer'}
              </Button>
            </div>
          </form>
        )}
      </Card>
    </div>
  );
}
function PlanTasks({ question: q }: { question: Question }) {
  const { data: s } = useStore(),
    plan = q.plan || [],
    roles = s.roles[q.project] || [];
  return (
    <ol className="plan-tasks" aria-label="Planned tasks">
      {plan.map((t) => {
        const after = (t.after || []).map((id) => {
          const n = plan.findIndex((p) => p.task === id);
          return n < 0 ? s.tasks.find((task) => task.id === id)?.title || id : String(n + 1);
        });
        return (
          <li key={t.task}>
            <strong className="task-written-text">{t.title}</strong>
            <small className="work-facts actions">
              <span>{roles.find((r) => r.id === t.role || r.slug === t.role)?.name || t.role}</span>
              {t.routine && <span>Routine</span>}
              {after.length > 0 && (
                <span>
                  Starts after {taskList(after)} {after.length === 1 ? 'merges' : 'merge'}
                </span>
              )}
            </small>
          </li>
        );
      })}
    </ol>
  );
}
const swotLabels = [
  ['Strengths', /^strengths?/i],
  ['Weaknesses', /^weakness(es)?/i],
  ['Opportunities', /^opportunit(y|ies)/i],
  ['Threats', /^threats?/i],
] as const;
function SwotGrid({ effect }: { effect: string }) {
  const parts = effect.split('\n').flatMap((line) => {
    const [head, ...rest] = line.split(':'),
      found = swotLabels.find(([, re]) => re.test(head.trim()));
    return found && rest.length ? [{ label: found[0], text: rest.join(':').trim() }] : [];
  });
  if (!parts.length) return <small>{effect}</small>;
  return (
    <dl className="swot-grid">
      {parts.map((p) => (
        <div key={p.label}>
          <dt>{p.label}</dt>
          <dd>{p.text}</dd>
        </div>
      ))}
    </dl>
  );
}
