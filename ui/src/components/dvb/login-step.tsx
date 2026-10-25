import { useCallback, useEffect, useRef, useState } from 'react';
import { ListRow } from './card';
import { Status } from './common';
import { DataError } from './data-feedback';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { useStore } from '@/lib/store';
import { refusalText } from '@/lib/data/display';
import type { LoginStarted, Logins, Vendor } from '@/lib/data/types';
/**
 * Which vendors are signed in, as the engine reports it; `onChange` hears every read. The owner is
 * signed in once every vendor the engine lists as needed is, so an empty list needs none; an older
 * engine that sends no list needs both vendors.
 */
export function LoginStep({ onChange }: { onChange?: (signedIn: boolean) => void } = {}) {
  const { data: s } = useStore(),
    [logins, setLogins] = useState<Logins>();
  const read = useCallback(async () => {
    const result = await s.logins().catch(() => undefined);
    if (result?.ok) setLogins(result.value);
  }, [s.logins]);
  useEffect(() => {
    void read();
  }, [read]);
  const needed = logins?.needed;
  useEffect(() => {
    onChange?.(
      !!logins &&
        (needed
          ? needed.every((vendor) => vendor === 'fake' || logins[vendor])
          : logins.claude && logins.codex),
    );
  }, [logins, needed, onChange]);
  return (
    <section>
      <h2>Sign in to your subscriptions</h2>
      {(['claude', 'codex'] as const).map((vendor) => (
        <VendorLogin
          key={vendor}
          vendor={vendor}
          signedIn={!!logins?.[vendor]}
          optional={!!needed && !needed.includes(vendor)}
          onDone={read}
        />
      ))}
    </section>
  );
}
function VendorLogin({
  vendor,
  signedIn,
  optional,
  onDone,
}: {
  vendor: Vendor;
  signedIn: boolean;
  optional: boolean;
  onDone: () => void;
}) {
  const { data: s, canWrite } = useStore(),
    [started, setStarted] = useState<LoginStarted>(),
    [code, setCode] = useState(''),
    [busy, setBusy] = useState(false),
    [error, setError] = useState(''),
    [poll, setPoll] = useState(false);
  const alive = useRef(true);
  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
    };
  }, []);
  const send = useCallback(
    async (finish: boolean) => {
      setBusy(true);
      setError('');
      try {
        const result =
          finish && started
            ? await s.finishLogin(vendor, { run: started.run, code })
            : await s.startLogin(vendor);
        if (!alive.current) return;
        if (!result.ok) {
          setError(refusalText(result.refusal));
          setPoll(false);
          setStarted(undefined);
          setCode('');
          onDone();
          return;
        }
        setStarted(result.value);
        const loggedIn = result.value.status === 'logged_in',
          waiting = result.value.status === 'pending';
        if (loggedIn || (finish && !waiting)) {
          onDone();
          if (!loggedIn) {
            setStarted(undefined);
            setError(result.value.prompt);
          }
        }
        if (finish) {
          setCode('');
          setPoll(waiting);
        }
      } catch (e) {
        if (alive.current) {
          setError(e instanceof Error ? e.message : 'Unable to sign in.');
          setPoll(false);
        }
      } finally {
        if (alive.current) setBusy(false);
      }
    },
    [s.finishLogin, s.startLogin, vendor, started, code, onDone],
  );
  useEffect(() => {
    if (!poll || busy) return;
    const timer = setTimeout(() => {
      void send(true);
    }, 1000);
    return () => clearTimeout(timer);
  }, [poll, busy, send]);
  const done = signedIn || started?.status === 'logged_in',
    disabled = busy || !canWrite;
  // Only link to a vendor-supplied HTTPS URL; login codes stay in this component.
  const url = started?.url.startsWith('https://') ? started.url : '';
  return (
    <ListRow asChild>
      <div className="login-row">
        <div>
          <h3>{vendor === 'claude' ? 'Claude Code' : 'Codex'}</h3>
          {optional && <p>Optional. New projects' hires do not use it.</p>}
          <DataError message={error} />
          {started && <p role="status">{started.prompt}</p>}
          {url && !done && (
            <a href={url} target="_blank" rel="noreferrer">
              Open sign-in page
            </a>
          )}
          {started && !done && vendor === 'claude' && (
            <label className="field">
              Code from the sign-in page
              <Input
                autoComplete="off"
                value={code}
                onChange={(e) => setCode(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key !== 'Enter' || e.nativeEvent.isComposing) return;
                  e.preventDefault();
                  if (!disabled && !poll && code.trim()) void send(true);
                }}
              />
            </label>
          )}
        </div>
        {done ? (
          <Status tone="good">Signed in</Status>
        ) : (
          <Button
            type="button"
            variant="outline"
            disabled={disabled || poll || (!!started && vendor === 'claude' && !code.trim())}
            onClick={() => void send(!!started)}
          >
            {busy || poll ? 'Waiting for sign-in' : started ? 'Finish sign-in' : 'Sign in'}
          </Button>
        )}
      </div>
    </ListRow>
  );
}
