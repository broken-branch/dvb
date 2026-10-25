import { it, expect, describe } from 'vitest';
import { render, screen, within } from '@testing-library/react';
import App from '@/App';
import { createFixtureSource } from './support/render';
describe('engine app', () => {
  it('opens fixed engine projections through the complete app shell', async () => {
    history.replaceState({}, '', '/#home');
    render(<App source={createFixtureSource()} />);
    expect(await screen.findByText('Working now', { selector: '.page h2' })).toBeInTheDocument();
    expect(
      within(document.getElementById('needs-you')!).getByRole('link', {
        name: /Import one catalogue/,
      }),
    ).toBeInTheDocument();
    history.replaceState({}, '', '/');
  });
});
describe('version', () => {
  it('shows the version banner in the app and keeps displayed work readable', () => {
    history.replaceState({}, '', '/#decision/local');
    const source = createFixtureSource();
    source.set({ connection: { state: 'version-mismatch', engineVersion: '2', appVersion: '1' } });
    render(<App source={source} />);
    expect(screen.getByRole('alert')).toHaveTextContent('Update dvb before making changes');
    expect(screen.getByRole('alert').closest('.page')).not.toBeNull();
    expect(screen.getByRole('button', { name: 'Send answer' })).toBeDisabled();
    history.replaceState({}, '', '/');
  });
});
describe('engine connection notices', () => {
  const raw =
    'The engine is not reachable: /run/user/1000/dvbgui/engine.sock: Connection refused (os error 111)';
  it('headlines reconnecting for the owner and keeps the raw error as detail inside the page', () => {
    history.replaceState({}, '', '/#home');
    const source = createFixtureSource();
    source.set({ connection: { state: 'reconnecting' } });
    source.reportIssue(raw);
    render(<App source={source} />);
    const notice = screen.getByRole('status');
    expect(notice.closest('.page')).not.toBeNull();
    expect(within(notice).getByText("dvb's engine isn't running. Reconnecting…").tagName).toBe(
      'STRONG',
    );
    expect(notice).toHaveTextContent('Displayed work is the last known state.');
    expect(within(notice).getByText(raw).tagName).toBe('SMALL');
    expect(screen.queryByRole('alert')).toBeNull();
    history.replaceState({}, '', '/');
  });
  it('shows an error on its own inside the page when connected', () => {
    history.replaceState({}, '', '/#home');
    const source = createFixtureSource();
    source.reportIssue('Unable to refresh state.');
    render(<App source={source} />);
    const alert = screen.getByRole('alert');
    expect(alert).toHaveTextContent('Unable to refresh state.');
    expect(alert.closest('.page')).not.toBeNull();
    expect(screen.queryByText(/Reconnecting/)).toBeNull();
    history.replaceState({}, '', '/');
  });
});
