## Install

Download one file below, open it, done. No terminal needed.

| Your system | Download |
| --- | --- |
| Ubuntu, Linux Mint, Pop!_OS, Debian, Zorin | `rustpods_*_amd64.deb`: double-click to install |
| Fedora, openSUSE, Nobara | `rustpods-*.x86_64.rpm`: double-click to install |
| Anything else (Arch, CachyOS, Manjaro, Steam Deck, …) | `RustPods-*-x86_64.AppImage`: right-click → Properties → *Allow executing*, then double-click |

Then open **RustPods** from your app menu. The first time it starts, it sets up the background
service and turns on *Start at login*. That's it.

Arch users can also build the package from `packaging/arch/PKGBUILD`.

## What's new

- **New app icon**: two white earbuds on a rust-orange tile, in the app menu, the taskbar, the
  settings window and the AppImage.
- Everything from 0.2.0: the settings window, per-side shortcuts for every press, no terminal needed
  (the app starts its background service and *Start at login* is a switch), and packages for
  Debian/Ubuntu, Fedora/openSUSE, an AppImage and an Arch PKGBUILD.
