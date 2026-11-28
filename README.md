# companion-plugin-calendar

Dated events, birthdays, and appointments: a settings tab, reminder banners, and calendar tools the avatar can call.

One of the official feature plugins for **Companion**, a plugin-based desktop
companion app (Tauri + React). A plugin is a `manifest.json` plus an optional
bundled React frontend and an optional Rust cdylib backend; see the
companion-tauri repository (`app/PLUGINS.md`) for the full plugin format and
host API documentation.

## Layout

| Path | Contents |
|---|---|
| `manifest.json` | Plugin metadata: id, ABI version, permissions, entry points |
| `src/` | React frontend source (bundled with esbuild at build time) |
| `backend/` | Rust cdylib backend (event store + tools) |
| `scripts/` | `build-vendor.mjs` — bundles vendored deps for the calendar grid |

React is never bundled into the plugin: `react` / `react-dom` are aliased to
the host's copies at build time, so plugin frontends always match the app.

## Building

Install dependencies first (pulls `@companion-lab/plugin` from npm and `companion-plugin-sdk` from crates.io):

```bash
npm install
```


The `companion-cli` CLI ([npm](https://www.npmjs.com/package/companion-cli),
install with `npm install -g companion-cli`) bundles the frontend
(and builds the Rust backend when present) into a zip importable via the app's
**Settings → Plugins → Import plugin…**:

```bash
companion-cli build /path/to/companion-plugin-calendar --backend
```

Install dependencies first if you use a package manager lockfile
(`pnpm install` / `npm install`).

## Google Calendar sync

The plugin can two-way sync with Google Calendar. Setup (one time, per user):

1. In [Google Cloud Console](https://console.cloud.google.com/), create (or
   pick) a project and **enable the Google Calendar API** for it.
2. Configure the **OAuth consent screen** (type *External*; while the app is
   unverified, add your own account as a *test user*).
3. Create **Credentials → OAuth client ID → Desktop app** and copy the
   client ID and client secret.
4. In the app: **Settings → Calendar → Google Calendar**, paste both values
   and hit **Connect Google account**. The consent screen opens in your
   browser and redirects to a local loopback server (`http://127.0.0.1:<port>`,
   per RFC 8252); pick the calendars to sync afterwards.

How the sync behaves:

- **Pull**: selected calendars are pulled incrementally (Google syncToken,
  5-minute error backoff, full resync when the token expires). Google
  deletions/cancellations remove the local copies.
- **Push**: events created or edited in the app (settings UI or chat tools)
  are queued and pushed to your **default calendar**; deletions push too.
- **Reminders**: synced Google events fire the same reminder banner as local
  events. Pausing an event (`enabled` toggle) is app-only.
- **Recurrence**: every Google event — recurring or not — is stored as a
  single local-style row, exactly like events created in the app. RRULEs map
  onto the closest local repeat option (daily / weekdays / weekly / monthly
  / yearly). Qualifiers the local model cannot represent (COUNT, UNTIL,
  EXDATE, exotic BYDAY sets, per-instance overrides) are simplified away:
  the series repeats forever locally, and deleted or moved single
  occurrences may not mirror Google exactly.
- **Editing**: all synced Google events are editable in the app and push
  back to Google; repeating events created in the app push as RRULEs.
- **Conflicts**: last write wins per sync cycle; a local edit waiting in the
  push queue is never overwritten by a pull.
- **Storage**: events and sync state live in SQLite
  (`plugins-data/calendar/calendar.db`); OAuth tokens and the client secret
  live in the OS keychain. Disconnecting revokes the token and stops syncing
  but keeps already-synced events.

Older plugin versions expanded complex recurring series into per-date
read-only rows; the first run of 0.7.0+ drops those rows and re-pulls each
calendar so every Google event becomes a single editable row.
The legacy `calendar.json` store is imported into SQLite automatically on
first run of this version (renamed to `calendar.json.bak`). This is a
one-way migration: older app/plugin versions won't see events stored in
SQLite.

## Downloading releases

CI builds installable zips on every push to `main` (the "Build plugin"
workflow). To publish a release, push a tag matching the version in
`manifest.json`:

```bash
git tag v0.1.0 && git push origin v0.1.0
```

The release gets one zip per platform — download the one for your machine
and import it in the Companion app via **Settings → Plugins → Import
plugin…**:

| File | Platform |
|---|---|
| `calendar-0.1.0-linux-x86_64.zip` | Linux (Intel/AMD 64-bit) |
| `calendar-0.1.0-macos-arm64.zip` | macOS (Apple Silicon) |
| `calendar-0.1.0-macos-x86_64.zip` | macOS (Intel) |
| `calendar-0.1.0-windows-x86_64.zip` | Windows (64-bit) |

Non-tag runs of the workflow publish the same zips as run artifacts for
testing. CI never touches the zips committed under `dist/`.
