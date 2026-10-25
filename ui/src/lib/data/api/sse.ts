/** Incremental SSE framing: chunks may split UTF-8, CRLF, ids and multi-line data. */
export async function readEvents(
  body: ReadableStream<Uint8Array>,
  receive: (data: string, id?: string) => void,
  live: () => void = () => {},
) {
  const reader = body.getReader(),
    decoder = new TextDecoder();
  let buffer = '',
    data: string[] = [],
    id: string | undefined;
  const line = (text: string) => {
    if (!text) {
      if (data.length) receive(data.join('\n'), id);
      data = [];
      id = undefined;
      return;
    }
    if (text.startsWith(':')) {
      if (text === ': live') live();
      return;
    }
    const colon = text.indexOf(':'),
      field = colon < 0 ? text : text.slice(0, colon);
    const value = colon < 0 ? '' : text.slice(colon + 1).replace(/^ /, '');
    if (field === 'data') data.push(value);
    if (field === 'id' && !value.includes('\0')) id = value;
  };
  try {
    while (true) {
      const { done, value } = await reader.read();
      buffer += decoder.decode(value, { stream: !done });
      let match: RegExpExecArray | null;
      while ((match = /\r\n|\r|\n/.exec(buffer))) {
        // A trailing CR may be the first half of CRLF in the next chunk.
        if (!done && match[0] === '\r' && match.index === buffer.length - 1) break;
        line(buffer.slice(0, match.index));
        buffer = buffer.slice(match.index + match[0].length);
      }
      if (done) break; // Incomplete events are replayed on reconnect.
    }
  } finally {
    await reader.cancel().catch(() => {});
    reader.releaseLock();
  }
}
