import React from "react"
import ReactDOM from "react-dom/client"
import App from "./App.tsx"
import { loadDemoFixtures } from "./lib/demo"
import { isDemoMode } from "./lib/utils"
import "./index.css"

function render() {
  ReactDOM.createRoot(document.getElementById("root")!).render(
    <React.StrictMode>
      <App />
    </React.StrictMode>
  )
}

if (isDemoMode()) {
  // Views read demo fixtures during render, so a failed chunk load (e.g. a stale chunk after a
  // redeploy) must surface an error instead of rendering the app or leaving a blank page.
  loadDemoFixtures().then(render, (error: unknown) => {
    console.error("Failed to load demo fixtures", error)
    const root = document.getElementById("root")
    if (root) {
      root.textContent =
        "Failed to load demo mode. Reload the page to try again."
    }
  })
} else {
  render()
}
