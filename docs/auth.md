# Authentication

confed needs three things to talk to Confluence: a base URL, a secret, and — on Cloud —
the account e-mail that goes with the secret. This page covers where to get them, how
confed decides which value to use, where the secret is stored, and what that storage does
and does not protect you from.

## Getting a credential

### Confluence Cloud: an API token

1. Sign in and open <https://id.atlassian.com/manage-profile/security/api-tokens>.
2. **Create API token**, give it a label such as `confed on my laptop`, and copy it. The
   value is shown once.
3. The token is used as an HTTP Basic *password*; the *username* is the e-mail address of
   your Atlassian account. Both are required.

```bash
confed init --base-url https://acme.atlassian.net/wiki \
            --user you@example.com \
            --space DOCS
# prompts for the token, with the input hidden
```

The base URL must include the `/wiki` context path. `https://acme.atlassian.net` without
it will fail on the first API call.

### Confluence Data Center or Server: a Personal Access Token

1. Sign in, open your profile menu, and choose **Personal Access Tokens**.
2. **Create token**, name it, and set an expiry if your administrator has not fixed one.
3. Copy the value. It is a bearer token and authenticates on its own, so there is no
   username to supply.

```bash
confed init --base-url https://wiki.corp.example.com/confluence --space DOCS
```

The base URL is the Confluence context root — often *not* the host root. If your pages
live at `https://wiki.corp.example.com/confluence/display/DOCS/Home`, then
`https://wiki.corp.example.com/confluence` is the base URL.

If PATs are not available on your instance, pass `--user <username>` and supply the
password as the token. confed then uses HTTP Basic instead of a bearer header. This is the
only reason to pass `--user` on Data Center: doing so switches the authentication method
for the whole session.

### Which method confed chose

`confed init` records the method in `.session.db` and every later command reuses it.

| Server | `--user` given? | Method stored | HTTP header |
|---|---|---|---|
| Cloud | required | `api_token` | `Authorization: Basic base64(email:token)` |
| Data Center | no | `pat` | `Authorization: Bearer <token>` |
| Data Center | yes | `basic` | `Authorization: Basic base64(user:secret)` |

`confed whoami` prints who you are authenticated as and what the server supports;
`confed doctor` reports the method alongside the rest of its checks.

## Precedence

Every parameter — base URL, token, username, space, flavor, concurrency — resolves the
same way, and stops at the first place it is found:

1. **the command-line flag**, e.g. `--token`
2. **the environment variable**, e.g. `CONFED_TOKEN`
3. **stored configuration** written by `confed init` (`.state.db` for the base URL, space,
   and flavor; `.session.db` for the credential)
