# Personal xwayland-satellite branch

Official xwayland-satellite plus a short stack of personal patches for this
desktop (CachyOS, NVIDIA, niri at 200% scale).

## Remotes and branches

| Name | Meaning |
| --- | --- |
| `official` | https://github.com/Supreeeme/xwayland-satellite.git (upstream) |
| `origin` | https://github.com/statnixlab/xwayland-satellite.git (personal fork; push here) |
| `machine/nvidia-desktop` | Personal patches; what the installed binaries are built from |
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
| Document the personal branch and update workflow | This file |

Upstream's own fix for X11 events buffered during synchronous replies
(779151f) replaced the equivalent local change; the local one was dropped.
Each commit builds and passes the test suite on its own. Keep one feature per
commit; drop a commit once upstream has an equivalent.

## Installed binaries

| Binary | Display / service | Users |
| --- | --- | --- |
| `~/.local/libexec/xwayland-satellite-hidpi` | `:2` xwayland-chat.service | QQ, AB Download Manager |
| `~/.local/libexec/xwayland-satellite-steam` | `:5` xwayland-steam.service | Steam |
| `~/.local/libexec/xwayland-satellite-wechat` / `-wps` | `:4` / `:3` | separate patches in desktop-config |

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
Push the archive tag first, then the branch with `--force-with-lease`, then
update `desktop-config/system-reference/xwayland-satellite/source.json`.
