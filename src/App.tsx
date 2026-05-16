import { createSignal, onCleanup, onMount, ErrorBoundary } from 'solid-js'
import { nativeApi } from './services/native'
import { TitleBar } from './components/TitleBar'
import { MainView } from './components/MainView'
import { useStore } from './store'

function App() {
  const { loadProjects, loadCliTools, loadAvailableShells } = useStore()
  const [isBooting, setIsBooting] = createSignal(true)

  onMount(() => {
    const BOOT_TIMEOUT_MS = 4000

    const initialize = async () => {
      try {
        await Promise.race([
          loadProjects(),
          new Promise<void>((resolve) => {
            window.setTimeout(resolve, BOOT_TIMEOUT_MS)
          }),
        ])
      } catch (error) {
        console.error('App boot initialization failed:', error)
      }

      setIsBooting(false)
    }

    void initialize()
  })

  onMount(() => {
    let timeoutId: number | null = null
    let idleId: number | null = null

    const runDeferredInitialization = () => {
      void Promise.allSettled([loadCliTools(), loadAvailableShells()])
    }

    const scheduleDeferredInitialization = () => {
      if (typeof window.requestIdleCallback === 'function') {
        idleId = window.requestIdleCallback(() => {
          runDeferredInitialization()
        }, { timeout: 1500 })
        return
      }

      timeoutId = window.setTimeout(() => {
        runDeferredInitialization()
      }, 250)
    }

    timeoutId = window.setTimeout(scheduleDeferredInitialization, 150)

    onCleanup(() => {
      if (timeoutId !== null) {
        window.clearTimeout(timeoutId)
      }
      if (idleId !== null && typeof window.cancelIdleCallback === 'function') {
        window.cancelIdleCallback(idleId)
      }
    })
  })

  onMount(() => {
    const handleBeforeUnload = () => {
      void nativeApi.invoke('cleanup_runtime').catch(() => {})
    }

    window.addEventListener('beforeunload', handleBeforeUnload)
    onCleanup(() => {
      window.removeEventListener('beforeunload', handleBeforeUnload)
    })
  })

  const fallback = (
    <div class="flex flex-1 items-center justify-center bg-black text-sm text-zinc-300">
      Startup failed to render. Please restart the app.
    </div>
  )

  return (
    <>
      <div class="flex h-full min-h-0 flex-col overflow-hidden">
        <ErrorBoundary fallback={fallback}>
          <TitleBar />
        </ErrorBoundary>
        {isBooting() ? (
          <div class="flex-1 bg-black" />
        ) : (
          <ErrorBoundary fallback={fallback}>
            <MainView />
          </ErrorBoundary>
        )}
      </div>
    </>
  )
}

export default App
