# delocal

Keep a folder identical across all of your machines, over your tailnet.

> **Sync is not backup.**
> delocal copies your changes to your other machines, faithfully. If you edit a file
> badly or delete it on the machine you are sitting at, that edit or deletion spreads.
> The local trash only holds copies displaced by *other* machines, and it is pruned.
> Keep a real backup of anything you cannot lose.

> **Status: pre-release.** Nothing below works yet. This README describes v1 as
> designed in [DESIGN.md](DESIGN.md).

## What it is

delocal runs over [Tailscale](https://tailscale.com), so it has no accounts, no device
IDs, no ports to open and no config file. You install it with one command, run
`delocal up`, and your machines find each other. It is a single static binary with a
background service and a small CLI.

It is built to never lose a file. Every remote change that overwrites or deletes
something goes through a local trash first, and any change large enough to be a mistake
(a mass delete, a script gone wrong, an unmounted disk) is held for your approval
before it spreads.

Linux (x86_64, aarch64) and macOS (Apple silicon, Intel). Requires Tailscale installed,
logged in and running on every machine.

## Install

```sh
curl -fsSL https://delocal.sh/install | sh
```

**Not yet live.** The installer and releases arrive with v1. Until then, build from
source with `cargo build --release`.

## The five commands

| Command | What it does |
|---|---|
| `delocal up` | Start delocal. On first run it checks Tailscale, installs the background service, and offers to sync the folders your other machines already share. |
| `delocal status` | Your machines, how each is reached, your folders, and anything pending, held or paused. The one command most people ever run. |
| `delocal share <path>` | Turn a directory into a synced folder and offer it to all of your machines, at the same path relative to your home directory. |
| `delocal review` | Show changes that were held because they were large or delete-heavy, with what to approve or deny. |
| `delocal restore <path>` | Bring a file back from the trash to where it was. It then syncs out like any other change. |

Every command has `--help`. There are more (`history`, `trash`, `revert`, `rules`,
`doctor`, `update`) but you should not need them until something has gone wrong.

## Licence

MIT OR Apache-2.0, at your option. See [LICENSE-MIT](LICENSE-MIT) and
[LICENSE-APACHE](LICENSE-APACHE).
