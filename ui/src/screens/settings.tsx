import { routes } from '@/lib/routes';
import { Check, Palette as PaletteIcon } from 'lucide-react';
import { toast } from 'sonner';
import { Card } from '@/components/dvb/card';
import {
  Heading,
  PageTabs,
  ChoiceSelect,
  Status,
  Meter,
  Hint,
  InfoTip,
} from '@/components/dvb/common';
import { DataError, useAction, useRead } from '@/components/dvb/data-feedback';
import { Button } from '@/components/ui/button';
import { RadioGroup, RadioGroupItem } from '@/components/ui/radio-group';
import { Input } from '@/components/ui/input';
import { Switch } from '@/components/ui/switch';
import { useAppearance, textSizes } from '@/lib/appearance';
import { palettes, themeName, contrast } from '@/lib/theme';
import type { SettingsKey, UpdateInfo } from '@/lib/data/types';
import { HistoryButton } from '@/components/dvb/history-sheet';
import { LoginStep } from '@/components/dvb/login-step';
import { useStore, go } from '@/lib/store';
import { word } from '@/lib/data/display';
import { useEffect, useRef, useState } from 'react';
function Appearance() {
  const a = useAppearance(),
    { canWrite } = useStore();
  return (
    <div className="appearance-grid">
      <Card asChild>
        <section>
          <h2>Make yourself at home.</h2>
          <div className="setting-row">
            <label className="field">
              Colour theme
              <ChoiceSelect
                disabled={!canWrite}
                label="Colour theme"
                value={a.theme}
                onChange={a.setTheme}
                options={[
                  { value: 'auto', label: 'Auto / follow Omarchy' },
                  ...Object.keys(palettes).map((k) => ({ value: k, label: themeName(k) })),
                ]}
              />
            </label>
            <HistoryButton target={{ kind: 'setting', key: 'theme', name: 'Colour theme' }} />
          </div>
          {a.theme === 'auto' && (
            <Card asChild>
              <div className="auto-theme">
                <Status tone="good">Following the desktop theme</Status>
              </div>
            </Card>
          )}
          <div className="setting-row">
            <label className="field">
              <span>
                Font{' '}
                <InfoTip text="Auto follows the system monospace. Other fonts must be installed locally." />
              </span>
              <ChoiceSelect
                disabled={!canWrite}
                label="Font"
                value={a.font}
                onChange={a.setFont}
                options={[
                  'Auto',
                  'JetBrainsMono Nerd Font',
                  'DejaVu Sans Mono',
                  'Liberation Mono',
                  'monospace',
                ]}
              />
            </label>
            <HistoryButton target={{ kind: 'setting', key: 'font', name: 'Font' }} />
          </div>
          <div className="field">
            <span id="text-size-label">Text size</span>
            <RadioGroup
              className="actions text-size"
              aria-labelledby="text-size-label"
              value={a.textSize}
              onValueChange={(value) => a.setTextSize(value as keyof typeof textSizes)}
              disabled={!canWrite}
              orientation="horizontal"
            >
              {(Object.keys(textSizes) as (keyof typeof textSizes)[]).map((size) => (
                <RadioGroupItem asChild key={size} value={size}>
                  <Button variant="outline">{size}</Button>
                </RadioGroupItem>
              ))}
            </RadioGroup>
          </div>
        </section>
      </Card>
      <Card asChild>
        <aside className="appearance-example">
          <div className="example-heading">
            <PaletteIcon size={16} />
            <span>Your colours</span>
          </div>
          <div className="theme-swatches">
            {['background', 'foreground', 'accent', 'green', 'yellow', 'red'].map((k) => (
              <Hint key={k} label={k}>
                <span
                  role="img"
                  tabIndex={0}
                  aria-label={`${k} colour`}
                  style={{ background: `var(--palette-${k})` }}
                />
              </Hint>
            ))}
          </div>
          <Status tone="working">In review</Status>
          <h2>Your next good idea.</h2>
          <Meter value={64} label="Example progress" />
          <div className="actions">
            <Button onClick={() => toast.success('This is how confirmation feels.')}>
              Try the feedback
            </Button>
            <Button variant="outline" onClick={() => go(routes.home())}>
              See Home
            </Button>
          </div>
          <div className="contrast-note">
            <Check size={14} />
            <span>
              Contrast {contrast(a.palette.foreground, a.palette.background).toFixed(1)}:1{' '}
              <InfoTip text="Source palette contrast. Display colours adjust toward light or dark when needed for readable text and focus." />
            </span>
          </div>
        </aside>
      </Card>
    </div>
  );
}
function Publication() {
  const { data: s, canWrite } = useStore(),
    action = useAction(),
    read = useRead(() => s.publication(), [s.publication]);
  const [saved, setSaved] = useState<typeof read.value>(),
    [draft, setDraft] = useState<{ name: string; email: string }>();
  const current = saved ?? read.value;
  if (!current) return <DataError message={read.error} />;
  const on = draft !== undefined || current.as_owner,
    fields = draft ?? current,
    disabled = action.busy || !canWrite,
    publish = (name: string, email: string, as_owner: boolean) =>
      void action.act(
        () => s.setPublication({ name, email, as_owner }),
        (identity) => {
          setSaved(identity);
          setDraft(undefined);
        },
      );
  return (
    <form
      onSubmit={(event) => {
        event.preventDefault();
        publish(fields.name, fields.email, true);
      }}
    >
      <p>{word('publication.current', current)}</p>
      <label className="switch-row">
        {word('publication.as_owner')}
        <Switch
          aria-describedby="publication-as-owner-off"
          checked={on}
          disabled={disabled}
          onCheckedChange={(value) => {
            if (value) setDraft({ name: '', email: '' });
            // Empty with as_owner off is the engine's default identity.
            else if (current.as_owner) publish('', '', false);
            else setDraft(undefined);
          }}
        />
      </label>
      <p id="publication-as-owner-off" className="muted">
        {word('publication.as_owner.off')}
      </p>
      {on && (
        <>
          {(['name', 'email'] as const).map((key) => (
            <label className="field" key={key}>
              {word(`publication.${key}`)}
              <Input
                type={key === 'email' ? 'email' : 'text'}
                value={fields[key]}
                disabled={disabled}
                aria-invalid={!!action.error}
                aria-describedby={action.error ? 'publication-refusal' : undefined}
                onChange={(event) => setDraft({ ...fields, [key]: event.target.value })}
              />
            </label>
          ))}
          {action.error && (
            <p role="alert" id="publication-refusal" className="warning-text">
              {action.error}
            </p>
          )}
          {fields.name && fields.email && <p>{word('publication.as_owner.effect', fields)}</p>}
          <Button type="submit" disabled={disabled || !draft}>
            Save
          </Button>
        </>
      )}
    </form>
  );
}
function Updates() {
  const { data: s, canWrite } = useStore(),
    action = useAction(),
    [refresh, setRefresh] = useState(0),
    check = useRead(() => s.updateCheck(), [s.updateCheck, refresh]),
    [started, setStarted] = useState<UpdateInfo>(),
    [after, setAfter] = useState(() => s.events.at(-1)?.id ?? 0),
    info = started ?? check.value,
    outcome = s.events.findLast(
      (e) => e.id > after && (e.kind === 'engine.updated' || e.kind === 'engine.update_failed'),
    ),
    failure = outcome?.kind === 'engine.update_failed' ? word(outcome.kind, outcome.payload) : '',
    updating = !failure && info?.updating,
    version = 'engineVersion' in s.connection ? s.connection.engineVersion : undefined,
    previous = useRef({ state: s.connection.state, version });
  useEffect(() => {
    const returned =
      s.connection.state !== 'reconnecting' &&
      (previous.current.state === 'reconnecting' || previous.current.version !== version);
    previous.current = { state: s.connection.state, version };
    if (returned && info?.updating) {
      setStarted(undefined);
      setRefresh((value) => value + 1);
    }
  }, [s.connection.state, version, info?.updating]);
  return (
    <Card className="settings-panel">
      <h2>Update dvb</h2>
      {info && <p>Installed: {info.current}</p>}
      <DataError message={check.error && `The update check failed: ${check.error}`} />
      <div className="actions">
        <div role="status" aria-live="polite" aria-atomic="true">
          {failure ? (
            <Status tone="bad">{failure}</Status>
          ) : info ? (
            <Status tone={updating ? 'working' : info.available ? 'neutral' : 'good'}>
              {updating
                ? `Updating to ${updating.to}`
                : info.available
                  ? `Available: ${info.available}`
                  : 'Up to date'}
            </Status>
          ) : (
            !check.error && <p>Checking for a newer release…</p>
          )}
        </div>
        {info && (
          <Button
            disabled={action.busy || !canWrite || !!updating || !info.available}
            onClick={() => {
              setAfter(s.events.at(-1)?.id ?? 0);
              void action.act(() => s.startUpdate(), setStarted);
            }}
          >
            Update dvb
          </Button>
        )}
      </div>
      <DataError message={action.error} />
    </Card>
  );
}
export function Settings({ tab = 'appearance' }: { tab?: string }) {
  const { data: s, canWrite, settings: values } = useStore(),
    a = useAppearance(),
    action = useAction();

  const disabled = action.busy || !canWrite;
  const save = (key: SettingsKey, value: unknown) => {
    void action.act(() => s.changeSettings({ values: { [key]: value } }));
  };
  const modelMode = values.model_mode,
    pool = values.model_pool,
    runsMemory = s.settings.values.runs_memory_gb,
    memoryAvailable = runsMemory !== undefined,
    memoryValue = typeof runsMemory === 'number' ? runsMemory : '',
    ram = s.settings.ram_gb,
    halfRam = s.settings.half_ram_gb,
    memoryWarning = typeof runsMemory === 'number' && halfRam != null && runsMemory > halfRam;
  const preferences = (
    <div className="settings-two-col">
      <Card>
        <h2>How dvb talks to you</h2>
        <div className="setting-row">
          <label className="field">
            Response style
            <ChoiceSelect
              disabled={disabled}
              label="Response style"
              value={values.response_style}
              options={['Simple', 'Clear', 'Technical', 'Expert']}
              onChange={(value) => save('response_style', value)}
            />
          </label>
          <HistoryButton
            target={{ kind: 'setting', key: 'response_style', name: 'Response style' }}
          />
        </div>
        <div className="setting-row">
          <label className="field">
            Question format
            <ChoiceSelect
              disabled={disabled}
              label="Question format"
              value={values.question_format}
              options={[
                { value: 'simple', label: 'Simple question' },
                { value: 'quick_compare', label: 'Quick compare' },
                { value: 'swot', label: 'SWOT' },
              ]}
              onChange={(value) => save('question_format', value)}
            />
          </label>
          <HistoryButton
            target={{ kind: 'setting', key: 'question_format', name: 'Question format' }}
          />
        </div>
      </Card>
      <Card>
        <h2>Run capacity</h2>
        <div className="setting-row">
          <label className="field">
            Maximum runs
            <Input
              key={values.max_runs}
              type="number"
              min={1}
              step={1}
              required
              disabled={disabled}
              defaultValue={values.max_runs}
              aria-describedby="max-runs-hint"
              onBlur={(event) => {
                const input = event.currentTarget;
                const value = input.valueAsNumber;
                if (!input.validity.valid || !Number.isInteger(value) || value < 1) {
                  toast.error('Enter a whole number of at least 1.');
                  input.value = String(values.max_runs);
                } else if (value !== values.max_runs) {
                  save('max_runs', value);
                }
              }}
              onKeyDown={(event) => {
                if (event.key === 'Enter') event.currentTarget.blur();
              }}
            />
          </label>
          <HistoryButton target={{ kind: 'setting', key: 'max_runs', name: 'Maximum runs' }} />
        </div>
        <p id="max-runs-hint" className="muted">
          Each run may use up to 8 GB of memory; memory, disk and load holds still apply.
        </p>
        <div className="setting-row">
          <label className="field">
            Memory for all runs (GiB)
            <Input
              key={String(runsMemory)}
              type="number"
              min={2}
              max={ram ?? undefined}
              step={1}
              required
              disabled={disabled || !memoryAvailable}
              defaultValue={memoryValue}
              placeholder={memoryAvailable ? 'Unlimited' : 'Unavailable'}
              aria-describedby={
                !memoryAvailable
                  ? 'runs-memory-unavailable'
                  : memoryWarning
                    ? 'runs-memory-warning'
                    : undefined
              }
              onBlur={(event) => {
                const input = event.currentTarget;
                if (input.value === String(memoryValue)) return;
                const value = input.valueAsNumber;
                if (!input.validity.valid || !Number.isInteger(value) || value < 2) {
                  toast.error(
                    ram != null
                      ? `Enter a whole number between 2 and ${ram} GiB.`
                      : 'Enter a whole number of at least 2 GiB.',
                  );
                  input.value = String(memoryValue);
                } else if (value !== runsMemory) {
                  save('runs_memory_gb', value);
                }
              }}
              onKeyDown={(event) => {
                if (event.key === 'Enter') event.currentTarget.blur();
              }}
            />
          </label>
        </div>
        {!memoryAvailable && (
          <p id="runs-memory-unavailable" className="muted">
            This engine has no dvb.slice; install the engine units to set this limit.
          </p>
        )}
        {memoryWarning && (
          <p id="runs-memory-warning" className="warning-text">
            Several runs building at once can use this much memory and freeze the desktop.
          </p>
        )}
      </Card>
      <Card>
        <h2>Notifications</h2>
        <label className="switch-row">
          Questions for you
          <Switch checked disabled />
        </label>
        <div className="setting-row">
          <label className="switch-row">
            Something stopped unexpectedly
            <Switch
              checked={!!values.notify_faults}
              disabled={disabled}
              onCheckedChange={(value) => save('notify_faults', value)}
            />
          </label>
          <HistoryButton
            target={{
              kind: 'setting',
              key: 'notify_faults',
              name: 'Something stopped unexpectedly',
            }}
          />
        </div>
        <div className="setting-row">
          <label className="switch-row">
            Goals completed
            <Switch
              checked={!!values.notify_completed}
              disabled={disabled}
              onCheckedChange={(value) => save('notify_completed', value)}
            />
          </label>
          <HistoryButton
            target={{ kind: 'setting', key: 'notify_completed', name: 'Goals completed' }}
          />
        </div>
      </Card>
      <Card>
        <h2>{word('publication.heading')}</h2>
        <Publication />
      </Card>
    </div>
  );
  const models = (
    <Card className="settings-panel">
      <h2>Models</h2>
      <RadioGroup
        className="actions"
        aria-label="Model mode"
        value={modelMode}
        onValueChange={(mode) => save('model_mode', mode)}
        disabled={disabled}
        orientation="horizontal"
      >
        {(['Custom', 'Auto'] as const).map((mode) => (
          <RadioGroupItem asChild key={mode} value={mode}>
            <Button variant={modelMode === mode ? 'default' : 'outline'}>{mode}</Button>
          </RadioGroupItem>
        ))}
        <HistoryButton target={{ kind: 'setting', key: 'model_mode', name: 'Model mode' }} />
      </RadioGroup>
      <p>
        {modelMode === 'Auto'
          ? 'The orchestrator picks each task’s model and effort from your pool, and says why.'
          : 'Choose each role’s model and effort in Company.'}
      </p>
      <div className="setting-row">
        <label className="switch-row">
          When a vendor is at its limit, move its work to the orchestrator’s vendor
          <Switch
            checked={values.vendor_fallback !== false}
            disabled={disabled}
            onCheckedChange={(value) => save('vendor_fallback', value)}
          />
        </label>
        <HistoryButton
          target={{ kind: 'setting', key: 'vendor_fallback', name: 'Vendor fallback' }}
        />
      </div>
      <LoginStep />
      <h3>
        Model pool{' '}
        <HistoryButton target={{ kind: 'setting', key: 'model_pool', name: 'Model pool' }} />
      </h3>
      {s.models.models.map(({ id }) => (
        <label key={id} className="switch-row">
          {id}
          <Switch
            checked={pool.includes(id)}
            disabled={disabled || (pool.length === 1 && pool.includes(id))}
            onCheckedChange={(on) =>
              save('model_pool', on ? [...pool, id] : pool.filter((m) => m !== id))
            }
          />
        </label>
      ))}
      <label className="field">
        Add model by full ID
        <Input
          disabled={disabled}
          placeholder="Full model ID"
          onKeyDown={(e) => {
            if (e.key === 'Enter') {
              const id = e.currentTarget.value.trim();
              if (id && !pool.includes(id)) {
                save('model_pool', [...pool, id]);
                e.currentTarget.value = '';
              }
            }
          }}
        />
      </label>
      <Button variant="outline" onClick={() => go(routes.company())}>
        Models by role
      </Button>
    </Card>
  );
  return (
    <>
      <Heading title="Settings" description="A workspace that feels like yours." />
      <DataError message={action.error} />
      <PageTabs
        value={tab}
        onChange={(value) => {
          go(routes.settings(value));
        }}
        items={[
          { id: 'appearance', label: 'Appearance', content: <Appearance /> },
          {
            id: 'accessibility',
            label: 'Accessibility',
            content: (
              <Card>
                <h2>Motion and effects</h2>
                <ChoiceSelect
                  disabled={disabled}
                  label="Motion and effects"
                  value={a.motionPreference}
                  options={['Follow system', 'On', 'Off']}
                  onChange={(value) => a.setMotionPreference(value)}
                />
                <HistoryButton
                  target={{ kind: 'setting', key: 'motion', name: 'Motion and effects' }}
                />
              </Card>
            ),
          },
          { id: 'preferences', label: 'Preferences', content: preferences },
          { id: 'models', label: 'Models', content: models },
          {
            id: 'updates',
            label: 'Updates',
            content: <Updates />,
          },
        ]}
      />
    </>
  );
}
