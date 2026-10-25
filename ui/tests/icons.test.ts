import { readFileSync } from 'node:fs';
import path from 'node:path';
import { describe, expect, it } from 'vitest';

const file = (name: string) => readFileSync(path.resolve(import.meta.dirname, '..', name));
const pngSize = (bytes: Buffer) => [bytes.readUInt32BE(16), bytes.readUInt32BE(20)];

describe('app icon', () => {
  it('links the favicon from index.html to files in public/icons', () => {
    const page = new DOMParser().parseFromString(file('index.html').toString(), 'text/html');
    const links = Array.from(page.querySelectorAll<HTMLLinkElement>('link[rel="icon"]'));
    expect(links.map((link) => link.getAttribute('href'))).toEqual([
      '/icons/dvb.svg',
      '/icons/dvb-16.png',
      '/icons/dvb-32.png',
      '/icons/dvb-256.png',
    ]);
    for (const link of links) {
      const bytes = file(`public${link.getAttribute('href')}`),
        sizes = link.getAttribute('sizes');
      if (link.type === 'image/svg+xml') expect(bytes.toString()).toContain('<svg');
      else expect(pngSize(bytes).join('x')).toBe(sizes);
    }
  });

  it('ships every installed size', () => {
    for (const size of [16, 32, 48, 64, 128, 256, 512])
      expect(pngSize(file(`public/icons/dvb-${size}.png`))).toEqual([size, size]);
  });
});
