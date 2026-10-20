// Settings → Agents: Claude Code inside WSL distros and on SSH hosts
// (src-tauri/src/wsl.rs, src-tauri/src/remote.rs). Same steps as every other
// install: the exact diff first, and nothing is written before the click.

import { invoke } from "@tauri-apps/api/core";
import { IS_TAURI, type HookPreview } from "../core/bridge";
import { clear, h } from "../views/dom";
import { renderDiff, statusDot } from "./parts";
import { t } from "../i18n/i18n";

interface WslUser { name: string; settingsPath: string; installed: boolean }
interface WslDistro { name: string; running: boolean; users: WslUser[] }
interface HostStatus { host: string; state: "off" | "connecting" | "connected" | "error"; error: string }

const errorText = (err: unknown) => String(err).replace(/^Error:\s*/, "");
const column = (gap: number) => h("div", { style: `display:flex;flex-direction:column;gap:${gap}px` });

/**
 * Shows the diff in `body`, and writes it only on the click that follows.
 * `back` redraws what was there; `done` runs a moment after the write.
 */
async function review(
  body: HTMLElement,
  install: boolean,
  preview: () => Promise<HookPreview>,
  apply: (fingerprint: string) => Promise<string>,
  back: () => void,
  done: () => void,
) {
  clear(body);
  body.append(h("div", { class: "hint", text: t("Reading the current settings…") }));
  let plan: HookPreview;
  try {
    plan = await preview();
  } catch (err) {
    clear(body);
    body.append(
      h("div", { class: "notice err", text: errorText(err) }),
      h("div", { class: "line" }, h("button", { class: "btn secondary", text: t("Back"), onclick: back })),
    );
    return;
  }
  clear(body);
  body.append(
    h("div", {
      class: "hint",
      text: install
        ? t("This is exactly what will change. Your own hooks are left untouched.")
        : t("This removes {app}'s entries only. Your own hooks are left untouched."),
    }),
    renderDiff(plan.diff),
    h("div", { class: "line" }, h("span", {
      class: "path",
      text: plan.backup ? t("Backup → {path}", { path: plan.backup }) : t("No settings.json yet — nothing to back up."),
    })),
  );
  const confirm = h("button", {
    class: install ? "btn primary" : "btn danger",
    text: install ? t("Back up and write") : t("Back up and remove"),
  });
  confirm.addEventListener("click", async () => {
    confirm.disabled = true;
    try {
      const backup = await apply(plan.fingerprint);
      clear(body);
      body.append(h("div", {
        class: "notice ok",
        text: backup
          ? t("Done. Previous settings saved as {backup}. Open a new Claude Code session to pick the hooks up.", { backup })
          : t("Done. Open a new Claude Code session to pick the hooks up."),
      }));
      window.setTimeout(done, 2600);
    } catch (err) {
      confirm.disabled = false;
      body.append(h("div", { class: "notice err", text: t("Could not write: {error}", { error: errorText(err) }) }));
    }
  });
  body.append(h("div", { class: "line" }, confirm, h("button", { class: "btn secondary", text: t("Cancel"), onclick: back })));
}

// ── WSL ───────────────────────────────────────────────────────────────────────

/** Empty (and hidden) until WSL lists a distro: most machines have none. */
export function wslSection(): HTMLElement {
  const blocks = column(14);
  const section = h(
    "section",
    { style: "display:none" },
    h("h2", { class: "section-title" }, h("span", { text: t("Claude Code in WSL") })),
    h("div", {
      class: "hint",
      text: t("Claude Code inside a WSL distro reads that distro's own settings.json. The hooks there run {app}'s relay through WSL, so its sessions, Allow and Deny work as they do on Windows. Only running distros are read: reading a stopped one would start it."),
    }),
    blocks,
  );

  async function load() {
    const list = IS_TAURI ? await invoke<WslDistro[]>("wsl_hooks_list").catch(() => []) : [];
    section.style.display = list.length ? "" : "none";
    clear(blocks);
    for (const distro of list) {
      if (!distro.running) {
        blocks.append(h("div", { class: "line" }, statusDot(false), h("span", {
          class: "hint", text: t("{distro} is not running. Open it once, then come back here.", { distro: distro.name }),
        })));
      } else if (!distro.users.length) {
        blocks.append(h("div", { class: "line" }, statusDot(false), h("span", {
          class: "hint", text: t("{distro}: nobody has a ~/.claude there yet. Run Claude Code in it once.", { distro: distro.name }),
        })));
      }
      for (const user of distro.users) blocks.append(wslUser(distro.name, user, load));
    }
  }

  void load();
  return section;
}