4. **for a workspace inside an instance root, with no `.session.db` of its own**, the
   root's shared credential in `.confed-instance-session.db` or its keyring entry
   (see [Sharing one credential](#sharing-one-credential-between-spaces))
5. **an interactive prompt** — only when stdin is a terminal *and* neither `--json` nor
   `--non-interactive` was passed

An empty string counts as absent, so `CONFED_SPACE= confed status` falls through to the
stored value rather than failing.

In step 5's absence — no TTY, `--json`, or `--non-interactive` — confed does not hang. It
exits **2** immediately with a message naming the missing value, its flag, and its
environment variable:

```text
error: missing API token
hint: pass --token, set CONFED_TOKEN, or run `confed init` in this directory (interactive prompts are disabled without a TTY)
```

| Value | Flag | Environment |
|---|---|---|
| Base URL | `--base-url` | `CONFED_BASE_URL` |
| Token or PAT | `--token` | `CONFED_TOKEN` |
| Username or e-mail | `--user` | `CONFED_USERNAME` |
| Space key | `--space` | `CONFED_SPACE` |
| Flavor (`cloud`, `dc`, `datacenter`) | `--flavor` | `CONFED_FLAVOR` |
| Request concurrency | `--concurrency` | `CONFED_CONCURRENCY` |
| Machine output | `--json` | `CONFED_JSON` |
| Never prompt | `--non-interactive` | `CONFED_NON_INTERACTIVE` |
| Tracing filter | `--log` | `CONFED_LOG` |

`confed config --list` prints every resolved value together with where it came from, with
secrets shown as `***`.

## Where the secret is stored

`confed init` verifies the credential with a `whoami` call *before* writing anything —
a bad token exits 3 and leaves the directory untouched — and then stores it in one of two
places.

**The OS keyring, by default.** macOS Keychain, Windows Credential Manager, or a
Secret Service implementation (GNOME Keyring, KWallet) on Linux. `.session.db` then holds
only the base URL, flavor, auth method, username, and a note saying the secret is in the
keyring.

**`.session.db`, as a fallback.** Headless Linux boxes, containers, and CI runners
usually have no Secret Service. confed notices, stores the token in the SQLite file
instead, and warns:

```text
warning: no OS keyring was available, so the token is stored in .session.db (mode 0600); prefer CONFED_TOKEN in CI
```

Force either backend with `confed init --credential-store keyring|sqlite`. Requesting
`keyring` when none is available is an error rather than a silent downgrade, which is what
you want in a hardened environment.

## Reusing a credential across workspaces

Each workspace keeps its own copy of the credential, so a second space on the same
instance would ask for the token again. `confed clone --from` copies it from a workspace
that already has it instead — the base URL, flavor, auth method and user from its
`.state.db` and `.session.db`, and the secret from wherever that workspace keeps it:

```bash
confed clone ENG --from ../DOCS             # one more space
confed clone --all ~/wiki --from ~/wiki/DOCS # every space you can see
```

- **Precedence is unchanged.** The source stands where stored configuration would, so
  `--token`, `--user`, `--base-url` and their `CONFED_*` variables still win; with an
  explicit token the source's secret is not read at all.
- **The token stays on its instance.** A `--base-url`, space URL or `--flavor` that
  disagrees with the source is exit 2, not a fallback.
- **Verified before anything is written.** A source token that has been revoked or has
  expired is exit 3, and no folder is created.
- **Stored the same way.** The new workspace keeps the secret in the source's backend
  unless `--credential-store` says otherwise. Keyring entries are per base URL and user,
  so two keyring workspaces on one instance share one entry.
- **The keyring may ask.** Reading a keyring entry can raise a password dialog. Without a
  terminal — `--json`, `--non-interactive`, or no TTY — nobody can answer it, so confed
  does not try: it exits 2 and suggests `--token`, or
  `confed -C ../DOCS config --no-keychain` to move the source's credential where it can be
  read without a prompt. At a terminal, a keyring that cannot be read falls back to asking
  for the token.

At a terminal, `clone` without `--from` also offers the credential of the one workspace
already in the target's parent folder (for the same base URL, when one was given). It
asks first, and with more than one candidate it offers none.

## Sharing one credential between spaces

An instance root (`confed instance`, see [commands](commands.md#confed-instance)) keeps
one credential for all of its workspaces, so a rotated token is stored once:

```bash
confed instance init ~/wiki --base-url https://wiki.example.test   # asks for the token once
confed instance adopt --move-credentials                           # or: share the existing ones
```

`adopt --move-credentials` takes the token from `--token`/`CONFED_TOKEN`, or from the
first workspace that stores one. It checks the token with `whoami`, and stores it in the
root: in the keyring, or `.confed-instance-session.db` (mode 0600) with
`--credential-store sqlite`. Only then does it remove each workspace's `.session.db`. A
workspace's keyring entry is left alone when the root's credential is that same entry
(same base URL and user).

A workspace that still has a `.session.db` keeps using it, alone and in `confed instance`
commands. Remove the file to make it use the root's. To rotate the shared token, run
`confed instance init <root> --base-url …` again: it replaces the root's credential and
leaves the spaces alone. `confed config --no-keychain` and `--force-keychain`, run in a
workspace without a credential of its own, move the root's.

## CI and automation

**Use `CONFED_TOKEN` and do not run `confed init` for the credential.** The environment
variable wins over anything stored, so a workspace that was initialized on someone's
laptop still works in CI under a different token. Nothing is written to disk that you then
have to remember to shred.

```yaml
env:
  CONFED_TOKEN: ${{ secrets.CONFLUENCE_TOKEN }}
  CONFED_USERNAME: ci-bot@example.com     # Cloud only
  CONFED_BASE_URL: https://acme.atlassian.net/wiki
  CONFED_SPACE: DOCS
  CONFED_JSON: "1"
```

With those set, a fresh checkout needs only `confed init --credential-store sqlite` (to
create `.state.db`) followed by `confed pull`; or, in a scratch directory,
`confed clone DOCS`. Because `CONFED_JSON=1` is set, every command is already
non-interactive.

Two habits worth keeping:

- Check exit codes, not output text. `confed status --exit-code` gives 10 when anything
  differs, and 4 when a page is conflicted, so a "docs are in sync" gate is one command.
- Run `confed push --dry-run --json` in pull requests and `confed push --json` only on the
  default branch. The dry run makes no mutating request at all.

## Security properties

**`.session.db` is created `0600` and confed refuses to open it if it is looser.** The
permissions are set when the file is created and reasserted on every save. If the mode is
widened afterwards, the next command fails with exit 7 and tells you to run
`chmod 600 .session.db`. (That error's hint also offers `confed doctor --fix`; it does not
actually work, because `doctor` opens the session file itself and hits the same error
first. Use `chmod`.) The mode protects against the file being read by another user on a
shared machine; it does not protect against root, against backups, or against you
committing it.

**Secrets are never logged or printed.** The token type wraps the value so it cannot be
formatted accidentally, `--token`'s environment value is hidden from `--help`,
`confed config --list` masks it as `***`, and no tracing level — including `-vvv` —
emits the credential. The end-to-end tests assert that the token appears in neither
stdout nor stderr of a successful `init`, nor of `clone --from` and `clone --all`,
succeeding or failing.

**`.state.db`, `.session.db` and `.confed.lock` are added to `.gitignore`** by `init`, and
`confed doctor` warns if any of them is missing (`--fix` re-adds them). `.state.db` is
excluded deliberately as well as `.session.db`: it contains full page bodies, is rebuilt
from the server by `init` + `fetch`, and would conflict on every single sync if committed.

**What confed cannot do for you.** It cannot scope a Cloud API token — Atlassian tokens
carry all of your permissions. Use a dedicated service account for automation, set an
expiry on Data Center PATs, and remember that anyone who can read the workspace directory
can read `.session.db` if the keyring fallback was used.

## Rotating or removing a credential

Re-running `confed init` in the same directory with the same space refreshes the stored
credential in place. Pointing an initialized directory at a *different* space is refused
with exit 7 unless you pass `--force`, so a typo cannot silently re-bind a workspace.

To verify a credential without changing anything, `confed whoami` (exit 3 if it is
rejected). To see everything at once — permissions, keyring availability, `.gitignore`
coverage, connectivity — run `confed doctor`.

## Choosing where the credential lives

confed prefers the OS keychain and falls back to `.session.db` when there is none. You can
move it either way at any time:

```bash
confed config --no-keychain      # keep it in .session.db; reading never prompts
confed config --force-keychain   # put it back in the OS keychain
```

`--no-keychain` is what you want when the keychain asks for a password every time confed
runs — on a headless box, in a container, or under a desktop keyring that re-locks. The
credential moves into `.session.db`, which is mode 0600 and git-ignored, and the keychain
entry is deleted so it never lives in two places.

The cost is that the token sits in that file in plain text, readable by anything running
as you. On a shared machine prefer `CONFED_TOKEN`, which confed reads without storing
anything.

Moving *to* the keychain reads the credential once, which may prompt; after that
`--no-keychain` never does. If the current store cannot be read at all, pass `--token` (or
set `CONFED_TOKEN`) and confed will use that value for the move.
