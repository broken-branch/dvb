import { useState } from 'react';
import { Button } from '@/components/ui/button';
import { Card } from '@/components/dvb/card';
import { Status } from '@/components/dvb/common';
import { ArrowRight } from 'lucide-react';

/** Four short screens over made-up data; nothing here reads or changes real work. */
const screens = [
  {
    key: 'projects',
    title: 'Projects',
    text: 'Each project is one repository. Every project shows its goal, its tasks and what needs you.',
    sample: [
      ['Fieldnotes', 'Bring your notebooks together', '3 tasks'],
      ['Garden', 'Plan the spring planting', '1 task'],
    ],
  },
  {
    key: 'orchestrator',
    title: 'The orchestrator',
    text: 'You state a goal. The orchestrator turns it into a plan and hands the steps to builders. Nothing starts until you approve.',
    sample: [
      ['Plan', 'Import Markdown notebooks', 'Waiting for you'],
      ['Build', 'Read a folder of notes', 'Not started'],
    ],
  },
  {
    key: 'questions',
    title: 'Questions',
    text: 'When the team needs a decision, it asks. Pick an option or write your own reply. The team waits for you.',
    sample: [['Question', 'Keep the first release small?', 'Needs you']],
  },
  {
    key: 'reviews',
    title: 'Reviews',
    text: 'Every change is reviewed before it is merged. You see the result, and you stay in charge of what ships.',
    sample: [
      ['Review', 'Read a folder of notes', 'Approved'],
      ['Checks', 'All passed', 'Ready to merge'],
    ],
  },
] as const;

export function Walkthrough({ onDone }: { onDone: () => void }) {
  const [index, setIndex] = useState(0),
    screen = screens[index],
    last = index === screens.length - 1;
  return (
    <>
      <h2>How dvb works</h2>
      <p aria-live="polite">
        {index + 1} of {screens.length}. {screen.title}
      </p>
      <p>{screen.text}</p>
      {screen.sample.map(([kind, title, state]) => (
        <Card density="compact" key={title}>
          <div className="login-row">
            <div>
              <small>{kind}</small>
              <h3 className="task-written-text">{title}</h3>
            </div>
            <Status>{state}</Status>
          </div>
        </Card>
      ))}
      <small>Sample data. Your real work appears after you add a project.</small>
      <div className="setup-actions">
        <Button type="button" variant="ghost" onClick={onDone}>
          Skip
        </Button>
        <Button type="button" onClick={() => (last ? onDone() : setIndex(index + 1))}>
          {last ? 'Done' : 'Next'}
          <ArrowRight size={15} />
        </Button>
      </div>
    </>
  );
}
