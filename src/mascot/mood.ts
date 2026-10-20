// What the island's mascot makes of the quiet and of the PC. Not a feature, no timer runs while
// the mascot is hidden: the sleep is one timeout that re-arms itself, and the PC reading
// (`sysmon` "mood", every 2 s) is asked for only while the mascot is on screen.
//
//   apply(state)  the face to show: the agent's state wins; at rest, sleep, then the PC's mood.
//   poke()        something happened (pointer, sound, microphone): wakes it, restarts the quiet.
//   watch(on)     the mascot is on screen or not.
//   tip           why the PC mood is on, for the mascot's tooltip ("" = none).

import { Bridge, onEvent } from "../core/bridge";
import type { BotStateName } from "../core/layout";
import { t } from "../i18n/i18n";

/** How long nothing may happen before it falls asleep. */
const QUIET_MS = 5 * 60_000;
interface Reading { cpu: number | null; pressure: number | null; battery: number | null; temp: number | null }

export class Mood {
  private asleep = false;
  private last = performance.now();
  private timer: number | undefined;
  private busyRuns = 0;
  private pc: BotStateName | null = null;
  private watching = false;
  private pcTip = "";

  /** `changed`: the face may be different now (the island draws again). */
  constructor(private changed: () => void) {
    void onEvent<Reading>("sysmon:mood", (r) => this.read(r));
    void onEvent<{ apps: string[] }>("huds-power:micUse", (m) => m.apps.length > 0 && this.poke());
  }

  get tip(): string {
    return this.pc ? this.pcTip : "";
  }

  apply(state: BotStateName): BotStateName {
    if (state !== "idle") {
      this.poke();
      return state;
    }
    return this.pc ?? (this.asleep ? "sleeping" : state);
  }

  poke() {
    this.last = performance.now();
    if (this.asleep) {
      this.asleep = false;
      this.changed();
    }
    this.arm();
  }

  watch(on: boolean) {
    if (on === this.watching) return;
    this.watching = on;
    void Bridge.featureCall("sysmon", "mood", { on });
    if (!on) {
      this.pc = null;
      this.busyRuns = 0;
    }
  }

  private arm() {
    if (this.timer !== undefined) return;
    this.timer = window.setTimeout(() => {
      this.timer = undefined;
      const left = QUIET_MS - (performance.now() - this.last);
      if (left > 1000) return this.arm();
      this.asleep = true;
      this.changed();
    }, QUIET_MS - (performance.now() - this.last));
  }

  private read(r: Reading) {
    // debt: the camera is not watched; the system lets a program see only its own.
    this.busyRuns = r.cpu != null && r.cpu >= 0.9 ? this.busyRuns + 1 : 0;
    const was = this.pc;
    let tip = "";
    if (r.temp != null && r.temp >= 85) [this.pc, tip] = ["error", t("Hot: {0} °C", { 0: Math.round(r.temp) })];
    else if ((r.pressure ?? 0) >= 2) [this.pc, tip] = ["dizzy", t("Short of memory")];
    else if (r.battery != null && r.battery <= 15) [this.pc, tip] = ["sleeping", t("Low battery: {0}%", { 0: r.battery })];
    else if (this.busyRuns >= 3) [this.pc, tip] = ["working", t("Busy: the processor is at {0}%", { 0: Math.round((r.cpu ?? 0) * 100) })];
    else this.pc = null;
    this.pcTip = tip;
    if (was !== this.pc) this.changed();
  }
}
