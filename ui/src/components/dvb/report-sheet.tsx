import { useState } from 'react';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Textarea } from '@/components/ui/textarea';
import {
  Sheet,
  SheetContent,
  SheetDescription,
  SheetHeader,
  SheetTitle,
} from '@/components/ui/sheet';
import { DataError, useAction } from './data-feedback';
import { returnFocus } from './workspace';
import { useStore } from '@/lib/store';
import type { ReportKind, ReportPreview } from '@/lib/data/types';

/** Report a bug or suggest a feature: shows the exact issue text, then opens GitHub to file it. */
export function ReportSheet({ kind, onClose }: { kind: ReportKind | null; onClose: () => void }) {
  const { data: s, canWrite } = useStore(),
    action = useAction(),
    [title, setTitle] = useState(''),
    [text, setText] = useState(''),
    [preview, setPreview] = useState<ReportPreview>();
  const close = () => {
    setTitle('');
    setText('');
    setPreview(undefined);
    onClose();
  };
  const url = preview?.url.startsWith('https://') ? preview.url : '';
  return (
    <Sheet
      open={kind !== null}
      onOpenChange={(open) => {
        if (!open) close();
      }}
    >
      <SheetContent className="report-sheet" onCloseAutoFocus={returnFocus}>
        <SheetHeader>
          <SheetTitle>{kind === 'bug' ? 'Report a bug' : 'Suggest a feature'}</SheetTitle>
          <SheetDescription>
            {preview
              ? 'This is exactly what will be filed.'
              : 'Describe it in your own words. You see the result before anything is sent.'}
          </SheetDescription>
        </SheetHeader>
        {preview ? (
          <div className="report-preview">
            <h3>{preview.title}</h3>
            <pre>{preview.body}</pre>
            <div className="setup-actions">
              <Button type="button" variant="ghost" onClick={() => setPreview(undefined)}>
                Edit
              </Button>
              {url ? (
                <Button asChild>
                  <a href={url} target="_blank" rel="noreferrer" onClick={close}>
                    File it
                  </a>
                </Button>
              ) : (
                <Button type="button" disabled>
                  File it
                </Button>
              )}
            </div>
          </div>
        ) : (
          <form
            onSubmit={(e) => {
              e.preventDefault();
              if (kind) void action.act(() => s.report({ kind, title, text }), setPreview);
            }}
          >
            <label className="field">
              Title
              <Input required value={title} onChange={(e) => setTitle(e.target.value)} />
            </label>
            <label className="field">
              {kind === 'bug' ? 'What happened?' : 'What would help?'}
              <Textarea required rows={6} value={text} onChange={(e) => setText(e.target.value)} />
            </label>
            <DataError message={action.error} />
            <Button disabled={action.busy || !title.trim() || !text.trim() || !canWrite}>
              Show what will be filed
            </Button>
          </form>
        )}
      </SheetContent>
    </Sheet>
  );
}
