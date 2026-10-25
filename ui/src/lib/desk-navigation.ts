/** The desk sends routes separately from window focus, which can ignore a launch URL. */
export function connectDeskNavigation(
  navigate: (route: string) => void,
  create = (url: string) => new EventSource(url),
) {
  const events = create('/desk/events');
  const receive = (event: MessageEvent) => navigate(String(event.data));
  events.addEventListener('navigate', receive as EventListener);
  return () => events.close();
}
