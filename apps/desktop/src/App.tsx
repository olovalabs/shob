import { Component, startTransition, useEffect, useState, useMemo } from 'react';
import type { ErrorInfo, ReactNode } from 'react';
import { nativeApi } from './services/native';
import { TitleBar } from './components/TitleBar';
import { MainView } from './components/MainView';
import { useStore } from './store';
import { applyAppearanceTheme } from './theme';
import type { ElectronOpencodeLogEvent } from './electron';
import { MarkedProvider } from '@shob/ui/context/marked';
import { I18nProvider } from '@shob/ui/context/i18n';
import { dict as en } from '@shob/ui/i18n/en';
import type { UiI18n } from '@shob/ui/context/i18n';

class StartupErrorBoundary extends Component<
  { children: ReactNode; fallback?: ReactNode },
  { hasError: boolean }
> {
  state = { hasError: false };

  static getDerivedStateFromError() {
    return { hasError: true };
  }

  componentDidCatch(error: Error, errorInfo: ErrorInfo) {
    console.error('Startup render crashed:', error, errorInfo);
  }

  render() {
    if (this.state.hasError) {
      return (
        this.props.fallback ?? (
          <div className="flex flex-1 items-center justify-center bg-background text-sm text-muted-foreground">
            Startup failed to render. Please restart the app.
          </div>
        )
      );
    }

    return this.props.children;
  }
}

function App() {
  const { loadProjects, loadCliTools, loadAvailableShells, appearanceThemeId, colorScheme } = useStore();
  const [isBooting, setIsBooting] = useState(true);

  useEffect(() => {
    applyAppearanceTheme(appearanceThemeId, colorScheme);

    if (colorScheme !== 'system') {
      return;
    }

    const mediaQuery = window.matchMedia('(prefers-color-scheme: dark)');
    const handleSystemThemeChange = () => {
      applyAppearanceTheme(appearanceThemeId, colorScheme);
    };

    mediaQuery.addEventListener('change', handleSystemThemeChange);
    return () => {
      mediaQuery.removeEventListener('change', handleSystemThemeChange);
    };
  }, [appearanceThemeId, colorScheme]);

  useEffect(() => {
    let cancelled = false;
    const BOOT_TIMEOUT_MS = 4000;

    const initialize = async () => {
      try {
        await Promise.race([
          loadProjects(),
          new Promise<void>((resolve) => {
            window.setTimeout(resolve, BOOT_TIMEOUT_MS);
          }),
        ]);
      } catch (error) {
        console.error('App boot initialization failed:', error);
      }

      if (cancelled) return;

      startTransition(() => {
        setIsBooting(false);
      });
    };

    void initialize();

    return () => {
      cancelled = true;
    };
  }, [loadProjects]);

  useEffect(() => {
    if (isBooting) {
      return;
    }

    let cancelled = false;
    let timeoutId: number | null = null;
    let idleId: number | null = null;

    const runDeferredInitialization = () => {
      void Promise.allSettled([loadCliTools(), loadAvailableShells()]);
    };

    const scheduleDeferredInitialization = () => {
      if (cancelled) return;

      if (typeof window.requestIdleCallback === 'function') {
        idleId = window.requestIdleCallback(() => {
          if (!cancelled) {
            runDeferredInitialization();
          }
        }, { timeout: 1500 });
        return;
      }

      timeoutId = window.setTimeout(() => {
        if (!cancelled) {
          runDeferredInitialization();
        }
      }, 250);
    };

    timeoutId = window.setTimeout(scheduleDeferredInitialization, 150);

    return () => {
      cancelled = true;
      if (timeoutId !== null) {
        window.clearTimeout(timeoutId);
      }
      if (idleId !== null && typeof window.cancelIdleCallback === 'function') {
        window.cancelIdleCallback(idleId);
      }
    };
  }, [isBooting, loadCliTools, loadAvailableShells]);

  useEffect(() => {
    const handleBeforeUnload = () => {
      void nativeApi.invoke('cleanup_runtime').catch(() => {});
    };

    window.addEventListener('beforeunload', handleBeforeUnload);
    return () => {
      window.removeEventListener('beforeunload', handleBeforeUnload);
    };
  }, []);

  useEffect(() => {
    const unlistenPromise = nativeApi.listen<ElectronOpencodeLogEvent>('opencode-log', ({ payload }) => {
      const method = payload.level === 'debug' ? 'log' : payload.level;
      console[method](`[opencode] ${payload.message}`, payload.meta ?? '');
    });

    return () => {
      void unlistenPromise.then((unlisten) => unlisten());
    };
  }, []);
  
  const i18n = useMemo<UiI18n>(() => ({
    locale: 'en',
    t: (key, params) => {
      const value = en[key] ?? String(key)
      if (!params) return value
      return value.replace(/{{\s*([^}]+?)\s*}}/g, (_, rawKey) => {
        const k = String(rawKey)
        const v = params[k]
        return v === undefined ? '' : String(v)
      })
    },
  }), [])

  return (
    <>
      <I18nProvider value={i18n}>
        <MarkedProvider>
          <div className="flex h-full min-h-0 flex-col overflow-hidden">
            <StartupErrorBoundary fallback={<div className="h-[40px] shrink-0 bg-chrome" />}>
              <TitleBar />
            </StartupErrorBoundary>
            {isBooting ? (
              <div className="flex-1 bg-background" />
            ) : (
              <StartupErrorBoundary>
                <MainView />
              </StartupErrorBoundary>
            )}
          </div>
        </MarkedProvider>
      </I18nProvider>
    </>
  );
}

export default App;
