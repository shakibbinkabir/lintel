// Chat plus — the page side (docs/ARCHITECTURE.md §2.3).
// Drag the mascot out of the island onto another window to attach it to the chat;
// the mail window sends a dropped file; the halo is the overlay shown while dragging.
// "Always" on approval cards lives in src/views/views.ts and calls "always" here.

import "./style.css";
import { emit, listen } from "@tauri-apps/api/event";
import { defineFeature } from "../../core/feature";
import { Bridge } from "../../core/bridge";
import { btn, field } from "../../ui/components";
import { h } from "../../views/dom";
import { attachFile, attachWindow } from "../../views/chat";
import { t, tl } from "../../i18n/i18n";

/** Dragging the mascot further than this starts the attach drag. */
const DRAG_PX = 7;

export default defineFeature({
  id: "chat-plus",

  start(cx) {
    let armed: { x: number; y: number } | null = null;
    let release: (() => void) | null = null;
    /** Ctrl+drag the mascot: the screen; Shift+drag: an area. Taken when the button is let go. */
    let shot: "full" | "area" | null = null;

    // The capture feature keeps the picture; this copies it into the chat's inbox and attaches it.
    const takeShot = async (kind: "full" | "area") => {
      const done = new Promise<string>((resolve) => {
        const stop = window.setTimeout(() => resolve(""), 120_000);
        void listen<{ shot: { name: string } }>("capture:captured", (e) => {
          window.clearTimeout(stop);
          resolve(e.payload.shot.name);
        }).then((un) => void done.finally(un));
      });
      await Bridge.featureCall("capture", "begin", kind === "full" ? { id: "full" } : { tool: "shot" });
      const name = await done;
      if (!name) return;
      const file = await cx.call<{ name: string; path: string }>("adopt", { name });
      if (file && attachFile(file)) void emit("shortcut", "openChat");
    };

    const overMascot = (e: MouseEvent) => {
      const canvas = document.getElementById("bot-canvas");
      if (!canvas || getComputedStyle(canvas).opacity === "0") return false;
      const r = canvas.getBoundingClientRect();
      const radius = Math.max(11, r.width / 4);
      return Math.hypot(e.clientX - (r.left + r.width / 2), e.clientY - (r.top + r.height / 2)) <= radius;
    };
    const down = (e: MouseEvent) => {
      armed = e.button === 0 && overMascot(e) ? { x: e.screenX, y: e.screenY } : null;
    };
    const move = (e: MouseEvent) => {
      if (!armed || !(e.buttons & 1)) return;
      if (Math.hypot(e.screenX - armed.x, e.screenY - armed.y) < DRAG_PX) return;
      armed = null;
      if (e.ctrlKey || e.shiftKey) {
        shot = e.shiftKey ? "area" : "full";
        return;
      }
      // Rust follows the pointer from here (the island goes click-through once it is left).
      release ??= cx.island.hold();
      void cx.call("drag");
    };
    const up = () => {
      armed = null;
      const kind = shot;
      shot = null;
      if (kind) void takeShot(kind);
    };
    document.addEventListener("mousedown", down, true);
    document.addEventListener("mousemove", move, true);
    document.addEventListener("mouseup", up, true);

    const offEnd = cx.on("ended", () => {
      release?.();
      release = null;
    });
    const off = cx.on<{ appName: string; title: string }>("attached", (w) => {
      if (attachWindow(w)) void emit("shortcut", "openChat");
    });
    return () => {
      document.removeEventListener("mousedown", down, true);
      document.removeEventListener("mousemove", move, true);
      document.removeEventListener("mouseup", up, true);
      release?.();
      off();
      offEnd();
    };
  },

  windows: {
    halo(root) {
      root.append(h("div", { class: "f-chat-plus-halo" }));
    },

    mail(root, cx) {
      const to = field({ placeholder: tl("name@example.com"), icon: "send" });
      const from = field({ placeholder: tl("Sender address (verified in Resend)") });
      const subject = field({ placeholder: tl("Subject") });
      const message = field({ placeholder: tl("Message (optional)"), multiline: true });
      const status = h("div", { class: "f-chat-plus-status" });
      const fileLine = h("div", { class: "f-chat-plus-file" });
      const close = () => void cx.call("mail_close");
      const send = btn(tl("Send"), "primary", () => void submit());
      const cancel = btn(tl("Cancel"), "ghost", close);
      from.hidden = true;

      // Fills the fields: from a dropped file alone, or from a draft voice prepared (to, subject, message, maybe no file).
      const load = () =>
        void cx.call<{ name: string; size: number; to: string; subject: string; message: string; resend: boolean; from: string }>("mail_info").then((i) => {
          if (!i) return;
          fileLine.textContent = i.name ? `${i.name} · ${(i.size / 1024).toFixed(i.size < 1024 * 100 ? 1 : 0)} KB` : t("No file attached");
          to.input.value = i.to;
          subject.input.value = i.subject || i.name;
          message.input.value = i.message;
          from.input.value = i.from;
          from.hidden = !i.resend;
          status.textContent = "";
        });
      load();
      cx.on("mailChanged", load);

      async function submit() {
        send.disabled = true;
        status.className = "f-chat-plus-status";
        status.textContent = t("Sending…");
        try {
          const r = await Bridge.featureCall<{ via: string }>("chat-plus", "mail_send", {
            to: to.input.value,
            from: from.input.value,
            subject: subject.input.value,
            message: message.input.value,
          });
          const via = r?.via;
          if (via === "cancelled") {
            status.textContent = "";
          } else if (via === "mailto") {
            status.className = "f-chat-plus-status warn";
            status.textContent = t("Your mail client does not take attachments from here. The message is open without the file; the file's folder is open too.");
          } else {
            status.className = "f-chat-plus-status ok";
            status.textContent = via === "resend" ? t("Email sent.") : t("Opened in your mail client. Press send there.");
            window.setTimeout(close, 1800);
          }
        } catch (err) {
          status.className = "f-chat-plus-status bad";
          status.textContent = String(err).replace(/^Error:\s*/, "");
        }
        send.disabled = false;
      }

      root.append(
        h(
          "div",
          { class: "win f-chat-plus-mail" },
          h("div", { class: "win-bar" }, h("span", { text: tl("Send by email") }),
            h("button", { class: "close", title: tl("Close"), onclick: close, text: "×" })),
          h("div", { class: "f-chat-plus-form" }, fileLine, to, from, subject, message, status,
            h("div", { class: "f-chat-plus-actions" }, cancel, send)),
        ),
      );
    },
  },
});
