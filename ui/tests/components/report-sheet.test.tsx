import { mount } from '../support/render';
import { describe, expect, it } from 'vitest';
import { screen } from '@testing-library/react';
import { createFixtureSource } from '../support/render';
import { ReportSheet } from '@/components/dvb/report-sheet';
describe('report sheet', () => {
  it('shows the exact title and body, then links to the returned address', async () => {
    const source = createFixtureSource();
    source.replies['POST /report'] = {
      title: '[bug] Board stuck',
      body: 'It never moves\n\ndvb 0.1.0',
      url: 'https://example.invalid/example-org/repo/issues/new?title=x',
    } satisfies import('@/api/schema').components['schemas']['ReportPreview'];
    const { user } = mount(<ReportSheet kind="bug" onClose={() => {}} />, source);
    // The padding and width rules in design-system.css target this slot and side.
    const sheet = screen.getByRole('dialog', { name: 'Report a bug' });
    expect(sheet).toHaveAttribute('data-slot', 'sheet-content');
    expect(sheet).toHaveAttribute('data-side', 'right');
    await user.click(screen.getByLabelText('Title'));
    await user.paste('Board stuck');
    await user.click(screen.getByLabelText('What happened?'));
    await user.paste('It never moves');
    await user.click(screen.getByRole('button', { name: 'Show what will be filed' }));
    expect(await screen.findByText('[bug] Board stuck')).toBeInTheDocument();
    expect(screen.getByText(/It never moves/)).toHaveTextContent('dvb 0.1.0');
    expect(screen.getByRole('link', { name: 'File it' })).toHaveAttribute(
      'href',
      'https://example.invalid/example-org/repo/issues/new?title=x',
    );
  });
  it('keeps the text and shows the refusal when the report is refused', async () => {
    const source = createFixtureSource();
    source.replies['POST /report'] = {
      code: 'NOT_IMPLEMENTED',
      detail: 'x',
    } satisfies import('@/api/schema').components['schemas']['Refusal'];
    source.statuses['POST /report'] = 501;
    const { user } = mount(<ReportSheet kind="feature" onClose={() => {}} />, source);
    await user.click(screen.getByLabelText('Title'));
    await user.paste('Dark mode');
    await user.click(screen.getByLabelText('What would help?'));
    await user.paste('Please');
    await user.click(screen.getByRole('button', { name: 'Show what will be filed' }));
    expect(await screen.findByRole('alert')).toBeInTheDocument();
    expect(screen.getByLabelText('Title')).toHaveValue('Dark mode');
  });
});
