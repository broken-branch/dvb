import { routes } from '@/lib/routes';
import { useState } from 'react';
import { Plus } from 'lucide-react';
import { OrgChart, canReport } from '@/components/dvb/org-chart';
import { Card } from '@/components/dvb/card';
import { Heading, PageTabs, ChoiceSelect, Status } from '@/components/dvb/common';
import { DataError, useAction, useRead, Feed } from '@/components/dvb/data-feedback';
import { HistoryButton } from '@/components/dvb/history-sheet';
import { ProjectLabel } from '@/components/dvb/project-identity';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Textarea } from '@/components/ui/textarea';
import { Switch } from '@/components/ui/switch';
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogDescription,
} from '@/components/ui/dialog';
import { go, useStore } from '@/lib/store';
import { useEventPages } from '@/lib/data/pages';
import { roleFields } from '@/lib/data/roles';
import { stateLabels } from '@/lib/data/display';
import type { Role, RoleFields, RoleChanges, Hire } from '@/lib/data/types';
export function Company({
  profileId = '',
  projectId = '',
}: {
  profileId?: string;
  projectId?: string;
}) {
  const { data: s, canWrite } = useStore(),
    [view, setView] = useState('chart'),
    [adding, setAdding] = useState(false),
    [name, setName] = useState(''),
    action = useAction();
  const selectedProject = projectId || s.projects[0]?.id || '',
    roles = s.roles[selectedProject] || [],
    selected = roles.find((r) => r.id === profileId || r.slug === profileId);
  return (
    <>
      <Heading
        title="Company"
        description="Distinct strengths. Shared direction. A record of the work."
      >
        <ChoiceSelect
          label="Company project"
          value={selectedProject}
          onChange={(value) => go(routes.company(undefined, value))}
          options={s.projects.map((p) => ({ value: p.id, label: <ProjectLabel id={p.id} /> }))}
        />
        <Button disabled={!selectedProject || !canWrite} onClick={() => setAdding(true)}>
          <Plus size={15} />
          Add a role
        </Button>
      </Heading>
      <DataError message={action.error} />
      <PageTabs
        value={view}
        onChange={setView}
        items={[
          { id: 'chart', label: 'Org chart', content: <OrgChart project={selectedProject} /> },
          {
            id: 'cards',
            label: 'Cards',
            content: (
              <div className="company-grid">
                {roles.map((role) => {
                  const hire = s.hires.find((h) => h.role === role.id && h.status === 'active');
                  return (
                    <div className="role-card-item" key={role.id}>
                      <Card asChild>
                        <button
                          className="role-card"
                          onClick={() => go(routes.company(role.id, selectedProject))}
                        >
                          <div className="card-top">
                            <Status>{hire ? 'Hired' : 'Not hired'}</Status>
                          </div>
                          <h2>{role.name}</h2>
                          <div className="role-model-line">{hire?.model || 'Choose a model'}</div>
                          <p className="role-about">{role.job}</p>
                          <span className="role-footer">Edit profile →</span>
                        </button>
                      </Card>
                      <HistoryButton target={{ kind: 'role', id: role.id, name: role.name }} />
                    </div>
                  );
                })}
              </div>
            ),
          },
        ]}
      />
      {selected && (
        <RoleProfile key={selected.id} role={selected} roles={roles} project={selectedProject} />
      )}
      <Dialog open={adding} onOpenChange={setAdding}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Add a role</DialogTitle>
            <DialogDescription>
              Define its responsibility, then hire a model in its profile.
            </DialogDescription>
          </DialogHeader>
          <form
            onSubmit={(e) => {
              e.preventDefault();
              void action.act(
                () =>
                  s.addRole(selectedProject, {
                    name: name.trim(),
                    job: '',
                    lane: '',
                    lead: false,
                    reports_to: roles.find((r) => r.slug === 'orchestrator')?.id,
                    reviewed_by: null,
                    review_instructions: '',
                  }),
                (role) => {
                  setAdding(false);
                  setName('');
                  go(routes.company(role.id, selectedProject));
                },
              );
            }}
          >
            <label className="field">
              Role name
              <Input required value={name} onChange={(e) => setName(e.target.value)} />
            </label>
            <DataError message={action.error} />
            <Button disabled={action.busy || !name.trim()}>Add role</Button>
          </form>
        </DialogContent>
      </Dialog>
    </>
  );
}
function RoleProfile({ role, roles, project }: { role: Role; roles: Role[]; project: string }) {
  const { data: s, canWrite, settings } = useStore(),
    action = useAction(),
    [initial, setInitial] = useState<RoleFields>(() => roleFields(role)),
    [fields, setFields] = useState<RoleFields>(() => roleFields(role)),
    pool = settings.model_pool;
  const hire = s.hires.find((h) => h.role === role.id && h.status === 'active'),
    [model, setModel] = useState(hire?.model || pool[0] || ''),
    [effort, setEffort] = useState<Hire['effort']>(
      hire?.effort || s.models.models.find((m) => m.id === model)?.efforts[0] || 'medium',
    ),
    disabled = action.busy || !canWrite,
    efforts = s.models.models.find((m) => m.id === model)?.efforts || [];
  const work = s.tasks.filter((t) => t.project === project && t.role === role.id);
  const activity = useEventPages({ project, role: role.id, limit: 12 });
  const usage = useRead(() => s.usage({ project }), [s.usage, project, s.usageVersion]);
  const rows = usage.value?.rows.filter((r) => r.role === role.id) || [];
  return (
    <Dialog
      open
      onOpenChange={(open) => {
        if (!open) go(routes.company(undefined, project));
      }}
    >
      <DialogContent className="profile-sheet">
        <DialogHeader>
          <DialogTitle>{role.name}</DialogTitle>
          <DialogDescription>{role.job || 'Edit this role and its hire.'}</DialogDescription>
        </DialogHeader>
        <DataError message={action.error} />
        <form
          onSubmit={(e) => {
            e.preventDefault();
            const changed: RoleChanges = Object.fromEntries(
              Object.entries(fields).filter(
                ([key, value]) => value !== initial[key as keyof RoleFields],
              ),
            );
            if (!Object.keys(changed).length) return;
            void action.act(
              () => s.changeRole(role.id, changed),
              () => setInitial(fields),
            );
          }}
        >
          <label className="field">
            Role name
            <Input
              required
              value={fields.name}
              onChange={(e) => setFields({ ...fields, name: e.target.value })}
            />
          </label>
          <label className="field">
            Job
            <Textarea
              value={fields.job}
              onChange={(e) => setFields({ ...fields, job: e.target.value })}
            />
          </label>
          <label className="field">
            Decides
            <Input
              value={fields.lane}
              onChange={(e) => setFields({ ...fields, lane: e.target.value })}
            />
          </label>
          <label className="field">
            Reports to
            <ChoiceSelect
              label="Reports to"
              value={fields.reports_to || 'owner'}
              onChange={(value) =>
                setFields({ ...fields, reports_to: value === 'owner' ? null : value })
              }
              options={[
                { value: 'owner', label: 'Owner' },
                ...roles
                  .filter((r) => canReport(roles, role.id, r.id))
                  .map((r) => ({ value: r.id, label: r.name })),
              ]}
            />
          </label>
          <label className="field">
            Reviewed by
            <ChoiceSelect
              label="Reviewed by"
              value={fields.reviewed_by || 'area'}
              onChange={(value) =>
                setFields({ ...fields, reviewed_by: value === 'area' ? null : value })
              }
              options={[
                { value: 'area', label: 'Default reviewer for task area' },
                ...roles
                  .filter((r) => r.id !== role.id)
                  .map((r) => ({ value: r.id, label: r.name })),
              ]}
            />
          </label>
          <label className="field">
            Review instructions
            <Textarea
              value={fields.review_instructions}
              onChange={(e) => setFields({ ...fields, review_instructions: e.target.value })}
            />
          </label>
          <label className="switch-row">
            Lead adds a model run per task
            <Switch
              checked={fields.lead}
              onCheckedChange={(lead) => setFields({ ...fields, lead })}
            />
          </label>
          <Button disabled={disabled}>Save role</Button>
        </form>
        <section className="profile-section">
          <h2>Hire</h2>
          <ChoiceSelect
            label="Model"
            value={model}
            options={pool}
            onChange={(value) => {
              setModel(value);
              setEffort(s.models.models.find((m) => m.id === value)?.efforts[0] || 'medium');
            }}
          />
          <ChoiceSelect
            label="Effort"
            value={effort}
            options={efforts}
            onChange={(value) => setEffort(value)}
          />
          <div className="actions">
            <Button
              disabled={disabled || !model || !efforts.includes(effort)}
              onClick={() => void action.act(() => s.hire({ role: role.id, model, effort }))}
            >
              {hire ? 'Hire another model' : 'Hire model'}
            </Button>
            {hire && (
              <Button
                variant="outline"
                disabled={disabled || !model || !efforts.includes(effort)}
                onClick={() =>
                  void action.act(async () => {
                    const created = await s.hire({ role: role.id, model, effort });
                    if (!created.ok) return created;
                    for (const previous of s.hires.filter(
                      (h) => h.role === role.id && h.status === 'active',
                    )) {
                      const retired = await s.retire(previous.id);
                      if (!retired.ok) return retired;
                    }
                    return created;
                  })
                }
              >
                Use for new work
              </Button>
            )}
          </div>
          {s.hires
            .filter((h) => h.role === role.id)
            .map((h) => (
              <div key={h.id} className="actions">
                <span>
                  {h.model} {h.effort} {h.status}
                </span>
                <HistoryButton
                  target={{ kind: 'hire', id: h.id, name: `${role.name}: ${h.model}` }}
                />
                {h.status === 'active' && (
                  <Button
                    variant="outline"
                    disabled={disabled}
                    onClick={() => void action.act(() => s.retire(h.id))}
                  >
                    Retire {h.model}
                  </Button>
                )}
              </div>
            ))}
        </section>
        <section className="profile-section">
          <h2>Work in this project</h2>
          {work.length ? (
            work.map((t) => (
              <p key={t.id}>
                <a className="task-written-text" href={`#${routes.task(t.id)}`}>
                  {t.title}
                </a>{' '}
                {stateLabels[t.state.state]}
              </p>
            ))
          ) : (
            <p>No assignments yet.</p>
          )}
          <DataError message={activity.error} />
          <Feed events={activity.events.slice(-12)} />
        </section>
        <section className="profile-section">
          <h2>Usage in this project</h2>
          <DataError message={usage.error} />
          {rows.map((r) => (
            <p key={r.model}>
              {r.model}: {(r.uncached_in + r.cached_in + r.out).toLocaleString()} tokens $
              {r.est_cost.toFixed(2)} estimated equivalent
            </p>
          ))}
          {usage.value && !rows.length && <p>No usage reported yet.</p>}
          <a href={`#${routes.usage()}`}>All usage</a>
        </section>
      </DialogContent>
    </Dialog>
  );
}
