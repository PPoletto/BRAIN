# Release playbook

Step-by-step guide to cutting a BRAIN release. Run through the **One-time
setup** once, then the **Per-release** flow for every version.

---

## One-time setup

These four steps unlock signed, auto-updating GitHub releases. They only
need to be done once for the lifetime of the product (until you rotate
keys).

### 1. Generate the signing keypair via the Tauri CLI

Tauri 2 uses minisign-format signatures internally, but the canonical
way to generate the keypair is the Tauri CLI — no extra dependency
needed.

```bash
# From the repo root. The command prompts for a password — pick a strong
# one and stash it in your password manager.
pnpm tauri signer generate -w ~/.tauri/brain.key

# Output:
#   ~/.tauri/brain.key       — encrypted private key (password-protected)
#   prints the PUBLIC KEY base64 to stdout — copy it for step 2
```

### 2. Wire the public key into the bundle

Paste the public-key base64 string the previous command printed into
`src-tauri/tauri.conf.json`:

```jsonc
"plugins": {
  "updater": {
    "active": true,
    "endpoints": [
      "https://github.com/<user>/brain/releases/latest/download/latest.json"
    ],
    "pubkey": "dW50cnVzdGVkIGNvbW1lbnQ6...<your-base64>..."
  }
}
```

Commit this change. **`~/.tauri/brain.key` stays OUT of the repo —
the `*.key` rule in `.gitignore` already blocks it, but double-check.**

### 3. Stash the private key

Two places:

- **Your password manager** (1Password / Bitwarden / KeePassXC):
  the contents of `~/.tauri/brain.key` (cat it once and copy) plus
  the password you set in step 1.
- **GitHub Actions Secrets** for the repository — under
  Settings → Secrets and variables → Actions → New repository secret:
  - `TAURI_SIGNING_PRIVATE_KEY` → the full contents of `~/.tauri/brain.key`
    (it's a base64 blob — paste the whole text)
  - `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` → the password you set

### 4. (Optional but recommended) OS code-signing

Beyond minisign — which signs the *update payload* — Windows SmartScreen
and macOS Gatekeeper want a separate OS-trusted signature on the
installer bundles themselves.

**Windows** — buy a code-signing certificate (DigiCert, Sectigo, etc.).
Configure in `tauri.conf.json`:

```jsonc
"bundle": {
  "windows": {
    "certificateThumbprint": "<thumbprint>",
    "digestAlgorithm": "sha256",
    "timestampUrl": "http://timestamp.digicert.com"
  }
}
```

**macOS** — Apple Developer ID Application certificate. Set env vars on
the build host (or as Actions secrets):

- `APPLE_CERTIFICATE` (base64-encoded `.p12`)
- `APPLE_CERTIFICATE_PASSWORD`
- `APPLE_SIGNING_IDENTITY` (e.g. `"Developer ID Application: Pascal Poletto (TEAMID)"`)
- `APPLE_ID`, `APPLE_PASSWORD`, `APPLE_TEAM_ID` for notarisation

**Linux** — sign the AppImage with `gpg`:

```bash
gpg --output brain.AppImage.sig --detach-sign brain.AppImage
```

If you skip this step the installer still works; users just see "Unknown
publisher" warnings.

---

## Per-release flow

### 1. Bump versions

Two files need to agree:

- `src-tauri/tauri.conf.json` → `version`
- `package.json` → `version`

Use the same SemVer string in both (e.g. `0.2.0`).

### 2. Update the changelog

Add a new section to `CHANGELOG.md` describing what users get out of this
release. Keep it user-facing (no internal refactors), grouped by
**Added / Changed / Fixed / Removed**.

### 3. Tag and push

```bash
git add -A
git commit -m "chore: release v0.2.0"
git tag v0.2.0
git push origin main
git push origin v0.2.0
```

### 4. Let CI build the bundles

The `.github/workflows/release.yml` workflow triggers on `v*` tags. It:

1. Builds `pnpm tauri build` on `windows-latest`, `macos-latest`,
   `ubuntu-latest`.
2. The `tauri-action` GitHub Action signs each bundle with your
   minisign private key (sourced from
   `TAURI_SIGNING_PRIVATE_KEY`).
3. Uploads the bundles to a GitHub Release for the tag.
4. Generates `latest.json` with the format Tauri's updater expects:

   ```json
   {
     "version": "0.2.0",
     "notes": "See the changelog at https://github.com/.../releases/tag/v0.2.0",
     "pub_date": "2026-04-30T18:00:00Z",
     "platforms": {
       "windows-x86_64": {
         "signature": "...",
         "url": "https://github.com/.../BRAIN_0.2.0_x64-setup.exe"
       },
       "darwin-x86_64":   { "signature": "...", "url": "..." },
       "darwin-aarch64":  { "signature": "...", "url": "..." },
       "linux-x86_64":    { "signature": "...", "url": "..." }
     }
   }
   ```

### 5. Verify

- Open the GitHub Release page — bundles + `latest.json` must be
  attached.
- Install the new bundle on a clean machine.
- Open an existing BRAIN install on another machine — within 6 h (or
  on first manual click of the version pill in the status bar) the
  updater should detect the new version.

---

## Rolling back

Tauri's updater always picks the version listed in `latest.json` at the
endpoint URL. If a release is broken:

1. Delete the bad release on GitHub (or mark it as a draft).
2. The previous release becomes the new "latest" — installs that already
   pulled the broken version stay on it, but no new install will get it.
3. Cut a `v0.2.1` patch release that fixes the issue.

GitHub keeps the bad bundle in the release tag for forensics; don't
delete the *tag* unless you really mean it.

---

## Local release smoke test

Before you push the tag, confirm the build works on your machine:

```bash
pnpm tauri build

# Output lands under src-tauri/target/release/bundle/<platform>/
ls src-tauri/target/release/bundle/
```

Install it locally (`.msi` on Windows, drag to `/Applications` on macOS,
`AppImage` on Linux). Run through:

- Onboarding completes
- bge-m3 download succeeds
- A test page is auto-committed
- `claude mcp list` shows `BRAIN`
- Tray icon shows `BRAIN ready` after mount

If all four pass, you're good to tag.
