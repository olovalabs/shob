import { Component, startTransition, useEffect, useState } from 'react';
import type { ErrorInfo, ReactNode } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { TitleBar } from './components/TitleBar';
import { MainView } from './components/MainView';
import { useStore } from './store';

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
          <div className="flex flex-1 items-center justify-center bg-black text-sm text-zinc-300">
            Startup failed to render. Please restart the app.
          </div>
        )
      );
    }

    return this.props.children;
  }
}

function App() {
  const { loadProjects, loadCliTools, loadAvailableShells } = useStore();
  const [isBooting, setIsBooting] = useState(true);

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
      void invoke('cleanup_runtime').catch(() => {});
    };

    window.addEventListener('beforeunload', handleBeforeUnload);
    return () => {
      window.removeEventListener('beforeunload', handleBeforeUnload);
    };
  }, []);
  
  return (
    <>
      <div className="flex h-full min-h-0 flex-col overflow-hidden">
        <StartupErrorBoundary fallback={<div className="h-[40px] shrink-0 bg-[#121212]" />}>
          <TitleBar />
        </StartupErrorBoundary>
        {isBooting ? (
          <div className="flex-1 bg-black" />
        ) : (
          <StartupErrorBoundary>
            <MainView />
          </StartupErrorBoundary>
        )}
      </div>
    </>
  );
}

export default App;
