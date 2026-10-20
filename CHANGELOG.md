# Changelog

All notable changes to Lintel are written in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [2.0.0] - 2026-10-20

A large release: about ninety additions and fixes, many of them ideas from the projects Lintel grew from, rebuilt for Windows. New options are off until you turn them on.

### Coding agents

- Plan approval cards for Claude Code: the plan's title, then Auto mode, Manual or Change plan. Approving a plan now really lets Claude Code go ahead.
- A card answered in the terminal leaves the island at once.
- "Open terminal" finds the session's window for every session, including PowerShell under Windows Terminal, and falls back more reliably when Windows refuses to bring it forward.
- Sessions already running when Lintel starts get their pills straight away. A pill is named after the session's /rename title or its folder, or Claude Code's terminal title, and a second Claude profile gets its own pill.
- Quiet when it should be: no sound or reveal when a turn finishes in the terminal you are looking at, and optional quiet while the microphone or camera is in use. Optionally, another session's finish shows its card for a moment.
- Claude Code inside WSL and on SSH hosts: hooks installed after you see the change, events back to the island, Allow and Deny from the island.
- More agents: Cursor shell commands, Kiro CLI, Antigravity with exact one-command grants, OpenCode 1 and 2 (questions and permissions as cards), Codex patches as per-file diffs, JetBrains terminals.
- Background runs (scripts, `claude -p`) no longer make pills. Hooks follow `CLAUDE_CONFIG_DIR`. Slash commands show as typed.
- A small code view for a session (the lines just changed, the last command, Follow edits), and an optional agents and plan-limits line on the resting strip.
- The GitHub card shows a session's `git push` as it happens and the CI of the session's branch; GitLab and Gitea can sit beside GitHub.
- Usage: one card per Claude profile, OpenCode and Muse readers, and a weekly pace line with a daily allowance.

### The island and the mascot

- Notifications from other apps (WhatsApp, Telegram, Teams, Mail and others) can show in the island: Lintel reads them as they pop up, with no signing needed. Off by default; nothing leaves the PC.
- Updates from the island: after you agree once, Lintel checks GitHub for new versions, and Update now downloads the installer from GitHub, checks it against GitHub's SHA-256 and installs it.
- A watchdog restarts Lintel after a crash (never after you quit).
- The island stays out of Alt+Tab, stays painted after a lock, fits displays with different scaling, and drops onto a hidden island finish. The autostart entry follows the app when it moves.
- Any number of service pills, a Linear inbox pill, your own sound files, and a louder volume range.
- The mascot falls asleep when things are quiet and reacts to a busy, hot or tired PC.

### Chat

- Formulas drawn as math, code cards for dropped files, a screenshot from the mascot by Ctrl+drag, today's agenda on request.
- A Claude gateway address, a choice when a model cannot take pictures, and tools from MCP servers you already set up in other apps (each call asks first).

### Tools

- Window layout: pin any window on top, drag a window from anywhere with a modifier, snapping to edges and zones with per-edge gaps, and a strip of your named virtual desktops.
- Screenshots: Copy text from a capture, zoom and drag-out for pinned captures, and in the editor a spotlight, a magnifier, a ruler, curved arrows, filled shapes, extra space around a picture and pasted images.
- Cleaner: a disk map and a duplicate finder (only the copies you tick go to the Recycle Bin).
- A small bar beside text you select in any app: copy, clean a link, change case, keep it on the shelf.
- Sleep or shut down after a time (shutdown always asks first), keep awake while agents work, brightness keys that move every display, a separate touchpad scroll direction, and a radial slice that runs a command.
- Mixer: the mouse wheel moves a fader, the headphone guard can go to silence, and media from several browser tabs can be switched one by one.
- Calendar event notes, every Bluetooth accessory with its battery, Save as MP3, checkboxes in the scratchpad, and a shelf that folds the island when you drag an item out.

### Changed

- Releases are still not code-signed: the free signing program for open-source projects declined the project for now. A Microsoft Store version, signed by Microsoft, is being prepared.

## [1.0.1] - 2026-10-19

### Fixed

- The plan usage status line could run another relay that ran it back, without end, when an older build's relay was also installed. Each refresh started a new chain of processes, hundreds within minutes (#3). A relay started by another relay now runs nothing more, connecting the status line never keeps another relay as your own, and a status line that runs past its time is ended with every process it started.

## [1.0.0] - 2026-10-12

The first public release, and the first under the name Lintel. Earlier builds were private and carried a code name.

### The island

- An island that stays hidden at the top of the screen and opens on hover, on a click or with a shortcut.
- A Home view, a section for each feature, a resting strip with a mascot, and a tray menu.
- The island can be dragged away from the top and float as a pill. It steps aside for full-screen apps.
- Ten interface languages. Global shortcuts that can be changed.

### Agents

- Sessions of Claude Code, Codex, Gemini CLI, Cursor, GitHub Copilot CLI, OpenCode, Amp, Hermes, Antigravity and the Claude desktop app.
- Permission cards answered from the island with Allow, Always or Deny. Lintel never answers one by itself.
- Question cards, a live diff, a finished card, a queue when several requests wait, and a jump to the session's terminal.
- Plan usage for Claude and Codex, local usage figures and a weekly recap.
- An agent's settings file is changed only after a dated backup, a diff and a confirmation.

### Chat

- Chat with Anthropic, OpenAI, Google and OpenRouter models, OpenAI-compatible servers, local models and command-line agents.
- Drop a file to ask about it, or attach a window as context.

### Media

- Now playing with cover, seek bar and transport, for any player Windows knows.
- Synced lyrics (the lookup is off by default), a visualiser and a volume mixer per app.
- Level displays for volume, brightness and the microphone.

### System

- System monitor, battery, clipboard history, shelf, scratchpad, timer, calendar, reminders and weather.
- Keep awake, display controls, quick toggles, window layout, input tools, a command bar and a radial menu.
- App updates through winget, a cleaner and an uninstaller.
- Temperatures and fans through an optional helper that runs as administrator, only after consent.
- Status from GitHub, Vercel, Stripe, n8n, Resend, Notion and Cal.com with the user's own keys.
- A desktop companion.

### Capture

- Screenshots with an editor, text and QR codes read from a capture, a colour picker, a ruler and drawing on the screen.
- Screen recording.
- Media tools for pictures.

### Voice

- New and off by default. It has had little testing with live speech, so treat it as unproven.
- A talk key, and an optional wake phrase, "OK Lintel".
- Spoken commands and spoken answers. Speech recognition, the command model, the voice and the text model run on the computer. Each is downloaded only on a click.
- English only. An answer from the local text model takes 9 to 15 seconds to start on a laptop.
- Dictation into the app in front.

### Under the hood

- A Rust core with a web view for the interface (Tauri 2). One crate and one folder per feature.
- A feature that is switched off loads nothing.
- No telemetry and no account. Keys are kept in Windows Credential Manager.
- An installer for one user, without administrator rights. It is not code-signed.
- At the first start, settings, data and saved keys of the code-named build are copied over once. The old install is left as it is. Agents are connected again from Settings.

[1.0.1]: ../../releases/tag/v1.0.1
[1.0.0]: ../../releases/tag/v1.0.0