function wslUser(distro: string, user: WslUser, reload: () => Promise<void>): HTMLElement {
  const body = column(10);
  const draw = () => {
    clear(body);
    const install = h("button", {
      class: "btn primary",
      text: user.installed ? t("Reinstall hooks…") : t("Install hooks…"),
      onclick: () => go(true),
    });
    body.append(
      h("div", { class: "line" },
        statusDot(user.installed),
        h("span", { style: "font-size:12.5px", text: `${distro} · ${user.name}` }),
        h("span", { class: "spacer" }),
        install,
        user.installed ? h("button", { class: "btn danger", text: t("Uninstall hooks…"), onclick: () => go(false) }) : null,
      ),
      h("span", { class: "path", text: user.settingsPath }),
    );
  };
  const go = (install: boolean) => void review(
    body,
    install,
    () => invoke<HookPreview>("wsl_hooks_preview", { distro, user: user.name, install }),
    (fingerprint) => invoke<string>("wsl_hooks_apply", { distro, user: user.name, install, fingerprint }),
    draw,
    () => void reload(),
  );
  draw();
  return body;
}

// ── SSH hosts ─────────────────────────────────────────────────────────────────

const STATE_TEXT = {
  get off() { return t("off"); },
  get connecting() { return t("connecting…"); },
  get connected() { return t("connected"); },
  get error() { return t("not connected"); },
};

export function sshSection(): HTMLElement {
  const list = column(14);
  const input = h("input", {
    type: "text",
    placeholder: t("devbox or me@build.example.com"),
    style: "flex:1 1 auto;min-width:0",
    spellcheck: "false",
    autocomplete: "off",
  }) as HTMLInputElement;
  const feedback = h("div", {});
  let hosts: HostStatus[] = [];

  async function save(next: string[]) {
    clear(feedback);
    try {
      hosts = await invoke<HostStatus[]>("ssh_hosts_set", { hosts: next });
      draw();
      // A tunnel takes a few seconds to come up (or fail): look once more then.
      window.setTimeout(() => void invoke<HostStatus[]>("ssh_hosts").then((found) => { hosts = found; draw(); }).catch(() => {}), 8000);
      return true;
    } catch (err) {
      feedback.append(h("div", { class: "notice err", text: errorText(err) }));
      return false;
    }
  }

  const add = async () => {
    const host = input.value.trim();
    if (host && await save([...hosts.map((s) => s.host), host])) input.value = "";
  };
  input.addEventListener("keydown", (e) => { if (e.key === "Enter") void add(); });

  function draw() {
    clear(list);
    for (const status of hosts) list.append(hostBlock(status, () => void save(hosts.map((s) => s.host).filter((x) => x !== status.host))));
  }

  void (IS_TAURI ? invoke<HostStatus[]>("ssh_hosts") : Promise.resolve([])).then((found) => {
    hosts = found;
    draw();
  }).catch(() => {});

  return h(
    "section",
    {},
    h("h2", { class: "section-title" }, h("span", { text: t("Claude Code on SSH hosts") })),
    h("div", {
      class: "hint",
      text: t("Sessions on other machines, over SSH. Add a host as you type it after ssh: an alias from ~/.ssh/config, or user@host. Installing puts a small Python relay in ~/.lintel on the host and adds the hooks to its ~/.claude/settings.json, both shown first. {app} then keeps an ssh tunnel open to the host with your own keys, and nothing listens on the network. The host needs python3. Connect to it once in a terminal first: {app} never answers ssh's questions."),
    }),
    h("div", { class: "line" }, input, h("button", { class: "btn primary", text: t("Add"), onclick: () => void add() })),
    feedback,
    list,
  );
}

function hostBlock(status: HostStatus, remove: () => void): HTMLElement {
  const body = column(10);
  const draw = () => {
    clear(body);
    body.append(
      h("div", { class: "line" },
        statusDot(status.state === "connected"),
        h("span", { style: "font-size:12.5px", text: status.host }),
        h("span", { class: "hint", text: STATE_TEXT[status.state] ?? status.state }),
        h("span", { class: "spacer" }),
        h("button", { class: "btn primary", text: t("Install hooks…"), onclick: () => go(true) }),
        h("button", { class: "btn danger", text: t("Uninstall hooks…"), onclick: () => go(false) }),
        h("button", { class: "btn secondary", text: t("Remove"), onclick: remove }),
      ),
    );
    if (status.state === "error" && status.error) body.append(h("span", { class: "path", text: status.error }));
  };
  const go = (install: boolean) => void review(
    body,
    install,
    () => invoke<HookPreview>("ssh_hooks_preview", { host: status.host, install }),
    (fingerprint) => invoke<string>("ssh_hooks_apply", { host: status.host, install, fingerprint }),
    draw,
    draw,
  );
  draw();
  return body;
}
