# Personal xwayland-satellite branch

Official xwayland-satellite plus a short stack of personal patches for this
desktop (CachyOS, NVIDIA, niri at 200% scale).

## Remotes and branches

| Name | Meaning |
| --- | --- |
| `official` | https://github.com/Supreeeme/xwayland-satellite.git (upstream) |
| `origin` | https://github.com/statnixlab/xwayland-satellite.git (personal fork; push here) |
| `machine/nvidia-desktop` | Personal patches; what the installed binaries are built from |
| `machine/nvidia-desktop-wechat` | `machine/nvidia-desktop` + WeChat-only commits |
| `machine/nvidia-desktop-wps` | `machine/nvidia-desktop` + WPS-only commits |
| `review/official-YYYYMMDD` | A rebase onto newer upstream, built and tested before adoption |

## Patch stack (oldest first, on top of `official/main`)

| Commit | Purpose |
| --- | --- |
| Log why the Xwayland Wayland client disconnects | Diagnostics: protocol error code/object in the log |
| Add sharp HiDPI cursors behind `XWS_CURSOR_SCALE` | Upstream issue #194 workaround; opt-in per instance |
| Forward X11 maximize requests and report the maximized state | `_NET_WM_STATE_MAXIMIZED_*` <-> xdg_toplevel maximized |
| Forward ICCCM `WM_CHANGE_STATE`/IconicState as minimize | X11 minimize buttons (Electron, Qt) -> `set_minimized` |
| Accept move/resize requests with an unspecified (0) button | QQ/Electron title bar dragging |
| Make clipboard transfers nonblocking and bounded | Stalled selection owners no longer freeze windows (QQ) |
| Follow the compositor when it ignores our X11 selection source | An ignored `set_selection` no longer blocks every later Wayland copy (WeChat paste stuck empty) |
| Document the personal branch and update workflow | This file |

Upstream's own fix for X11 events buffered during synchronous replies
(779151f) replaced the equivalent local change; the local one was dropped.
Each commit builds and passes the test suite on its own. Keep one feature per
commit; drop a commit once upstream has an equivalent.

## Application branches

Shared fixes go on `machine/nvidia-desktop`; a commit that only one application
needs goes on that application's branch, which is always rebased on the shared
branch.

| Branch | Extra commits |
| --- | --- |
| `machine/nvidia-desktop-wechat` | Acknowledge X11 minimize with ICCCM IconicState (Qt repeated minimize); file-only clipboard view for `XWS_FILE_CLIPBOARD_EXE` |
| `machine/nvidia-desktop-wps` | niri client-maximize-to-column adapter (`XWS_CLIENT_MAXIMIZE_TO_COLUMN=1`); WPS selection toolbar as a popup |

The pre-branch single patches are kept as tags `archive/{chat,wechat,wps}-patch-20261121`
(all on upstream 0.8.2).

## Installed binaries

| Binary | Branch | Display / service | Users |
| --- | --- | --- | --- |
| `~/.local/libexec/xwayland-satellite-hidpi` | `machine/nvidia-desktop` | `:2` xwayland-chat.service | QQ, AB Download Manager |
| `~/.local/libexec/xwayland-satellite-steam` | `machine/nvidia-desktop` | `:5` xwayland-steam.service | Steam |
| `~/.local/libexec/xwayland-satellite-wechat` | `machine/nvidia-desktop-wechat` | `:4` xwayland-wechat.service | WeChat |
| `~/.local/libexec/xwayland-satellite-wps` | `machine/nvidia-desktop-wps` | `:3` xwayland-wps.service | WPS |

## Updating to newer upstream

```sh
git fetch official
git switch -c review/official-$(date +%Y%m%d) machine/nvidia-desktop
git rebase official/main          # resolve conflicts inside the patch they belong to
cargo build --release --locked --features systemd --jobs 4
cargo test  --release --locked --features systemd --jobs 4
```

Then install under a new name or after backing up the old binary, and restart
only the affected service. Restarting `xwayland-chat.service` disconnects QQ:
quit it first. When satisfied, move `machine/nvidia-desktop` to the review branch
(`git branch -f machine/nvidia-desktop review/...`), keeping the old tip as a tag
`archive/nvidia-desktop-YYYYMMDD`.
Then rebase each application branch onto the new shared branch:

```sh
git rebase --onto machine/nvidia-desktop <old shared tip> machine/nvidia-desktop-wechat
git rebase --onto machine/nvidia-desktop <old shared tip> machine/nvidia-desktop-wps
```

and build/test each before installing its binary.
Push the archive tag first, then the branch with `--force-with-lease`, then
update `desktop-config/system-reference/xwayland-satellite/source.json`.
