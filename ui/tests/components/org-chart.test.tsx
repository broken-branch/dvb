import { describe, it, expect, vi } from 'vitest';
import { act, fireEvent, render, screen } from '@testing-library/react';
import { AppearanceProvider } from '@/lib/appearance';
import { StoreProvider } from '@/lib/store';
import { TooltipProvider } from '@/components/ui/tooltip';
import { createFixtureSource } from '../support/render';
import { chartSnapshots } from '../support/engine-fixtures';
import { OrgChart, chartLevels, canReport } from '@/components/dvb/org-chart';
describe('interaction', () => {
  function mount() {
    // React's task and microtask scheduling must keep running while frames advance.
    vi.useFakeTimers({
      toFake: [
        'Date',
        'performance',
        'setTimeout',
        'clearTimeout',
        'requestAnimationFrame',
        'cancelAnimationFrame',
      ],
    });
    const source = createFixtureSource(),
      roles = source.getSnapshot().roles.harbour;
    source.set({
      settings: { values: { ...source.getSnapshot().settings.values, motion: 'Off' } },
    });
    const rect = (x: number, y: number, width = 100, height = 100) => ({
      x,
      y,
      left: x,
      top: y,
      right: x + width,
      bottom: y + height,
      width,
      height,
      toJSON() {},
    });
    vi.spyOn(HTMLElement.prototype, 'getBoundingClientRect').mockImplementation(function (
      this: HTMLElement,
    ) {
      const target = this.closest<HTMLElement>('[data-org-target]')?.dataset.orgTarget;
      const id = this.dataset.orgRole || target?.replace(/^(under|before|after|group)-/, '');
      const index = roles.findIndex((r) => r.id === id);
      if (target === 'detach') return rect(100, 700);
      if (target === 'ungroup') return rect(300, 700);
      if (index < 0) return rect(0, 0, 2000, 1000);
      const x = 150 + index * 200,
        y = index === 0 ? 50 : 300;
      if (target?.startsWith('before-')) return rect(x - 30, y, 20);
      if (target?.startsWith('after-')) return rect(x + 110, y, 20);
      if (target?.startsWith('group-')) return rect(x, y + 120);
      return rect(x, y);
    });
    class Pointer extends MouseEvent {
      pointerId = 1;
      isPrimary = true;
      pointerType = 'mouse';
    }
    vi.stubGlobal('PointerEvent', Pointer);
    const view = render(
      <StoreProvider source={source}>
        <AppearanceProvider>
          <TooltipProvider>
            <OrgChart project="harbour" />
          </TooltipProvider>
        </AppearanceProvider>
      </StoreProvider>,
    );
    return { ...view, source, roles };
  }
  const tick = () =>
    act(async () => {
      vi.advanceTimersByTime(30);
    });
  async function pickup(name: string) {
    const button = screen.getByLabelText(new RegExp(`^Move ${name}\\.`), { selector: 'button' });
    button.focus();
    fireEvent.keyDown(button, { code: 'Space', key: ' ' });
    await tick();
    expect(screen.getByLabelText('Chart move', { selector: '[role="status"]' })).toHaveTextContent(
      `Picked up ${name}`,
    );
    for (const target of document.querySelectorAll('[data-org-target][aria-label]'))
      expect(target).toHaveAttribute('role', 'group');
  }
  async function keyboardTarget(label: string) {
    for (let i = 0; i < 60; i++) {
      fireEvent.keyDown(document, { code: 'ArrowRight', key: 'ArrowRight' });
      await tick();
      if (
        screen.getByLabelText('Chart move', { selector: '[role="status"]' }).textContent === label
      )
        return;
    }
    throw new Error(
      `Keyboard never reached ${label}; last: ${screen.getByLabelText('Chart move', { selector: '[role="status"]' }).textContent}`,
    );
  }
  async function keyboardDrop() {
    fireEvent.keyDown(document, { code: 'Space', key: ' ' });
    await tick();
  }
  async function pointerPickup(name: string) {
    const button = screen.getByLabelText(new RegExp(`^Move ${name}\\.`), { selector: 'button' }),
      bounds = button.getBoundingClientRect();
    fireEvent.pointerDown(button, {
      clientX: bounds.x + 50,
      clientY: bounds.y + 50,
      button: 0,
      buttons: 1,
      pointerId: 1,
    });
    fireEvent.pointerMove(document, {
      clientX: bounds.x + 65,
      clientY: bounds.y + 50,
      buttons: 1,
      pointerId: 1,
    });
    await tick();
    expect(screen.getByLabelText('Chart move', { selector: '[role="status"]' })).toHaveTextContent(
      `Picked up ${name}`,
    );
  }
  async function pointerDrop(target?: HTMLElement) {
    const bounds = target?.getBoundingClientRect(),
      clientX = bounds ? bounds.x + bounds.width / 2 : 1990,
      clientY = bounds ? bounds.y + bounds.height / 2 : 990;
    fireEvent.pointerMove(document, { clientX, clientY, buttons: 1, pointerId: 1 });
    await tick();
    fireEvent.pointerUp(document, { clientX, clientY, button: 0, pointerId: 1 });
    await tick();
  }
  function target(id: string) {
    return document.querySelector<HTMLElement>(`[data-org-target="${id}"]`)!;
  }
  describe('org chart destinations', () => {
    it('Space pickup then drop and pointer empty-space drops make no API write', async () => {
      const { source } = mount(),
        move = vi.spyOn(source.getSnapshot(), 'moveChart');
      expect(document.querySelectorAll('[data-org-target]')).toHaveLength(0);
      await pickup('Backend builder');
      await keyboardDrop();
      expect(move).not.toHaveBeenCalled();
      expect(
        screen.getByLabelText('Chart move', { selector: '[role="status"]' }),
      ).toHaveTextContent('No changes');
      await pointerPickup('Backend builder');
      await pointerDrop();
      expect(move).not.toHaveBeenCalled();
      expect(
        screen.getByLabelText('Chart move', { selector: '[role="status"]' }),
      ).toHaveTextContent('No changes');
    });
    it('pointer reports, detaches and groups using named destinations', async () => {
      const { source, roles } = mount(),
        builder = roles.find((r) => r.slug === 'backend')!,
        architect = roles.find((r) => r.slug === 'architect')!;
      source.replies['POST /projects/harbour/chart'] = chartSnapshots.reported;
      const move = vi.spyOn(source.getSnapshot(), 'moveChart');
      await pointerPickup(builder.name);
      await pointerDrop(target(`under-${architect.id}`));
      expect(move).toHaveBeenLastCalledWith('harbour', {
        kind: 'report',
        role: builder.id,
        parent: architect.id,
      });
      act(() => source.set({ roles: { harbour: structuredClone(chartSnapshots.reported) } }));
      expect(source.getSnapshot().roles.harbour.find((r) => r.id === builder.id)?.reports_to).toBe(
        architect.id,
      );
      expect(
        screen.getByLabelText('Chart move', { selector: '[role="status"]' }),
      ).toHaveTextContent(`Completed: Report ${builder.name} to ${architect.name}`);
      await pointerPickup(builder.name);
      source.replies['POST /projects/harbour/chart'] = roles;
      await pointerDrop(target('detach'));
      expect(move).toHaveBeenLastCalledWith('harbour', {
        kind: 'report',
        role: builder.id,
        parent: roles[0].id,
      });
      act(() => source.set({ roles: { harbour: roles } }));
      expect(source.getSnapshot().roles.harbour.find((r) => r.id === builder.id)?.reports_to).toBe(
        roles[0].id,
      );
      await pointerPickup(builder.name);
      source.replies['POST /projects/harbour/chart'] = chartSnapshots.grouped;
      await pointerDrop(target(`group-${architect.id}`));
      expect(move).toHaveBeenLastCalledWith('harbour', {
        kind: 'group',
        roles: [architect.id, builder.id],
        group: 'Architect team',
      });
      act(() => source.set({ roles: { harbour: structuredClone(chartSnapshots.grouped) } }));
      expect(
        source
          .getSnapshot()
          .roles.harbour.filter((r) => [builder.id, architect.id].includes(r.id))
          .every((r) => r.group === `${architect.name} team`),
      ).toBe(true);
    });
    it('keyboard swap reverses by dragging the former top role', async () => {
      const { source, roles } = mount(),
        root = roles[0],
        builder = roles.find((r) => r.slug === 'backend')!;
      source.replies['POST /projects/harbour/chart'] = chartSnapshots.swapped;
      const move = vi.spyOn(source.getSnapshot(), 'moveChart');
      await pickup(builder.name);
      await keyboardTarget(`Swap ${builder.name} with ${root.name}`);
      await keyboardDrop();
      expect(move).toHaveBeenLastCalledWith('harbour', {
        kind: 'swap',
        role: builder.id,
        lead: root.id,
      });
      act(() => source.set({ roles: { harbour: structuredClone(chartSnapshots.swapped) } }));
      expect(
        source.getSnapshot().roles.harbour.find((r) => r.id === builder.id)?.reports_to,
      ).toBeNull();
      await pickup(root.name);
      await keyboardTarget(`Swap ${root.name} with ${builder.name}`);
      source.replies['POST /projects/harbour/chart'] = roles;
      await keyboardDrop();
      expect(move).toHaveBeenLastCalledWith('harbour', {
        kind: 'swap',
        role: root.id,
        lead: builder.id,
      });
      act(() => source.set({ roles: { harbour: roles } }));
      expect(source.getSnapshot().roles.harbour).toEqual(roles);
    });
    it('keyboard before and pointer after use persisted order across groups immediately and on reload', async () => {
      const { source, roles, unmount } = mount(),
        builder = roles.find((r) => r.slug === 'backend')!,
        architect = roles.find((r) => r.slug === 'architect')!;
      act(() =>
        source.set({
          roles: {
            harbour: roles.map((r) => ({
              ...r,
              group: r.id === builder.id ? 'Zebra' : r.id === architect.id ? 'Alpha' : null,
            })),
          },
        }),
      );
      source.replies['POST /projects/harbour/chart'] = chartSnapshots.before;
      const move = vi.spyOn(source.getSnapshot(), 'moveChart');
      await pickup(builder.name);
      await keyboardTarget(`Place ${builder.name} before ${architect.name}`);
      await keyboardDrop();
      expect(move).toHaveBeenLastCalledWith('harbour', {
        kind: 'reorder',
        role: builder.id,
        before: architect.id,
      });
      act(() => source.set({ roles: { harbour: structuredClone(chartSnapshots.before) } }));
      let ordered = chartLevels(source.getSnapshot().roles.harbour)[1].map((r) => r.id);
      expect(ordered.indexOf(builder.id) + 1).toBe(ordered.indexOf(architect.id));
      await pointerPickup(builder.name);
      source.replies['POST /projects/harbour/chart'] = chartSnapshots.after;
      await pointerDrop(target(`after-${architect.id}`));
      expect(move).toHaveBeenLastCalledWith('harbour', {
        kind: 'reorder',
        role: builder.id,
        before: 'harbour-frontend',
      });
      act(() => source.set({ roles: { harbour: structuredClone(chartSnapshots.after) } }));
      const saved = source.getSnapshot().roles.harbour;
      ordered = chartLevels(saved)[1].map((r) => r.id);
      expect(ordered.indexOf(architect.id) + 1).toBe(ordered.indexOf(builder.id));
      expect(saved.find((r) => r.id === builder.id)?.group).toBe('Zebra');
      unmount();
      const reloaded = createFixtureSource();
      reloaded.set({ roles: { harbour: JSON.parse(JSON.stringify(saved)) } });
      render(
        <StoreProvider source={reloaded}>
          <AppearanceProvider>
            <TooltipProvider>
              <OrgChart project="harbour" />
            </TooltipProvider>
          </AppearanceProvider>
        </StoreProvider>,
      );
      expect(
        Array.from(document.querySelectorAll('[data-org-level="1"] [data-org-role]')).map((el) =>
          el.getAttribute('data-org-role'),
        ),
      ).toEqual(ordered);
    });
    it('targets forbid cycles and foreign siblings while enabling the own lead swap', async () => {
      const { source, roles } = mount(),
        builder = roles.find((r) => r.slug === 'backend')!,
        architect = roles.find((r) => r.slug === 'architect')!;
      act(() =>
        source.set({
          roles: {
            harbour: roles.map((r) =>
              r.id === architect.id ? { ...r, reports_to: builder.id } : r,
            ),
          },
        }),
      );
      await pickup(builder.name);
      for (const kind of ['under', 'before', 'group'])
        expect(target(`${kind}-${architect.id}`)).toHaveAttribute('aria-disabled', 'true');
      expect(target(`under-${roles[0].id}`)).toHaveAttribute('aria-disabled', 'false');
      fireEvent.keyDown(document, { code: 'Escape', key: 'Escape' });
      await tick();
      expect(
        screen.getByLabelText('Chart move', { selector: '[role="status"]' }),
      ).toHaveTextContent(`Move cancelled for ${builder.name}. No changes.`);
    });
    it('rejected and cancelled moves are named and never announced as completed', async () => {
      const { source, roles } = mount(),
        builder = roles.find((r) => r.slug === 'backend')!,
        architect = roles.find((r) => r.slug === 'architect')!;
      const move = vi
        .fn()
        .mockResolvedValue({ ok: false, refusal: { code: 'ROLE_NOT_ALLOWED', detail: 'Changed' } });
      act(() => source.set({ moveChart: move }));
      await pickup(builder.name);
      await keyboardTarget(`Report ${builder.name} to ${architect.name}`);
      await keyboardDrop();
      expect(
        screen.getByLabelText('Chart move', { selector: '[role="status"]' }),
      ).toHaveTextContent(`Move rejected for ${builder.name}. No changes.`);
      expect(
        screen.getByLabelText('Chart move', { selector: '[role="status"]' }),
      ).not.toHaveTextContent('Completed');
      expect(
        screen.getByLabelText('Chart move', { selector: '[role="status"]' }),
      ).not.toHaveTextContent(builder.id);
      await pickup(builder.name);
      fireEvent.keyDown(document, { code: 'Escape', key: 'Escape' });
      await tick();
      expect(move).toHaveBeenCalledTimes(1);
      expect(
        screen.getByLabelText('Chart move', { selector: '[role="status"]' }),
      ).toHaveTextContent('Move cancelled');
    });
  });
});
describe('opening position', () => {
  function open(regionWidth: number, canvasWidth: number) {
    const source = createFixtureSource(),
      layout = { region: regionWidth, canvas: canvasWidth, owner: (canvasWidth - 260) / 2 },
      observers: ((entries: never[]) => void)[] = [],
      rect = (left: number, width: number) =>
        ({ left, width, right: left + width, top: 0, bottom: 100, height: 100 }) as DOMRect;
    vi.stubGlobal(
      'ResizeObserver',
      class {
        callback: (entries: never[]) => void;
        constructor(callback: (entries: never[]) => void) {
          this.callback = callback;
          observers.push(callback);
        }
        observe() {}
        disconnect() {
          observers.splice(observers.indexOf(this.callback), 1);
        }
      },
    );
    vi.spyOn(Element.prototype, 'clientWidth', 'get').mockImplementation(function (this: Element) {
      return this.classList.contains('org-chart') ? layout.region : 0;
    });
    vi.spyOn(Element.prototype, 'scrollWidth', 'get').mockImplementation(function (this: Element) {
      return this.classList.contains('org-chart') ? layout.canvas : 0;
    });
    // jsdom does not lay out: keep the region's scrollLeft within its overflow like a browser.
    let scrollLeft = 0;
    vi.spyOn(Element.prototype, 'scrollLeft', 'get').mockImplementation(() => scrollLeft);
    vi.spyOn(Element.prototype, 'scrollLeft', 'set').mockImplementation((value: number) => {
      scrollLeft = Math.max(0, Math.min(value, layout.canvas - layout.region));
    });
    vi.spyOn(HTMLElement.prototype, 'getBoundingClientRect').mockImplementation(function (
      this: HTMLElement,
    ) {
      const scroll = scrollLeft;
      if (this.classList.contains('org-chart')) return rect(0, layout.region);
      if (this.hasAttribute('data-org-owner')) return rect(layout.owner - scroll, 260);
      return rect(-scroll, layout.canvas);
    });
    const view = render(
      <StoreProvider source={source}>
        <AppearanceProvider>
          <TooltipProvider>
            <OrgChart project="harbour" />
          </TooltipProvider>
        </AppearanceProvider>
      </StoreProvider>,
    );
    const region = screen.getByRole('region', { name: 'Company reporting chart' });
    const resize = (next: Partial<typeof layout>) => {
      Object.assign(layout, next);
      act(() => observers.forEach((callback) => callback([])));
    };
    return { ...view, source, region, layout, resize };
  }
  const ownerCentre = (region: HTMLElement, layout: { owner: number }) =>
    layout.owner + 130 - region.scrollLeft;
  it('a chart wider than its region opens with the top node centred, once', () => {
    const { region, source } = open(390, 2634);
    expect(region.scrollLeft).toBe(2634 / 2 - 390 / 2);
    region.scrollLeft = 0;
    fireEvent.scroll(region);
    const roles = source.getSnapshot().roles.harbour;
    act(() => source.set({ roles: { harbour: [...roles] } }));
    expect(region.scrollLeft).toBe(0);
  });
  it('centres the Owner card at phone width after layout settles', () => {
    const { region, layout, resize } = open(348, 900);
    // Cards and connectors widen the canvas after the first measurement.
    resize({ canvas: 1744, owner: 742 });
    expect(ownerCentre(region, layout)).toBe(348 / 2);
    resize({ owner: 700 });
    expect(ownerCentre(region, layout)).toBe(348 / 2);
    resize({ region: 390 });
    expect(ownerCentre(region, layout)).toBe(390 / 2);
  });
  it.each([
    ['scrolls', (region: HTMLElement) => ((region.scrollLeft = 10), fireEvent.scroll(region))],
    ['drags', (region: HTMLElement) => fireEvent.pointerDown(region)],
    ['uses the keyboard', (region: HTMLElement) => fireEvent.keyDown(region, { key: 'ArrowLeft' })],
  ])('a later resize does not move the scroll after the owner %s', (_, interact) => {
    const { region, resize } = open(390, 1744);
    interact(region);
    const kept = region.scrollLeft;
    resize({ canvas: 2000, owner: 870 });
    expect(region.scrollLeft).toBe(kept);
  });
  it('a chart that fits is not scrolled', () => {
    const { region, resize } = open(1200, 1200);
    expect(region.scrollLeft).toBe(0);
    resize({ canvas: 1100, owner: 420 });
    expect(region.scrollLeft).toBe(0);
  });
});
describe('levels', () => {
  it('keeps levels together and forbids reporting cycles', () => {
    const roles = createFixtureSource().getSnapshot().roles.harbour;
    expect(chartLevels(roles).map((level) => level.length)).toEqual([1, 6]);
    expect(canReport(roles, roles[0].id, roles[1].id)).toBe(false);
    expect(canReport(roles, roles[1].id, roles[2].id)).toBe(true);
    const nested = roles.map((r) => (r.id === roles[3].id ? { ...r, reports_to: roles[2].id } : r));
    expect(chartLevels(nested).map((level) => level.length)).toEqual([1, 5, 1]);
  });
});
