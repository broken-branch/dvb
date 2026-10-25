import { useEffect, useState } from 'react';
/** Keep elapsed work and the seven-day boundary current without another request. */
export function useWorkClock() {
  const [now, setNow] = useState(Date.now);
  useEffect(() => {
    const timer = setInterval(() => setNow(Date.now()), 15000);
    return () => clearInterval(timer);
  }, []);
  return now;
}
