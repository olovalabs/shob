import { createRoot } from 'react-dom/client'
import './index.css'
import App from './App.tsx'

document.documentElement.classList.add('dark')
document.documentElement.style.colorScheme = 'dark'

createRoot(document.getElementById('root')!).render(
  <App />,
)

const bootSplash = document.getElementById('boot-splash')
if (bootSplash) {
  requestAnimationFrame(() => {
    bootSplash.classList.add('boot-splash-hidden')
    bootSplash.addEventListener('transitionend', () => {
      bootSplash.remove()
    }, { once: true })
    setTimeout(() => {
      if (bootSplash.isConnected) bootSplash.remove()
    }, 500)
  })
}
