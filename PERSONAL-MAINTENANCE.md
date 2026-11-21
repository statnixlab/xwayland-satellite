# Personal xwayland-satellite branch

Official xwayland-satellite plus a short stack of personal patches for this
desktop (CachyOS, NVIDIA, niri at 200% scale).

## Remotes and branches

| Name | Meaning |
| --- | --- |
| `official` | https://github.com/Supreeeme/xwayland-satellite.git (upstream) |
| `machine/nvidia-desktop` | Personal patches; what the installed binaries are built from |
| `review/official-YYYYMMDD` | A rebase onto newer upstream, built and tested before adoption |

## Patch stack (oldest first)

1. **Local hidpi-minimize patch**: `XWS_CURSOR_SCALE` sharp HiDPI cursors, X11
   minimize (`WM_CHANGE_STATE/IconicState` -> `set_minimized`) and maximize
   compatibility, clipboard transfer fixes (nonblocking, bounded, periodic wake),
   QQ fixes. Originally `desktop-config/system-reference/xwayland-satellite/hidpi-minimize.patch`.

Keep each feature in its own commit when changing it; drop a patch once
upstream has an equivalent.

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
