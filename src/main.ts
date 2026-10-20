// Entry point: boot the bridge, wire the island, start the greeting.

import "./style.css";
import { Bridge, IS_TAURI, onEvent } from "./core/bridge";
import { Sound } from "./core/sound";
import { State, type Settings } from "./core/state";
import { Island } from "./island/island";
import { registerHookHandlers } from "./island/hooks";
import { registerIntegrationHandlers, refreshConfigured } from "./island/integrations";
import { registerShortcutHandlers } from "./island/shortcuts";
import { Recap } from "./recap/recap";
import { featureOn, loadFeatures, onFeaturesChanged, setIslandApi, startFeatures } from "./core/registry";
import { pillDefinition } from "./core/pills";
import { onLanguageChange, resolveLanguage, setLanguage, systemLanguages } from "./i18n/i18n";
import { installTooltip } from "./ui/tooltip";

/** Shows the language Settings asks for ("" = the system's, when Lintel has it). */
function applyLanguage() {
  setLanguage(resolveLanguage(State.settings.language, systemLanguages()));
}

async function main() {
  const root = document.getElementById("root");
  if (!root) return;

  void Sound.preload();

  // Before the island exists (its canvases size their bitmaps once): make one
  // CSS pixel one window pixel when the web view's ratio differs from the window's.
  const dpr = window.devicePixelRatio || 1;
  if (await Bridge.fitZoom(dpr)) {
    for (let i = 0; i < 20 && window.devicePixelRatio === dpr; i++) await new Promise((r) => setTimeout(r, 25));
  }

  const island = new Island(root);
  installTooltip();

  const boot = await Bridge.boot();
  if (boot) {
    State.settings = { ...State.settings, ...boot.settings };
  }
  // A language change redraws the island in place: the texts given as tl(…)
  // relabel themselves (views/dom.ts) and the views redraw the rest on this
  // sync. Nothing is rebuilt, so tasks, steps and the chat stay as they are.
  onLanguageChange(() => State.notify());
  applyLanguage();
  // Rust shows the tray and its errors in the same language as the webview.
  void Bridge.setSystemLanguages(systemLanguages());
  island.applySettings();
  // Which features are on decides what the island shows from its first frame:
  // the pills of a built-in feature that is off are not loaded.
  await loadFeatures();
  State.pillAllowed = (id) => featureOn(pillDefinition(id)?.category === "service" ? "integrations" : "agents");
  onFeaturesChanged(() => State.loadIntegrationTasks());
  State.loadIntegrationTasks();
  setIslandApi((feature) => island.api(feature));

  await onEvent<{ x: number; y: number; down?: boolean }>("cursor", ({ x, y, down }) => island.onCursor(x, y, down === true));

  /** Pause has to reach Rust too, or the pollers keep calling out. */
  const setPaused = (on: boolean) => {
    if (State.paused === on) return;
    State.paused = on;
    void Bridge.setPaused(on);
  };

  await onEvent<string>("tray", (what) => {
    switch (what) {
      case "settings":
        setPaused(false);
        island.alert("settings");
        break;
      case "open":
        setPaused(false);
        island.alert(State.defaultView());
        break;
      case "putback":
        island.putBack();
        break;
      case "recap":
        setPaused(false);
        void Recap.open(island);
        break;
      case "pause":
        setPaused(!State.paused);
        if (State.paused) island.fsm.forceHidden();
        else island.reveal();
        break;
    }
  });

  // A dragged island follows its display, or goes back to the top centre when that display is gone.
  await onEvent<null>("screen-changed", async () => {
    await Bridge.fitZoom(window.devicePixelRatio || 1);
    void island.place();
  });

  // Settings → Reload sounds: read the sounds folder again, and let them hear it.
  await onEvent<null>("sounds-changed", () => {
    void Sound.reload().then(() => Sound.play("pop"));
  });

  // The settings window writes preferences; apply them here without a restart.
  await onEvent<Settings>("settings-changed", (s) => {
    const previousMain = State.mainPillId;
    State.settings = { ...State.settings, ...s };
    applyLanguage();
    island.applySettings();
    State.loadIntegrationTasks();
    // A new main tool comes to the front, as on macOS.
    if (State.mainPillId !== previousMain) State.setFocus(State.mainPillId);
    void refreshConfigured();
  });

  registerHookHandlers(island);
  registerIntegrationHandlers(island);
  registerShortcutHandlers(island, () => setPaused(false));

  // Monday recap: app start (greeting over), an agent starting work, waking up.
  const checkRecap = () => void (featureOn("recap") && Recap.check(island));
  island.onGreetingDone = checkRecap;
  island.onWake = checkRecap;
  await onEvent<null>("recap-check", checkRecap);

  // Every feature that is on gets its `start`; switches are followed from here.
  await startFeatures();

  island.launch();

  // In a plain browser there is no wake strip behind the cursor: make the whole
  // page wake the island so the visuals can be checked with `npm run dev`.
  if (!IS_TAURI) {
    document.addEventListener("click", () => Sound.resume(), { once: true });
  }
}

void main();
