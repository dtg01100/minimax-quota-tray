# Security Policy

> **Status: no private reporting channel is configured for this
> repository yet.** Until one is set up, please file security
> concerns as a regular GitHub issue with the `security` label and
> **no exploit details in the body** — just a one-line summary and
> a request to take the conversation off-thread. The repo owner
> monitors new issues.
>
> This document still defines the project's supported versions,
> threat model, and hardening checklist so reporters know what to
> expect and so the next maintainer has a template when they do
> stand up a channel.

## Supported versions

This project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html):
security fixes are released for the **latest minor** of the major
version line.

| Version | Supported          |
|---------|--------------------|
| 0.3.x   | :white_check_mark: |
| < 0.3.0 | :x: (use 0.3.x)    |

The 0.x line is the pre-1.0 development track; minor versions may
contain breaking changes. Until 1.0, treat every minor bump as a
"might affect you" release and read the CHANGELOG before upgrading
in a production setup.

## Reporting a vulnerability

For now: **open a GitHub issue labeled `security` with no exploit
details**, just a one-line summary. The repo owner monitors new
issues and will reach out to move the discussion off-thread before
any code-level details are shared.

When you file, include:

1. The version (`llm-quota-tray --version` if you can get it; else
   the commit SHA from the binary's help banner or the install path
   timestamp).
2. The OS / panel / libsecret provider (e.g. "Fedora 41 + KDE Plasma
   6 + kwalletd6"). Most credential-handling bugs are panel- or
   keyring-daemon-specific.
3. Steps to reproduce. If it's a keyring race, a `secret-tool`
   transcript alongside the tray's stderr is ideal.
4. Whether the issue exposes **your** credentials, a credential the
   tray is *handling on your behalf*, or just an information leak
   (log line, env var, etc.). The triage priority differs.

## Standing up a real channel later

When you do want a private reporting flow, two zero-infrastructure
options that don't require a mailbox you own:

* **GitHub's Private vulnerability reporting** — Repo → Settings →
  Code security and analysis → enable the toggle. Creates a
  `/.github/security/advisories/new` endpoint, handles the disclosure
  handshake, supports co-disclosed CVE-grade advisories.
* **A GitHub Discussions category** with posting restricted to
  maintainers — works as a private intake if the project doesn't
  need CVE-grade handling.

Replace the "Status" callout at the top of this file and the
"Reporting" section above when you stand one up. The rest of the
document (threat model + hardening checklist) stays as-is.

## Threat model — what this binary does and does not protect

This section exists so reports triage quickly. Anything outside this
list is "informational" and may not get a CVE-grade response.

**In scope (will be fixed):**

* The keyring write path (`src/keyring.rs`) — known footgun: the
  `secret-service` Rust crate panics when called from inside a tokio
  worker (CHANGELOG v0.3.0 / commit `d078df4`), so the project
  shells out to `secret-tool(1)` instead. Any leak in that shell-out
  path, any truncation issue, any path-injection in the keyring
  `application` attribute, is in-scope.
* The fallback credential file at
  `~/.config/.config/<instance>/key`. The duplicated `/.config/.config/`
  path is a known legacy quirk (do not "fix" it without coordinating
  with users who have an existing key file there). File-mode
  hardening (must be 0600, must not be world-readable) is in-scope.
* `LLM_API_KEY` env var propagation — make sure it doesn't leak into
  subprocess stdio, log lines, or crash dumps.
* The HTTP client — any credential exfiltration via a redirect, a
  mis-set `Authorization` header, or an SSRF through
  `~/.config/.../config.json` (the `endpoint` field is
  user-controlled; a malicious config could try to redirect polling
  to an attacker's host).
* The systemd unit — any way the `ExecStart=` line could be coerced
  into running arbitrary code via the `--instance=` flag or env vars.

**Out of scope (file a regular issue instead):**

* Bugs in the host panel's SNI implementation (KDE, GNOME Shell,
  Waybar, etc.).
* Bugs in `libsecret` / `gnome-keyring-daemon` / `kwalletd`.
* The gjs implementation on the `gjs/` branch — it's retired and
  exists for history only.

## Hardening checklist for users

These are the things you should do that the binary can't enforce
for you. They're listed here so the next person Googling "is this
tray safe" has a checklist.

* **Lock your screen.** `secret-service` lets any process in your
  session read unlocked keyring entries. If you walk away with the
  session unlocked, the tray's stored API key is readable.
* **Don't run as root.** The systemd unit installs to the **user**
  manager (`~/.config/systemd/user/`) precisely to keep the keyring
  in your user scope. Running the binary as root will silently
  fall through to root's keyring, which probably doesn't have the
  key — and then to env-var fallback, which is a footgun.
* **Treat `~/.config/.config/<instance>/key` like a credential.**
  It's a plaintext file used as the env-var-fallback store. If your
  HOME is on a shared filesystem or an encrypted volume that's
  currently unlocked, anyone with HOME read access can read the key.
* **Rotate the API key in your provider's dashboard** if you've
  ever pasted it into a shell history, posted it to a screenshot,
  or stored it in a non-encrypted dotfile manager. The tray has no
  way to detect that the key has been leaked.
* **Use a per-provider restricted key where the API supports it.**
  MiniMax, OpenAI, Anthropic, etc. all let you mint keys with
  read-only scopes. The tray only needs read access to the quota
  endpoint.

## Before publishing this file

The "Status" callout at the top is the only thing that needs to
change when you stand up a real channel. Everything below the
"Threat model" section is stable across reporting-channel changes
and is worth committing today.

The `security` issue label needs to exist on the repo before
someone can follow the "Reporting" instructions — create it under
Issues → Labels if it doesn't already exist.

## Credits

Reports that lead to a fix get credited in the release notes
(unless you ask to stay anonymous). Thanks for keeping the project's
attack surface small.
