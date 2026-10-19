# Changelog

All notable changes to Lintel are written in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
