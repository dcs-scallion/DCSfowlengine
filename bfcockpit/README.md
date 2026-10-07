# Attrition cockpit overlay

Client-side DCS Hooks plugin that opens an in-game WebView on the Attrition
`/cockpit` page (**Attrition Control** — JTAC panel first; more panels later).
Optional — F10 menus keep working without it.

## Install

Copy:

```
bfcockpit/Scripts/Hooks/attrition_cockpit.lua
  → %USERPROFILE%\Saved Games\DCS\Scripts\Hooks\attrition_cockpit.lua
```

Or download from the running dashboard:

`GET /api/cockpit/plugin/download`

Restart DCS. First run writes `Config\AttritionCockpit.lua`.

## Use

- Open with your **Comms / radio-menu** key (default), or **Ctrl+Shift+J**
- **Sleep / wake** (title bar only, WebView unloaded — no HTTP): double-click the
  window title, or the sleep button in the UI; wake with double-click or Ctrl+Shift+J
- Resize: drag the window corner only (minimum size enforced)
- Compact / restore size: header button in the cockpit UI
- Panels: tab row at the top (currently **JTAC** only); only the active panel polls
- Point `url` in `AttritionCockpit.lua` at the **dashboard** host that serves
  the `/cockpit` SPA (Caddy + bfweb), e.g. `https://stats.attrition.cz/cockpit`.
  Do **not** use bfdb `:8880` — that port is localhost API-only and has no UI.

## License

See `LICENSE` in this directory. Proprietary — Copyright (c) 2026 Robo76
(same terms as `acmi_sanitize`). Independent rewrite; not covered by the
Vector-derived dashboard grant in `bfweb/LICENSE`. Companion UI:
`bfweb/LICENSE.cockpit`. Not AGPL.
