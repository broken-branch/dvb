import {
  createContext,
  useContext,
  useEffect,
  useState,
  useMemo,
  useCallback,
  type ReactNode,
} from 'react';
import { useStore } from './store';
import { refusalText } from './data/display';
import { toast } from 'sonner';
import { MotionConfig } from 'motion/react';
import { followDeskTheme } from './desk-theme';
import { applyPalette, palettes, type Palette } from './theme';
import {
  textSizes,
  type TextSize,
  type MotionPreference,
  type SettingValues,
} from './data/settings';
export { textSizes };
function useAppearanceState(values: SettingValues) {
  const { text_size: textSize, motion: motionPreference, theme, font } = values;
  useEffect(() => {
    document.documentElement.style.setProperty('--written-text-scale', String(textSizes[textSize]));
  }, [textSize]);
  const [desk, setDesk] = useState<Palette | undefined>();
  useEffect(() => followDeskTheme(setDesk), []);
  const [systemReduced, setSystemReduced] = useState(
    () => window.matchMedia('(prefers-reduced-motion: reduce)').matches,
  );
  useEffect(() => {
    const media = window.matchMedia('(prefers-reduced-motion: reduce)');
    const update = () => setSystemReduced(media.matches);
    media.addEventListener('change', update);
    return () => media.removeEventListener('change', update);
  }, []);
  const reduced =
    motionPreference === 'Off' || (motionPreference === 'Follow system' && !!systemReduced);
  useEffect(() => {
    document.documentElement.classList.toggle('effects-off', reduced);
    document.documentElement.dataset.motion = motionPreference;
  }, [reduced, motionPreference]);
  const palette = (theme === 'auto' ? desk : palettes[theme]) ?? palettes.current;
  useEffect(() => applyPalette(palette, font), [palette, font]);
  return {
    textSize,
    motionPreference,
    reduced,
    theme,
    font,
    palette,
  };
}
type Appearance = ReturnType<typeof useAppearanceState> & {
  setTextSize(value: TextSize): void;
  setTheme(value: string): void;
  setFont(value: string): void;
  setMotionPreference(value: MotionPreference): void;
};
const Context = createContext<Appearance | null>(null);
export function AppearanceProvider({ children }: { children: ReactNode }) {
  const { data, settings } = useStore(),
    local = useAppearanceState(settings);
  const changeSettings = data.changeSettings;
  const save = useCallback(
    async (key: string, value: unknown) => {
      try {
        const result = await changeSettings({ values: { [key]: value } });
        if (!result.ok) toast.error(refusalText(result.refusal));
        return result.ok;
      } catch (e) {
        toast.error(e instanceof Error ? e.message : 'Unable to save appearance.');
        return false;
      }
    },
    [changeSettings],
  );
  const value = useMemo(
    () => ({
      ...local,
      setTextSize: (size: TextSize) => {
        void save('text_size', size);
      },
      setTheme: (theme: string) => {
        void save('theme', theme);
      },
      setFont: (font: string) => {
        void save('font', font);
      },
      setMotionPreference: (motion: MotionPreference) => {
        void save('motion', motion);
      },
    }),
    [
      local.textSize,
      local.motionPreference,
      local.reduced,
      local.theme,
      local.font,
      local.palette,
      save,
    ],
  );
  return (
    <Context.Provider value={value}>
      <MotionConfig
        reducedMotion={value.reduced ? 'always' : 'never'}
        transition={
          value.reduced ? { duration: 0 } : { type: 'spring', stiffness: 420, damping: 40 }
        }
      >
        {children}
      </MotionConfig>
    </Context.Provider>
  );
}
export function useAppearance() {
  const value = useContext(Context);
  if (!value) throw new Error('AppearanceProvider required');
  return value;
}

export function useAppReducedMotion() {
  return useContext(Context)?.reduced ?? false;
}
