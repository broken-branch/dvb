import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import './design-system.css';
import App from './App.tsx';
import { applyPalette, palettes } from './lib/theme';
applyPalette(palettes.current, 'Auto');
createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
