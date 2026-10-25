import { useAppReducedMotion } from '@/lib/appearance';

import { useEffect, useId, useRef } from 'react';
const xs = [52, 68, 84, 100, 116, 132, 148];
const heights = [12, 29, 42, 50, 42, 29, 12];
const letters = [
  'M73 51V92C73 103 65 112 54 112C43 112 35 103 35 92C35 81 43 73 54 73C65 73 73 81 73 92',
  'M87 77L97 106Q100 115 103 106L113 77',
  'M127 51V92C127 103 135 112 146 112C157 112 165 103 165 92C165 81 157 73 146 73C135 73 127 81 127 92',
];
export function Logo({ active, attention }: { active: boolean; attention: boolean }) {
  const reduced = useAppReducedMotion(),
    id = useId().replaceAll(':', ''),
    svg = useRef<SVGSVGElement>(null);
  const state = attention ? 'Needs you' : active ? 'Working' : 'Idle';
  useEffect(() => {
    const mark = svg.current,
      link = mark?.closest('a');
    if (!mark || !link) return;
    const bars = Array.from(mark.querySelectorAll<SVGRectElement>('.logo-meter rect'));
    const spot = mark.querySelector(`#${id}-spot`),
      reflection = mark.querySelector(`#${id}-reflection`),
      back = mark.querySelector('.logo-relief');
    let frame = 0,
      lastTime = 0,
      x = 0.2,
      y = -0.2,
      pointer = false,
      focused = false,
      touched = false,
      engaged = false;
    const bottoms = xs.map((x) => 100 + Math.sqrt(76 ** 2 - (x - 100) ** 2));
    const updateBars = (time: number, blend: number) =>
      bars.forEach((bar, index) => {
        const phase = index * 1.19,
          t = time / 1000;
        const signal =
          0.77 +
          0.13 * Math.sin(t * (1.37 + index * 0.09) + phase) +
          0.09 * Math.sin(t * 0.79 - phase * 0.73);
        const idle = 0.92 + 0.045 * Math.sin(t * 0.65 + phase * 0.5);
        let target = Math.max(9, heights[index] * (active ? signal : idle));
        if (engaged) {
          const distance = (xs[index] - (100 + x * 58)) / 22,
            crest = Math.exp((-distance * distance) / 2);
          const ceiling = [31, 45, 50, 54, 50, 45, 31][index],
            floor = Math.max(9, heights[index] * 0.52);
          target = floor + (ceiling - floor) * crest * (0.9 - y * 0.1);
        }
        const height =
          Number(bar.getAttribute('height')) +
          (target - Number(bar.getAttribute('height'))) * blend;
        bar.setAttribute('height', height.toFixed(3));
        bar.setAttribute('y', (bottoms[index] - height).toFixed(3));
      });
    const draw = () => {
      mark.style.transform = `perspective(600px) rotateX(${-y * 14 + 5}deg) rotateY(${x * 18 - 5}deg) scale(1.035)`;
      back?.setAttribute('transform', `translate(${-x * 5 + 2} ${-y * 5 + 4})`);
      spot?.setAttribute('cx', String(100 + x * 57));
      spot?.setAttribute('cy', String(141 + y * 10));
      reflection?.setAttribute('cx', String(100 + x * 80));
      reflection?.setAttribute('cy', String(80 + y * 65));
      updateBars(performance.now(), 1);
    };
    const engage = () => {
      engaged = !reduced && !document.hidden && (pointer || focused || touched);
      mark.classList.toggle('is-engaged', engaged);
      if (engaged) {
        mark.classList.add('has-engaged');
        draw();
      } else mark.style.transform = '';
    };
    const position = (event: PointerEvent) => {
      const bounds = link.getBoundingClientRect();
      x = Math.max(-1, Math.min(1, ((event.clientX - bounds.left) / bounds.width) * 2 - 1));
      y = Math.max(-1, Math.min(1, ((event.clientY - bounds.top) / bounds.height) * 2 - 1));
    };
    const enter = (event: PointerEvent) => {
      if (event.pointerType === 'touch') return;
      position(event);
      pointer = true;
      engage();
    };
    const move = (event: PointerEvent) => {
      position(event);
      if (engaged) draw();
    };
    const leave = () => {
      pointer = false;
      engage();
    };
    const cancel = () => {
      pointer = touched = false;
      engage();
    };
    const touch = (event: PointerEvent) => {
      if (event.pointerType !== 'touch') return;
      position(event);
      touched = !touched;
      engage();
    };
    const focus = () => {
      focused = link.matches(':focus-visible');
      if (focused) {
        x = 0.2;
        y = -0.2;
      }
      engage();
    };
    const blur = () => {
      focused = touched = false;
      engage();
    };
    const key = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        pointer = focused = touched = false;
        engage();
      }
    };
    const animate = (time: number) => {
      const dt = Math.min((time - (lastTime || time)) / 1000, 0.05);
      lastTime = time;
      updateBars(time, 1 - Math.exp(-dt / 0.19));
      frame = requestAnimationFrame(animate);
    };
    const start = () => {
      cancelAnimationFrame(frame);
      if (document.hidden) {
        pointer = focused = touched = false;
        engage();
      }
      if (!reduced && !document.hidden) {
        lastTime = 0;
        frame = requestAnimationFrame(animate);
      }
    };
    if (reduced)
      bars.forEach((bar, i) => {
        bar.setAttribute('height', String(heights[i]));
        bar.setAttribute('y', String(bottoms[i] - heights[i]));
      });
    const events = {
      pointerenter: enter,
      pointermove: move,
      pointerleave: leave,
      pointercancel: cancel,
      pointerup: touch,
      focus,
      blur,
      keydown: key,
    };
    for (const [name, listener] of Object.entries(events))
      link.addEventListener(name, listener as EventListener);
    if (!reduced && link.matches(':hover,:focus-visible')) {
      pointer = link.matches(':hover');
      focused = link.matches(':focus-visible');
      engage();
    }
    start();
    document.addEventListener('visibilitychange', start);
    return () => {
      cancelAnimationFrame(frame);
      document.removeEventListener('visibilitychange', start);
      for (const [name, listener] of Object.entries(events))
        link.removeEventListener(name, listener as EventListener);
      mark.classList.remove('is-engaged');
      mark.style.transform = '';
    };
  }, [active, reduced, id]);
  return (
    <svg
      ref={svg}
      className={`dvb-logo inner-arc ${attention ? 'needs-attention' : ''}`}
      viewBox="0 0 200 200"
      role="img"
      aria-label={`dvb, ${state}`}
    >
      <defs>
        <linearGradient id={id} gradientUnits="userSpaceOnUse" x1="22" y1="50" x2="180" y2="150">
          <stop stopColor="var(--logo-start)" />
          <stop offset="1" stopColor="var(--logo-end)" />
        </linearGradient>
        <radialGradient id={`${id}-spot`} gradientUnits="userSpaceOnUse" cx="100" cy="141" r="45">
          <stop stopColor="var(--logo-glint)" />
          <stop offset=".35" stopColor="var(--logo-start)" stopOpacity=".85" />
          <stop offset="1" stopColor="var(--logo-start)" stopOpacity="0" />
        </radialGradient>
        <radialGradient
          id={`${id}-reflection`}
          gradientUnits="userSpaceOnUse"
          cx="100"
          cy="80"
          r="85"
        >
          <stop stopColor="var(--logo-glint)" stopOpacity=".7" />
          <stop offset="1" stopColor="var(--logo-start)" stopOpacity="0" />
        </radialGradient>
        <g id={`${id}-ink`} strokeLinecap="round" strokeLinejoin="round">
          <g fill="none">
            <path
              className="logo-ring"
              d="M100 12A88 88 0 0 1 100 188A88 88 0 0 1 100 12Z"
              strokeWidth="8.5"
              pathLength="1"
            />
            <g strokeWidth="10">
              {letters.map((d) => (
                <path key={d} className="logo-letter" d={d} pathLength="1" />
              ))}
            </g>
          </g>
          <g className="logo-meter" stroke="none">
            {xs.map((x, i) => (
              <rect
                key={x}
                x={x - 4.5}
                y={100 + Math.sqrt(76 ** 2 - (x - 100) ** 2) - heights[i]}
                width="9"
                height={heights[i]}
                rx="4.5"
              />
            ))}
          </g>
        </g>
      </defs>
      <use
        className="logo-fx logo-relief"
        href={`#${id}-ink`}
        fill="var(--logo-end)"
        stroke="var(--logo-end)"
      />
      <use href={`#${id}-ink`} fill={`url(#${id})`} stroke={`url(#${id})`} />
      <use
        className="logo-fx"
        href={`#${id}-ink`}
        fill={`url(#${id}-spot)`}
        stroke={`url(#${id}-spot)`}
      />
      <use
        className="logo-fx"
        href={`#${id}-ink`}
        fill={`url(#${id}-reflection)`}
        stroke={`url(#${id}-reflection)`}
      />
      <circle
        className="logo-attention-ring"
        cx="100"
        cy="100"
        r="88"
        fill="none"
        stroke="var(--logo-start)"
        strokeWidth="8.5"
      />
    </svg>
  );
}
